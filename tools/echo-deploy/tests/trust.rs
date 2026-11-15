//! Offline Ed25519 trust tests. All signing keys are generated in the test
//! temporary directory; no key material is checked in.

use echo_deploy::trust::{self, TrustStore};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::{tempdir, TempDir};

const KEY_ID: &str = "release-1";

fn run_openssl(args: &[&str]) -> Vec<u8> {
    let output = Command::new(trust::OPENSSL)
        .args(args)
        .output()
        .expect("run openssl");
    assert!(
        output.status.success(),
        "openssl {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

struct Fixture {
    temp: TempDir,
    trust: PathBuf,
    private_key: PathBuf,
    public_pem: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempdir().unwrap();
        let trust = temp.path().join("trust");
        fs::create_dir(&trust).unwrap();
        fs::set_permissions(&trust, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = trust.join("trust-format");
        fs::write(&marker, trust::TRUST_FORMAT).unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();

        let private_key = temp.path().join("release-signing.pem");
        run_openssl(&[
            "genpkey",
            "-algorithm",
            "Ed25519",
            "-out",
            private_key.to_str().unwrap(),
        ]);
        let public_key = trust.join(format!("{KEY_ID}.ed25519.pem"));
        run_openssl(&[
            "pkey",
            "-in",
            private_key.to_str().unwrap(),
            "-pubout",
            "-out",
            public_key.to_str().unwrap(),
        ]);
        fs::set_permissions(&public_key, fs::Permissions::from_mode(0o600)).unwrap();
        let public_pem = fs::read(&public_key).unwrap();
        Self {
            temp,
            trust,
            private_key,
            public_pem,
        }
    }

    fn sign(&self, manifest: &[u8]) -> Vec<u8> {
        let input = self.temp.path().join("manifest-to-sign.bin");
        let signature = self.temp.path().join("manifest-to-sign.sig");
        fs::write(&input, manifest).unwrap();
        run_openssl(&[
            "pkeyutl",
            "-sign",
            "-inkey",
            self.private_key.to_str().unwrap(),
            "-rawin",
            "-in",
            input.to_str().unwrap(),
            "-out",
            signature.to_str().unwrap(),
        ]);
        let signature = fs::read(&signature).unwrap();
        assert_eq!(signature.len(), trust::ED25519_SIGNATURE_BYTES);
        signature
    }

    fn signature_file(&self, name: &str, signature: &[u8]) -> PathBuf {
        let path = self.temp.path().join(name);
        fs::write(&path, signature).unwrap();
        path
    }

    fn store(&self) -> TrustStore {
        TrustStore::open(&self.trust).unwrap()
    }
}

#[test]
fn verifies_detached_signature_and_returns_frozen_bytes() {
    let fixture = Fixture::new();
    let manifest = br#"{"schema_version":1,"release_id":"dev-1"}"#;
    let signature = fixture.sign(manifest);
    let signature_path = fixture.signature_file("good.sig", &signature);
    let store = fixture.store();

    let verified = store
        .verify_manifest(KEY_ID, manifest, &signature_path)
        .unwrap();
    assert_eq!(verified.key_id(), KEY_ID);
    assert_eq!(verified.bytes(), &manifest[..]);
    assert_eq!(verified.sha256(), format!("{:x}", Sha256::digest(manifest)));
    assert_eq!(verified.key_fingerprint().len(), 64);
    assert!(verified
        .key_fingerprint()
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit()));

    // The low-level API accepts the same fingerprint-validated PEM bytes.
    trust::verify_bytes(&fixture.public_pem, manifest, &signature).unwrap();
    fixture.store().key(KEY_ID).unwrap();
}

#[test]
fn rejects_tampered_manifest_signature_and_foreign_key() {
    let fixture = Fixture::new();
    let manifest = b"frozen-manifest-v1";
    let signature = fixture.sign(manifest);
    let good = fixture.signature_file("good.sig", &signature);
    let store = fixture.store();

    let mut tampered = manifest.to_vec();
    tampered[0] ^= 1;
    assert!(store.verify_manifest(KEY_ID, &tampered, &good).is_err());

    let mut bad_signature = signature.clone();
    bad_signature[0] ^= 1;
    let bad = fixture.signature_file("bad.sig", &bad_signature);
    assert!(store.verify_manifest(KEY_ID, manifest, &bad).is_err());
    assert!(trust::verify_bytes(&fixture.public_pem, manifest, &bad_signature).is_err());
    assert!(trust::verify_bytes(&fixture.public_pem, manifest, &[0_u8; 64]).is_err());

    let foreign = Fixture::new();
    assert!(foreign
        .store()
        .verify_manifest(KEY_ID, manifest, &good)
        .is_err());
}

#[test]
fn trust_directory_must_be_explicit_private_and_marked() {
    let temp = tempdir().unwrap();
    let base = temp.path();
    let trust = base.join("trust");
    fs::create_dir(&trust).unwrap();
    fs::set_permissions(&trust, fs::Permissions::from_mode(0o700)).unwrap();

    // Explicit absolute path is required and the marker is mandatory.
    assert!(TrustStore::open(&trust).is_err());
    assert!(TrustStore::open(Path::new("relative/trust")).is_err());
    assert!(TrustStore::open(base).is_err());

    let marker = trust.join("trust-format");
    fs::write(&marker, b"wrong-format\n").unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(TrustStore::open(&trust).is_err());
    fs::write(&marker, trust::TRUST_FORMAT).unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(TrustStore::open(&trust).is_ok());

    // Group/other writable marker is rejected even inside a private directory.
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(TrustStore::open(&trust).is_err());
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();

    // Trailing bytes after the marker are rejected.
    let mut extended = trust::TRUST_FORMAT.to_vec();
    extended.extend_from_slice(b"extra\n");
    fs::write(&marker, &extended).unwrap();
    assert!(TrustStore::open(&trust).is_err());
    fs::write(&marker, trust::TRUST_FORMAT).unwrap();

    // Not private.
    let open = base.join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(open.join("trust-format"), trust::TRUST_FORMAT).unwrap();
    assert!(TrustStore::open(&open).is_err());

    // Symlinked trust directory components are refused.
    let link = base.join("trust-link");
    symlink(&trust, &link).unwrap();
    assert!(TrustStore::open(&link).is_err());
}

#[test]
fn trusted_keys_are_strict_ed25519_spki_files() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let key_path = fixture.trust.join(format!("{KEY_ID}.ed25519.pem"));

    assert!(store.key("missing").is_err());
    assert!(store.key("../escape").is_err());
    assert!(store.key("release-1/child").is_err());

    // Symlinked key files are refused by descriptor-relative open.
    let real = fixture.temp.path().join("real-key.pem");
    fs::rename(&key_path, &real).unwrap();
    symlink(&real, &key_path).unwrap();
    assert!(store.key(KEY_ID).is_err());
    fs::remove_file(&key_path).unwrap();
    fs::copy(&real, &key_path).unwrap();

    // Hard-linked key files are refused (single link required).
    let hard = fixture.temp.path().join("hard-key.pem");
    fs::hard_link(&key_path, &hard).unwrap();
    assert!(store.key(KEY_ID).is_err());
    fs::remove_file(&hard).unwrap();

    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(store.key(KEY_ID).is_err());
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(store.key(KEY_ID).is_ok());

    // A private key PEM is not a public key.
    fs::write(&key_path, fs::read(&fixture.private_key).unwrap()).unwrap();
    assert!(store.key(KEY_ID).is_err());

    // RSA is structurally rejected before OpenSSL is invoked.
    let rsa = fixture.temp.path().join("rsa.pem");
    run_openssl(&[
        "genpkey",
        "-algorithm",
        "RSA",
        "-pkeyopt",
        "rsa_keygen_bits:2048",
        "-out",
        rsa.to_str().unwrap(),
    ]);
    run_openssl(&[
        "pkey",
        "-in",
        rsa.to_str().unwrap(),
        "-pubout",
        "-out",
        key_path.to_str().unwrap(),
    ]);
    assert!(store.key(KEY_ID).is_err());

    // More than one PEM block, and trailing garbage, are rejected.
    let mut doubled = fixture.public_pem.clone();
    doubled.extend_from_slice(&fixture.public_pem);
    fs::write(&key_path, &doubled).unwrap();
    assert!(store.key(KEY_ID).is_err());
    let mut garbage = fixture.public_pem.clone();
    garbage.extend_from_slice(b"garbage\n");
    fs::write(&key_path, &garbage).unwrap();
    assert!(store.key(KEY_ID).is_err());

    // Oversized "key" is rejected before parsing.
    fs::write(&key_path, vec![b'A'; trust::MAX_PUBLIC_KEY_BYTES + 1]).unwrap();
    assert!(store.key(KEY_ID).is_err());
}

