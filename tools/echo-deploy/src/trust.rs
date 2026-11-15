//! Offline Ed25519 verification of a frozen release manifest.
//!
//! Trust is anchored in an explicitly supplied trust directory: there is no
//! default location, no environment lookup and no key discovery. The Ed25519
//! operation itself is delegated to the system OpenSSL CLI at a fixed absolute
//! path. Every input is copied into a sealed anonymous `memfd` and passed as
//! `/proc/self/fd/N`, so the exact bytes that were verified cannot be rewritten
//! afterwards (seals also prevent shrink/grow/write through any descriptor).
//!
//! Public key material is never logged; only key identifiers and SHA-256 key
//! fingerprints are exposed to callers.

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

use crate::{model, safe_fs::Dir};

/// Fixed OpenSSL CLI path. It is never resolved through `PATH` or the environment.
pub const OPENSSL: &str = "/usr/bin/openssl";
/// Marker file content that identifies an explicitly provisioned trust directory.
pub const TRUST_FORMAT: &[u8] = b"echo-deploy-trust-v1\n";
/// Matches the 4 MiB limit applied to `release.json` by the bundle reader.
pub const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
/// PEM text is tiny; anything larger is not a sane single public key.
pub const MAX_PUBLIC_KEY_BYTES: usize = 16 * 1024;
/// Raw detached Ed25519 signature size (RFC 8032).
pub const ED25519_SIGNATURE_BYTES: usize = 64;
/// Hard upper bound for the external verifier; it does no network or disk I/O.
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// A verified manifest. It owns the exact bytes that OpenSSL verified, so later
/// rewrites of a file or caller buffer cannot change what downstream code reads.
pub struct VerifiedManifest {
    key_id: String,
    key_fingerprint: String,
    manifest: Vec<u8>,
    sha256: String,
}

impl VerifiedManifest {
    fn new(key_id: String, key_fingerprint: String, manifest: &[u8]) -> Self {
        Self {
            key_id,
            key_fingerprint,
            manifest: manifest.to_vec(),
            sha256: format!("{:x}", Sha256::digest(manifest)),
        }
    }
    /// Trusted key identifier from the explicit trust directory.
    pub fn key_id(&self) -> &str {
        &self.key_id
    }
    /// Lowercase SHA-256 of the Ed25519 SPKI DER (safe to log; not key material).
    pub fn key_fingerprint(&self) -> &str {
        &self.key_fingerprint
    }
    /// The exact bytes covered by the signature.
    pub fn bytes(&self) -> &[u8] {
        &self.manifest
    }
    /// Lowercase SHA-256 of [`Self::bytes`].
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    /// Consume the verified copy, e.g. to freeze it into a candidate.
    pub fn into_bytes(self) -> Vec<u8> {
        self.manifest
    }
}

/// One trusted Ed25519 key loaded from a trust directory.
pub struct TrustedKey {
    id: String,
    pem: Vec<u8>,
    fingerprint: String,
}

impl TrustedKey {
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Lowercase SHA-256 of the Ed25519 SPKI DER.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    /// Verify detached raw signature bytes over exactly `manifest`.
    pub fn verify(&self, manifest: &[u8], signature: &[u8]) -> Result<VerifiedManifest> {
        verify_bytes(&self.pem, manifest, signature)?;
        Ok(VerifiedManifest::new(
            self.id.clone(),
            self.fingerprint.clone(),
            manifest,
        ))
    }
    /// Read a detached signature from an absolute path and verify it.
    pub fn verify_file(&self, manifest: &[u8], signature: &Path) -> Result<VerifiedManifest> {
        let signature = read_signature(signature)?;
        self.verify(manifest, &signature)
    }
}

/// An explicitly provisioned trust directory:
///
/// ```text
/// <absolute trust directory>/          0700, owned by the caller
/// <absolute trust directory>/trust-format      "echo-deploy-trust-v1\n"
/// <absolute trust directory>/<key-id>.ed25519.pem
/// ```
///
/// There is deliberately no constructor that guesses a location or scans keys.
pub struct TrustStore {
    dir: Dir,
    path: PathBuf,
}

impl TrustStore {
    /// Open an existing, private, marker-bearing trust directory. The path must
    /// be absolute; symlinked components, group/other access and missing markers
    /// all fail closed.
    pub fn open(path: &Path) -> Result<Self> {
        model::absolute_path(path)?;
        let dir = Dir::absolute(path).context("open trust directory")?;
        let meta = dir.0.metadata()?;
        ensure!(meta.is_dir(), "trust path is not a directory");
        // SAFETY: geteuid has no preconditions and cannot fail.
        ensure!(
            meta.uid() == unsafe { libc::geteuid() },
            "trust directory must be owned by the caller"
        );
        ensure!(
            meta.mode() & 0o077 == 0,
            "trust directory must be private (0700)"
        );
        let mut marker_file = dir
            .open_file("trust-format", false)
            .context("open trust-format marker")?;
        ensure!(
            marker_file.metadata()?.mode() & 0o022 == 0,
            "trust-format marker must not be group/other writable"
        );
        let mut marker = Vec::with_capacity(TRUST_FORMAT.len() + 1);
        let mut limited = (&mut marker_file).take(64);
        limited.read_to_end(&mut marker)?;
        ensure!(marker == TRUST_FORMAT, "not an echo-deploy trust directory");
        Ok(Self {
            dir,
            path: path.to_path_buf(),
        })
    }

    /// The explicit trust directory this store was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load `<id>.ed25519.pem` and require a canonical Ed25519 SPKI public key.
    pub fn key(&self, id: &str) -> Result<TrustedKey> {
        model::safe_id(id)?;
        let name = format!("{id}.ed25519.pem");
        let mut file = self
            .dir
            .open_file(&name, false)
            .with_context(|| format!("open trusted key {id}"))?;
        let meta = file.metadata()?;
        ensure!(
            meta.len() <= MAX_PUBLIC_KEY_BYTES as u64,
            "trusted key {id} is too large"
        );
        ensure!(
            meta.mode() & 0o022 == 0,
            "trusted key {id} must not be group/other writable"
        );
        let mut pem = Vec::with_capacity(meta.len() as usize);
        let mut limited = (&mut file).take(MAX_PUBLIC_KEY_BYTES as u64 + 1);
        limited.read_to_end(&mut pem)?;
        ensure!(
            pem.len() <= MAX_PUBLIC_KEY_BYTES,
            "trusted key {id} is too large"
        );
        let der = ed25519_spki(&pem).with_context(|| format!("trusted key {id}"))?;
        Ok(TrustedKey {
            id: id.to_string(),
            pem,
            fingerprint: format!("{:x}", Sha256::digest(&der)),
        })
    }

    /// Verify a detached raw signature over exactly `manifest` using one named
    /// trusted key. The signature is read from an absolute regular file.
    pub fn verify_manifest(
        &self,
        key_id: &str,
        manifest: &[u8],
        signature_file: &Path,
    ) -> Result<VerifiedManifest> {
        self.key(key_id)?.verify_file(manifest, signature_file)
    }
}