#[test]
fn signature_must_be_a_single_regular_64_byte_file() {
    let fixture = Fixture::new();
    let manifest = b"signature-file-contract";
    let signature = fixture.sign(manifest);
    let store = fixture.store();
    let good = fixture.signature_file("good.sig", &signature);
    assert!(store.verify_manifest(KEY_ID, manifest, &good).is_ok());

    assert!(store
        .verify_manifest(KEY_ID, manifest, Path::new("relative.sig"))
        .is_err());
    for length in [0_usize, 1, 63, 65, 4096] {
        let path = fixture.signature_file("wrong-length.sig", &vec![0_u8; length]);
        assert!(
            store.verify_manifest(KEY_ID, manifest, &path).is_err(),
            "accepted {length}-byte signature"
        );
    }

    let link = fixture.temp.path().join("signature-link");
    symlink(&good, &link).unwrap();
    assert!(store.verify_manifest(KEY_ID, manifest, &link).is_err());

    let hard = fixture.temp.path().join("signature-hard");
    fs::hard_link(&good, &hard).unwrap();
    assert!(store.verify_manifest(KEY_ID, manifest, &hard).is_err());

    let directory = fixture.temp.path().join("signature-directory");
    fs::create_dir(&directory).unwrap();
    assert!(store.verify_manifest(KEY_ID, manifest, &directory).is_err());
}