/// Low-level verification of raw bytes with caller-supplied public key PEM and
/// raw 64-byte signature. The caller is responsible for establishing trust in
/// the key (normally by using [`TrustStore`] instead of this function).
pub fn verify_bytes(public_key_pem: &[u8], manifest: &[u8], signature: &[u8]) -> Result<()> {
    ensure!(!manifest.is_empty(), "manifest is empty");
    ensure!(
        manifest.len() <= MAX_MANIFEST_BYTES,
        "manifest exceeds 4 MiB"
    );
    ensure!(
        signature.len() == ED25519_SIGNATURE_BYTES,
        "Ed25519 signature must be exactly 64 bytes"
    );
    ed25519_spki(public_key_pem).context("trusted public key")?;
    ensure_proc_fd()?;
    let binary = openssl_binary()?;

    let key = Frozen::new("echo-deploy-public-key", public_key_pem)?;
    let manifest = Frozen::new("echo-deploy-manifest", manifest)?;
    let signature = Frozen::new("echo-deploy-signature", signature)?;

    let mut command = Command::new(binary);
    command
        .arg("pkeyutl")
        .arg("-verify")
        .arg("-pubin")
        .arg("-inkey")
        .arg(&key.path)
        .arg("-rawin")
        .arg("-in")
        .arg(&manifest.path)
        .arg("-sigfile")
        .arg(&signature.path)
        .arg("-config")
        .arg("/dev/null")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear();
    let status = run_with_timeout(&mut command, VERIFY_TIMEOUT)?;
    ensure!(
        status.success(),
        "Ed25519 signature verification failed ({status})"
    );
    Ok(())
}

/// Read a detached signature: absolute path, no symlinks, exactly one link,
/// exactly 64 bytes. Failure to match any of these is a verification failure.
fn read_signature(path: &Path) -> Result<Vec<u8>> {
    model::absolute_path(path)?;
    let parent = Dir::absolute(path.parent().context("signature has no parent")?)
        .context("open signature directory")?;
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("signature filename must be UTF-8")?;
    let mut file = parent
        .open_file(leaf, false)
        .with_context(|| format!("open signature {}", path.display()))?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "signature is not a regular file");
    ensure!(meta.nlink() == 1, "signature must have exactly one link");
    ensure!(
        meta.len() == ED25519_SIGNATURE_BYTES as u64,
        "Ed25519 signature must be exactly 64 bytes"
    );
    let mut signature = Vec::with_capacity(ED25519_SIGNATURE_BYTES);
    let mut limited = (&mut file).take(ED25519_SIGNATURE_BYTES as u64 + 1);
    limited.read_to_end(&mut signature)?;
    ensure!(
        signature.len() == ED25519_SIGNATURE_BYTES,
        "Ed25519 signature must be exactly 64 bytes"
    );
    Ok(signature)
}