#[test]
fn verified_manifest_owns_the_exact_verified_bytes() {
    let fixture = Fixture::new();
    let mut manifest = b"owned-manifest-bytes".to_vec();
    let signature = fixture.sign(&manifest);
    let signature_path = fixture.signature_file("good.sig", &signature);
    let expected = manifest.clone();
    let verified = fixture
        .store()
        .verify_manifest(KEY_ID, &manifest, &signature_path)
        .unwrap();

    // Mutating the caller's buffer after verification cannot change the result.
    manifest[0] ^= 0xff;
    assert_eq!(verified.bytes(), expected.as_slice());
    assert_eq!(
        verified.sha256(),
        format!("{:x}", Sha256::digest(&expected))
    );
    assert_eq!(verified.into_bytes(), expected);
}

#[test]
fn rejects_malformed_or_oversized_inputs() {
    let fixture = Fixture::new();
    let manifest = b"malformed-inputs";
    let signature = fixture.sign(manifest);

    for bad_key in [
        b"".as_slice(),
        b"-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n",
        b"-----BEGIN FOO-----\nAAAA\n-----END FOO-----\n",
        b"not a pem at all",
    ] {
        assert!(trust::verify_bytes(bad_key, manifest, &signature).is_err());
    }
    assert!(trust::verify_bytes(&fixture.public_pem, b"", &signature).is_err());
    assert!(trust::verify_bytes(&fixture.public_pem, manifest, &[]).is_err());
    assert!(trust::verify_bytes(&fixture.public_pem, manifest, &[0_u8; 63]).is_err());
    assert!(trust::verify_bytes(&fixture.public_pem, manifest, &[0_u8; 65]).is_err());
    assert!(trust::verify_bytes(
        &fixture.public_pem,
        &vec![0_u8; trust::MAX_MANIFEST_BYTES + 1],
        &signature
    )
    .is_err());
}

#[test]
fn openssl_is_pinned_to_an_absolute_non_writable_path() {
    assert!(trust::OPENSSL.starts_with('/'));
    let meta = fs::symlink_metadata(trust::OPENSSL).unwrap();
    assert!(meta.file_type().is_file());
    assert_eq!(meta.permissions().mode() & 0o022, 0);
}