/// Parse exactly one canonical Ed25519 SubjectPublicKeyInfo PEM block and
/// return the 44-byte DER. Rejects private keys, other algorithms, extra PEM
/// blocks, non-canonical base64 and the all-zero public key.
fn ed25519_spki(pem: &[u8]) -> Result<Vec<u8>> {
    ensure!(!pem.is_empty(), "empty public key");
    ensure!(
        pem.len() <= MAX_PUBLIC_KEY_BYTES,
        "public key exceeds 16 KiB"
    );
    ensure!(pem.is_ascii(), "public key must be ASCII PEM");
    let text = std::str::from_utf8(pem).context("public key must be UTF-8")?;
    let mut body = String::new();
    let mut in_body = false;
    let mut finished = false;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if finished {
            bail!("trailing data after PEM public key");
        }
        if !in_body {
            ensure!(
                line == "-----BEGIN PUBLIC KEY-----",
                "expected a PEM public key header"
            );
            in_body = true;
        } else if line == "-----END PUBLIC KEY-----" {
            in_body = false;
            finished = true;
        } else {
            ensure!(
                line.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=')),
                "invalid base64 in PEM public key"
            );
            body.push_str(line);
        }
    }
    ensure!(finished, "incomplete PEM public key");
    ensure!(!body.is_empty(), "empty PEM public key body");
    let der = base64_decode(body.as_bytes())?;
    ensure!(
        der.len() == 44 && der[..12] == ED25519_SPKI_PREFIX[..],
        "trusted key is not an Ed25519 SPKI public key"
    );
    ensure!(
        der[12..].iter().any(|&byte| byte != 0),
        "all-zero Ed25519 public key"
    );
    Ok(der)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Strict, allocation-bounded base64 decoder for PEM bodies (whitespace is
/// rejected; PEM line splitting must have removed it already).
fn base64_decode(input: &[u8]) -> Result<Vec<u8>> {
    ensure!(!input.is_empty(), "empty base64 body");
    ensure!(input.len().is_multiple_of(4), "invalid base64 length");
    let (chunks, remainder) = input.as_chunks::<4>();
    ensure!(remainder.is_empty(), "invalid base64 length");
    let blocks = chunks.len();
    let mut output = Vec::with_capacity(blocks * 3);
    for (index, chunk) in chunks.iter().enumerate() {
        let last = index + 1 == blocks;
        let padding = chunk.iter().rev().take_while(|&&byte| byte == b'=').count();
        ensure!(padding <= 2, "invalid base64 padding");
        ensure!(last || padding == 0, "base64 padding before final block");
        ensure!(
            chunk[..4 - padding].iter().all(|&byte| byte != b'='),
            "invalid base64 padding position"
        );
        let v0 = base64_value(chunk[0]).context("invalid base64 byte")? as u32;
        let v1 = base64_value(chunk[1]).context("invalid base64 byte")? as u32;
        let v2 = if padding == 2 {
            0
        } else {
            base64_value(chunk[2]).context("invalid base64 byte")? as u32
        };
        let v3 = if padding >= 1 {
            0
        } else {
            base64_value(chunk[3]).context("invalid base64 byte")? as u32
        };
        let triple = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
        output.push((triple >> 16) as u8);
        match padding {
            0 => {
                output.push((triple >> 8) as u8);
                output.push(triple as u8);
            }
            1 => {
                ensure!(v2 & 0x03 == 0, "non-canonical base64 padding");
                output.push((triple >> 8) as u8);
            }
            2 => ensure!(v1 & 0x0f == 0, "non-canonical base64 padding"),
            _ => unreachable!("padding checked above"),
        }
    }
    Ok(output)
}

fn ensure_proc_fd() -> Result<()> {
    let meta = std::fs::symlink_metadata("/proc/self/fd")
        .context("/proc/self/fd is required to pass sealed inputs to openssl")?;
    ensure!(meta.is_dir(), "/proc/self/fd is not a directory");
    Ok(())
}

/// Validate the fixed OpenSSL path: regular file, no symlink, not writable by
/// the caller, not group/other writable, with non-writable ancestors. Ownership
/// itself is not asserted because confined sandboxes commonly map system files
/// to an unstripped overflow uid; write access and path components are checked.
fn openssl_binary() -> Result<&'static Path> {
    let path = Path::new(OPENSSL);
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("inspect {OPENSSL}"))?;
    ensure!(
        meta.file_type().is_file(),
        "{OPENSSL} must be a regular file"
    );
    let mode = meta.permissions().mode() & 0o7777;
    ensure!(
        mode & 0o022 == 0,
        "{OPENSSL} must not be group/other writable"
    );
    ensure!(mode & 0o111 != 0, "{OPENSSL} is not executable");
    let mut ancestor = path.parent();
    while let Some(directory) = ancestor {
        let meta = std::fs::symlink_metadata(directory)
            .with_context(|| format!("inspect {}", directory.display()))?;
        ensure!(meta.is_dir(), "{} is not a directory", directory.display());
        ensure!(
            meta.permissions().mode() & 0o022 == 0,
            "{} is group/other writable",
            directory.display()
        );
        ancestor = directory.parent();
    }
    let name = CString::new(OPENSSL)?;
    // SAFETY: `name` is a valid NUL-terminated path and faccessat only inspects it.
    ensure!(
        unsafe { libc::faccessat(libc::AT_FDCWD, name.as_ptr(), libc::W_OK, libc::AT_EACCESS,) }
            != 0,
        "{OPENSSL} must not be writable by the caller"
    );
    // SAFETY: same fixed C string; only the execute bit is queried.
    ensure!(
        unsafe { libc::faccessat(libc::AT_FDCWD, name.as_ptr(), libc::X_OK, libc::AT_EACCESS,) }
            == 0,
        "{OPENSSL} is not executable"
    );
    Ok(path)
}

/// An immutable anonymous file. `F_SEAL_WRITE` also blocks writes through any
/// duplicate descriptor, and `F_SEAL_SEAL` prevents removing the seals.
struct Frozen {
    // Held only to keep the sealed memfd alive while OpenSSL opens its path.
    #[allow(dead_code)]
    file: File,
    path: PathBuf,
}

impl Frozen {
    fn new(name: &str, bytes: &[u8]) -> Result<Self> {
        let name = CString::new(name)?;
        // SAFETY: valid NUL-terminated name; MFD_ALLOW_SEALING is a valid flag.
        let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_ALLOW_SEALING) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error())
                .context("create anonymous verification buffer");
        }
        let fd = move_above_stdio(raw)?;
        // SAFETY: `fd` is a freshly created descriptor now owned by `file`.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(bytes)
            .context("write frozen verification input")?;
        let seals =
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        // SAFETY: `file` owns the memfd and it was created with MFD_ALLOW_SEALING.
        ensure!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } == 0,
            "cannot seal verification input: {}",
            std::io::Error::last_os_error()
        );
        file.seek(SeekFrom::Start(0))?;
        let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        Ok(Self { file, path })
    }
}

/// Keep inherited descriptors away from 0/1/2 so `Stdio::null()` cannot replace
/// a frozen buffer in the child.
fn move_above_stdio(fd: i32) -> Result<i32> {
    if fd >= 3 {
        return Ok(fd);
    }
    // SAFETY: `fd` is a fresh owned descriptor; F_DUPFD creates an independent one.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD, 3) };
    let error = std::io::Error::last_os_error();
    // SAFETY: closing the original descriptor; a successful duplicate is independent.
    unsafe { libc::close(fd) };
    if duplicate < 0 {
        return Err(error).context("move verification buffer above stdio");
    }
    Ok(duplicate)
}

/// Poll a child with a hard deadline, then kill and reap it with a short grace
/// period. No stdout/stderr pipes are involved, so there is no output to leak.
fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<ExitStatus> {
    let mut child = command
        .spawn()
        .context("spawn openssl signature verification")?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().context("poll openssl")? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let grace = Instant::now() + Duration::from_millis(500);
            while Instant::now() < grace {
                if child.try_wait().context("reap openssl")?.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            bail!("openssl signature verification timed out after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod internal_tests {
    use super::*;

    #[test]
    fn base64_decoder_is_strict() {
        assert_eq!(base64_decode(b"TWFu").unwrap(), b"Man");
        assert_eq!(base64_decode(b"TWE=").unwrap(), b"Ma");
        assert_eq!(base64_decode(b"TQ==").unwrap(), b"M");
        assert!(base64_decode(b"TQ=").is_err());
        assert!(base64_decode(b"T===").is_err());
        assert!(base64_decode(b"TQ==TQ==").is_err());
        assert!(base64_decode(b"TR==").is_err());
        assert!(base64_decode(b"TQ==AAAA").is_err());
    }

    #[test]
    fn malformed_pem_is_rejected() {
        for pem in [
            b"".as_slice(),
            b"-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n",
            b"-----BEGIN FOO-----\nAAAA\n-----END FOO-----\n",
            b"-----BEGIN PUBLIC KEY-----\nnot base64!\n-----END PUBLIC KEY-----\n",
            b"-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\ntrailing\n",
        ] {
            assert!(ed25519_spki(pem).is_err());
        }
    }

    #[test]
    fn frozen_input_rejects_later_writes() {
        let frozen = Frozen::new("test", b"payload").unwrap();
        let mut duplicate = frozen.file.try_clone().unwrap();
        assert!(duplicate.write_all(b"x").is_err());
        assert!(duplicate.set_len(0).is_err());
        duplicate.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = Vec::new();
        duplicate.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"payload");
    }

    #[test]
    fn timeout_kills_a_hung_child() {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let started = Instant::now();
        assert!(run_with_timeout(&mut command, Duration::from_millis(50)).is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
