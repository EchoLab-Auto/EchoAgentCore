//! Activation authorization and immutable execution-config snapshots.
//!
//! The caller owns the state lock; this module never locks or controls services.
//! It rechecks private/non-nested directories, the trusted administrator
//! adapter, the current cgroup and the authenticated candidate before any hook
//! side effect, then pins adapter + execution JSON into a sealed per-transaction
//! directory. Recovery accepts only the same manifest/config/adapter hashes.

use crate::{
    execution::Execution,
    model::{self, Kind, Release},
    safe_fs::{self, Dir},
    transaction::Transaction,
    trust::VerifiedManifest,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub const CORE_BINARY_PATH: &str = "bin/echo-agent-core";
pub const PANEL_BINARY_PATH: &str = "bin/echo-agent-panel";
pub const ADAPTER_NAME: &str = "adapter";
const CONFIG_NAME: &str = "execution.json";
const META_NAME: &str = "snapshot.json";
const SNAPSHOT_SCHEMA: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_ADAPTER_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CGROUP_BYTES: u64 = 64 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Rejects the running instance-qualified Core/Panel service cgroup and the
/// legacy global `echo-agent-core/panel.service` names; the independent
/// `echo-agent-<instance>-update.service` unit passes. `/proc/self/cgroup` is
/// only read and parsed, never logged. The real development entry point is the
/// sole migration exception for legacy units.
#[derive(Debug, Clone, Copy)]
pub struct CgroupGuard;

impl CgroupGuard {
    pub fn current(instance_id: &str) -> Result<Self> {
        Self::parse(instance_id, &read_proc_cgroup()?)
    }

    pub fn parse(instance_id: &str, text: &str) -> Result<Self> {
        Self::parse_inner(instance_id, text, true)
    }

    /// Development/test entry point: legacy global units are tolerated because
    /// explicit development activation may be invoked from the old service
    /// during migration. Production `open` always rejects them.
    fn parse_development(instance_id: &str, text: &str) -> Result<Self> {
        Self::parse_inner(instance_id, text, false)
    }

    fn parse_inner(instance_id: &str, text: &str, reject_legacy: bool) -> Result<Self> {
        model::safe_id(instance_id)?;
        let core = format!("echo-agent-{instance_id}-core.service");
        let panel = format!("echo-agent-{instance_id}-panel.service");
        let legacy = ["echo-agent-core.service", "echo-agent-panel.service"];
        let mut entries = 0_usize;
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            // "0::/path" (v2) or "4:name=systemd:/path" (v1).
            let path = line
                .splitn(3, ':')
                .nth(2)
                .with_context(|| "malformed /proc/self/cgroup entry")?;
            ensure!(path.starts_with('/'), "malformed cgroup path");
            entries += 1;
            for component in path.split('/').filter(|part| !part.is_empty()) {
                if component == core
                    || component == panel
                    || (reject_legacy && legacy.contains(&component))
                {
                    bail!(
                        "refusing activation from {component} cgroup; \
                         run the deploy tool from the independent update unit"
                    );
                }
            }
        }
        ensure!(entries > 0, "/proc/self/cgroup contained no entries");
        Ok(Self)
    }
}

/// Authorized activation input; constructing it rechecked every boundary.
#[derive(Debug, Clone)]
pub struct ActivationContext {
    instance_id: String,
    release_id: String,
    manifest_sha256: String,
    config_sha256: String,
    adapter: PathBuf,
    adapter_sha256: String,
    state_dir: PathBuf,
    store_dir: PathBuf,
}

impl ActivationContext {
    pub fn open(
        config: &Execution,
        release: &str,
        authentication: &VerifiedManifest,
    ) -> Result<Self> {
        Self::open_with_cgroup(config, release, authentication, &read_proc_cgroup()?)
    }

    /// Deterministic entry point for tests and callers that already froze the
    /// cgroup text (tests never observe a real service cgroup).
    pub fn open_with_cgroup(
        config: &Execution,
        release: &str,
        authentication: &VerifiedManifest,
        cgroup_text: &str,
    ) -> Result<Self> {
        config.validate().context("validate execution config")?;
        model::safe_id(release)?;
        CgroupGuard::parse(&config.deployment.instance_id, cgroup_text)?;
        check_boundaries(config)?;
        let store_dir = PathBuf::from(&config.deployment.store);
        let adapter_sha256 = inspect_adapter(&config.adapter, &store_dir, &config.state_dir)?;
        check_candidate(config, release, authentication)?;
        Ok(Self {
            instance_id: config.deployment.instance_id.clone(),
            release_id: release.to_owned(),
            manifest_sha256: authentication.sha256().to_owned(),
            config_sha256: hash_bytes(&canonical_config(config)?),
            adapter: config.adapter.clone(),
            adapter_sha256,
            state_dir: config.state_dir.clone(),
            store_dir,
        })
    }

    /// Open an activation context for a staged development bundle without a
    /// trusted signature. The staged `release.json` must declare
    /// `development=true` and the caller must pass `allow_development=true`.
    ///
    /// Unlike [`Self::open`], this never fabricates a [`VerifiedManifest`]:
    /// [`crate::verify`] freezes the exact staged manifest bytes and the
    /// snapshot is bound to that SHA-256 only. Recovery should use
    /// [`Self::open_recovery`] + [`Self::load_snapshot_recovery`], which never
    /// create or re-authorize a snapshot from a development flag.
    pub fn open_development(
        config: &Execution,
        release: &str,
        allow_development: bool,
    ) -> Result<Self> {
        // The real entry point tolerates legacy global units during migration;
        // the deterministic (fake-text) entry point stays strict. Production
        // `open` and `open_recovery` always reject legacy units.
        Self::open_development_inner(
            config,
            release,
            allow_development,
            &read_proc_cgroup()?,
            false,
        )
    }

    /// Deterministic development entry point for tests and callers that
    /// already froze the cgroup text. It applies the full production cgroup
    /// guard, including legacy global Core/Panel units.
    pub fn open_development_with_cgroup(
        config: &Execution,
        release: &str,
        allow_development: bool,
        cgroup_text: &str,
    ) -> Result<Self> {
        Self::open_development_inner(config, release, allow_development, cgroup_text, true)
    }

    fn open_development_inner(
        config: &Execution,
        release: &str,
        allow_development: bool,
        cgroup_text: &str,
        reject_legacy: bool,
    ) -> Result<Self> {
        config.validate().context("validate execution config")?;
        model::safe_id(release)?;
        ensure!(
            allow_development,
            "development activation requires development=true and --allow-development"
        );
        if reject_legacy {
            CgroupGuard::parse(&config.deployment.instance_id, cgroup_text)?;
        } else {
            CgroupGuard::parse_development(&config.deployment.instance_id, cgroup_text)?;
        }
        check_boundaries(config)?;
        let store_dir = PathBuf::from(&config.deployment.store);
        let adapter_sha256 = inspect_adapter(&config.adapter, &store_dir, &config.state_dir)?;
        let manifest_sha256 = check_development_candidate(config, release)?;
        Ok(Self {
            instance_id: config.deployment.instance_id.clone(),
            release_id: release.to_owned(),
            manifest_sha256,
            config_sha256: hash_bytes(&canonical_config(config)?),
            adapter: config.adapter.clone(),
            adapter_sha256,
            state_dir: config.state_dir.clone(),
            store_dir,
        })
    }

    /// Recovery entry point without a live trust store. Trust is anchored in
    /// the durable transaction's manifest SHA-256, which was recorded only
    /// after an authenticated/development apply; the candidate must still hash
    /// to that exact manifest and the snapshot must match the same config,
    /// manifest and adapter hashes.
    pub fn open_recovery(config: &Execution, transaction: &Transaction) -> Result<Self> {
        Self::open_recovery_with_cgroup(config, transaction, &read_proc_cgroup()?)
    }

    /// Deterministic recovery entry point for tests/callers with frozen cgroup
    /// text.
    pub fn open_recovery_with_cgroup(
        config: &Execution,
        transaction: &Transaction,
        cgroup_text: &str,
    ) -> Result<Self> {
        config.validate().context("validate execution config")?;
        transaction.validate()?;
        ensure!(
            transaction.instance_id == config.deployment.instance_id,
            "transaction belongs to another instance"
        );
        CgroupGuard::parse(&config.deployment.instance_id, cgroup_text)?;
        check_boundaries(config)?;
        let store_dir = PathBuf::from(&config.deployment.store);
        let adapter_sha256 = inspect_adapter(&config.adapter, &store_dir, &config.state_dir)?;
        let bundle = crate::verify(&store_dir.join(&transaction.target_release))
            .context("verify recovery candidate")?;
        ensure!(
            bundle.manifest_sha256() == transaction.manifest_sha256,
            "candidate manifest differs from the durable transaction"
        );
        ensure!(
            bundle.release().release_id == transaction.target_release,
            "candidate manifest release_id does not equal its directory"
        );
        check_manifest(config, &transaction.target_release, bundle.release())?;
        Ok(Self {
            instance_id: config.deployment.instance_id.clone(),
            release_id: transaction.target_release.clone(),
            manifest_sha256: transaction.manifest_sha256.clone(),
            config_sha256: hash_bytes(&canonical_config(config)?),
            adapter: config.adapter.clone(),
            adapter_sha256,
            state_dir: config.state_dir.clone(),
            store_dir,
        })
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
    pub fn release_id(&self) -> &str {
        &self.release_id
    }
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }
    pub fn adapter_sha256(&self) -> &str {
        &self.adapter_sha256
    }
    pub fn candidate_root(&self) -> PathBuf {
        self.store_dir.join(&self.release_id)
    }
    pub fn core_binary_path(&self) -> PathBuf {
        self.candidate_root().join(CORE_BINARY_PATH)
    }
    pub fn snapshot_path(&self, transaction: &Transaction) -> PathBuf {
        self.state_dir
            .join(snapshot_dir_name(&self.instance_id, &transaction.id))
    }

    /// Create or idempotently reopen the snapshot. `raw_config` is optional
    /// because `main` only deserializes `Execution`; when supplied its exact
    /// bytes are retained and must match `config` semantically.
    pub fn prepare_snapshot(
        &self,
        config: &Execution,
        transaction: &Transaction,
        raw_config: Option<&[u8]>,
    ) -> Result<Snapshot> {
        ensure!(
            hash_bytes(&canonical_config(config)?) == self.config_sha256,
            "execution config changed after ActivationContext::open"
        );
        transaction.validate()?;
        ensure!(
            transaction.instance_id == self.instance_id
                && transaction.target_release == self.release_id
                && transaction.manifest_sha256 == self.manifest_sha256,
            "transaction does not match the activation context"
        );
        let config_bytes = snapshot_config_bytes(config, raw_config)?;
        let state = open_private_state(&self.state_dir)?;
        let name = snapshot_dir_name(&self.instance_id, &transaction.id);
        if let Ok(existing) = state.child(&name) {
            return self.read_snapshot(
                &self.state_dir.join(&name),
                &existing,
                transaction,
                raw_config,
            );
        }
        let temp = unique_name("snapshot-tmp");
        let dir = state.mkdir(&temp)?;
        self.write_snapshot(&dir, transaction, &config_bytes)?;
        dir.seal()?;
        if let Err(error) = state.publish(&temp, &name) {
            let _ = state
                .child(&name)
                .with_context(|| format!("publish snapshot: {error}"))?;
        }
        let path = self.state_dir.join(&name);
        let published = state.child(&name)?;
        self.read_snapshot(&path, &published, transaction, raw_config)
    }

    /// Recovery may only reopen the recorded snapshot; manifest, execution
    /// config and current hook hash must not drift.
    pub fn load_snapshot(
        &self,
        config: &Execution,
        transaction: &Transaction,
        authentication: &VerifiedManifest,
    ) -> Result<Snapshot> {
        let snapshot = verify_snapshot(
            &self.snapshot_path(transaction),
            config,
            transaction,
            authentication,
        )?;
        ensure!(
            snapshot.manifest_sha256 == self.manifest_sha256
                && snapshot.release_id == self.release_id
                && snapshot.config_sha256 == self.config_sha256,
            "snapshot belongs to a different activation context"
        );
        Ok(snapshot)
    }

    /// Reopen the recorded snapshot using the durable transaction hash as the
    /// manifest trust anchor. Never creates a snapshot.
    pub fn load_snapshot_recovery(
        &self,
        config: &Execution,
        transaction: &Transaction,
    ) -> Result<Snapshot> {
        transaction.validate()?;
        ensure!(
            transaction.instance_id == self.instance_id
                && self.manifest_sha256 == transaction.manifest_sha256
                && self.release_id == transaction.target_release
                && self.config_sha256 == hash_bytes(&canonical_config(config)?),
            "snapshot context differs from the durable transaction"
        );
        let path = self.snapshot_path(transaction);
        let dir = open_snapshot_dir(&path).context("missing or unsafe activation snapshot")?;
        self.read_snapshot(&path, &dir, transaction, None)
    }

    fn write_snapshot(&self, dir: &Dir, transaction: &Transaction, config: &[u8]) -> Result<()> {
        dir.write_new(CONFIG_NAME, config, 0o400)?;
        let mut input = open_trusted_file(&self.adapter)?;
        let mut output = dir.create(ADAPTER_NAME)?;
        let (adapter_sha256, adapter_bytes) =
            copy_reader(&mut input, &mut output, MAX_ADAPTER_BYTES)?;
        output.set_permissions(std::fs::Permissions::from_mode(0o500))?;
        output.sync_all()?;
        ensure!(
            adapter_sha256 == self.adapter_sha256,
            "trusted adapter changed between authorization and snapshot"
        );
        let meta = SnapshotMeta {
            schema_version: SNAPSHOT_SCHEMA,
            instance_id: self.instance_id.clone(),
            transaction_id: transaction.id.clone(),
            release_id: self.release_id.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
            config_sha256: self.config_sha256.clone(),
            config_file_sha256: hash_bytes(config),
            adapter_sha256,
            adapter_bytes,
        };
        dir.write_new(META_NAME, &serde_json::to_vec(&meta)?, 0o400)?;
        dir.sync()
    }

    fn read_snapshot(
        &self,
        path: &Path,
        dir: &Dir,
        transaction: &Transaction,
        raw_config: Option<&[u8]>,
    ) -> Result<Snapshot> {
        let (meta_bytes, meta) = read_meta(dir)?;
        ensure!(
            meta.instance_id == self.instance_id
                && meta.transaction_id == transaction.id
                && meta.release_id == self.release_id
                && meta.manifest_sha256 == self.manifest_sha256
                && meta.config_sha256 == self.config_sha256,
            "snapshot identity differs from the activation context"
        );
        ensure!(
            hash_file(dir, CONFIG_NAME, MAX_CONFIG_BYTES)?.0 == meta.config_file_sha256,
            "pinned execution JSON was modified"
        );
        if let Some(raw) = raw_config {
            ensure!(
                hash_bytes(raw) == meta.config_file_sha256,
                "raw execution config differs from the pinned snapshot"
            );
        }
        let (adapter_digest, adapter_bytes) = hash_file(dir, ADAPTER_NAME, MAX_ADAPTER_BYTES)?;
        ensure!(
            adapter_digest == meta.adapter_sha256 && adapter_bytes == meta.adapter_bytes,
            "pinned adapter was modified"
        );
        ensure!(
            inspect_adapter(&self.adapter, &self.store_dir, &self.state_dir)?
                == meta.adapter_sha256,
            "trusted adapter changed during the transaction"
        );
        Ok(Snapshot {
            dir: path.to_path_buf(),
            digest: hash_bytes(&meta_bytes),
            release_id: self.release_id.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
            config_sha256: self.config_sha256.clone(),
            adapter_sha256: meta.adapter_sha256,
        })
    }
}

/// Immutable pinned execution environment returned to `main`.
#[derive(Debug, Clone)]
pub struct Snapshot {
    dir: PathBuf,
    digest: String,
    release_id: String,
    manifest_sha256: String,
    config_sha256: String,
    adapter_sha256: String,
}

impl Snapshot {
    pub fn path(&self) -> &Path {
        &self.dir
    }
    /// SHA-256 of the exact `snapshot.json` bytes.
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn release_id(&self) -> &str {
        &self.release_id
    }
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }
    pub fn config_sha256(&self) -> &str {
        &self.config_sha256
    }
    pub fn adapter_path(&self) -> PathBuf {
        self.dir.join(ADAPTER_NAME)
    }
    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_NAME)
    }
    /// SHA-256 of the pinned adapter bytes.
    pub fn adapter_sha256(&self) -> &str {
        &self.adapter_sha256
    }
    /// Clone of the administrator config with `adapter` redirected to the
    /// pinned copy. Execute this, never mutable `config.adapter`, in the tx.
    pub fn pinned_execution(&self, config: &Execution) -> Result<Execution> {
        ensure!(
            hash_bytes(&canonical_config(config)?) == self.config_sha256,
            "execution config differs from the pinned snapshot"
        );
        let dir = Dir::absolute(&self.dir).context("open pinned snapshot")?;
        ensure!(
            hash_file(&dir, ADAPTER_NAME, MAX_ADAPTER_BYTES)?.0 == self.adapter_sha256,
            "pinned adapter changed"
        );
        let mut pinned = config.clone();
        pinned.adapter = self.adapter_path();
        pinned.validate()?;
        Ok(pinned)
    }
}

/// Verify a snapshot against the authenticated manifest, original execution
/// struct and transaction. Returns the metadata digest for comparison with a
/// previously recorded [`Snapshot::digest`].
pub fn verify_snapshot(
    path: &Path,
    config: &Execution,
    transaction: &Transaction,
    authentication: &VerifiedManifest,
) -> Result<Snapshot> {
    config.validate()?;
    transaction.validate()?;
    ensure!(
        transaction.instance_id == config.deployment.instance_id,
        "transaction belongs to another instance"
    );
    ensure!(
        authentication.sha256() == transaction.manifest_sha256,
        "authenticated manifest differs from the transaction manifest"
    );
    check_boundaries(config)?;
    let release = check_candidate(config, &transaction.target_release, authentication)?;
    let config_sha256 = hash_bytes(&canonical_config(config)?);
    let adapter_sha256 = inspect_adapter(
        &config.adapter,
        Path::new(&config.deployment.store),
        &config.state_dir,
    )?;
    let context = ActivationContext {
        instance_id: config.deployment.instance_id.clone(),
        release_id: release.release_id,
        manifest_sha256: authentication.sha256().to_owned(),
        config_sha256,
        adapter: config.adapter.clone(),
        adapter_sha256,
        state_dir: config.state_dir.clone(),
        store_dir: PathBuf::from(&config.deployment.store),
    };
    let dir = open_snapshot_dir(path)?;
    context.read_snapshot(path, &dir, transaction, None)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotMeta {
    schema_version: u32,
    instance_id: String,
    transaction_id: String,
    release_id: String,
    manifest_sha256: String,
    config_sha256: String,
    config_file_sha256: String,
    adapter_sha256: String,
    adapter_bytes: u64,
}

impl SnapshotMeta {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SNAPSHOT_SCHEMA,
            "unsupported snapshot schema"
        );
        model::safe_id(&self.instance_id)?;
        model::safe_id(&self.transaction_id)?;
        model::safe_id(&self.release_id)?;
        for digest in [
            &self.manifest_sha256,
            &self.config_sha256,
            &self.config_file_sha256,
            &self.adapter_sha256,
        ] {
            model::digest(digest)?;
        }
        ensure!(
            self.adapter_bytes > 0 && self.adapter_bytes <= MAX_ADAPTER_BYTES,
            "snapshot adapter size is invalid"
        );
        Ok(())
    }
}

fn read_proc_cgroup() -> Result<String> {
    let mut file = File::open("/proc/self/cgroup").context("read /proc/self/cgroup")?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_CGROUP_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_CGROUP_BYTES,
        "/proc/self/cgroup is unexpectedly large"
    );
    String::from_utf8(bytes).context("/proc/self/cgroup is not UTF-8")
}

fn check_boundaries(config: &Execution) -> Result<()> {
    let state_dir = &config.state_dir;
    let store_dir = Path::new(&config.deployment.store);
    let _state = open_private_state(state_dir)?;
    let store = Dir::absolute(store_dir).context("open candidate store")?;
    safe_fs::private_store(&store).context("candidate store")?;
    ensure!(
        !within(state_dir, store_dir) && !within(store_dir, state_dir),
        "state directory and candidate store must not nest in either direction"
    );
    Ok(())
}

fn open_private_state(path: &Path) -> Result<Dir> {
    model::absolute_path(path)?;
    let dir = Dir::absolute(path)?;
    let meta = dir.0.metadata()?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    ensure!(meta.is_dir(), "state path is not a directory");
    ensure!(
        meta.uid() == euid,
        "state directory must be owned by the caller"
    );
    ensure!(
        meta.mode() & 0o077 == 0 && meta.mode() & 0o700 == 0o700,
        "state directory must be private (0700)"
    );
    Ok(dir)
}

fn open_snapshot_dir(path: &Path) -> Result<Dir> {
    model::absolute_path(path)?;
    let dir = Dir::absolute(path)?;
    let meta = dir.0.metadata()?;
    ensure!(meta.is_dir(), "snapshot path is not a directory");
    ensure!(
        meta.mode() & 0o022 == 0,
        "snapshot directory must not be group/other writable"
    );
    Ok(dir)
}

fn inspect_adapter(path: &Path, store: &Path, state: &Path) -> Result<String> {
    ensure!(
        !within(path, store),
        "trusted adapter must not live inside the candidate store"
    );
    ensure!(
        !within(path, state),
        "trusted adapter must not live inside the transaction state directory"
    );
    let mut file = open_trusted_file(path).context("trusted adapter")?;
    let meta = file.metadata()?;
    ensure!(
        meta.nlink() == 1,
        "trusted adapter must have exactly one link"
    );
    ensure!(
        meta.mode() & 0o022 == 0,
        "trusted adapter must not be group/other writable"
    );
    ensure!(
        meta.mode() & 0o111 != 0,
        "trusted adapter must be executable"
    );
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    ensure!(
        meta.uid() == euid || meta.uid() == 0,
        "trusted adapter must be owned by the caller or root"
    );
    Ok(hash_reader(&mut file, MAX_ADAPTER_BYTES)?.0)
}

fn open_trusted_file(path: &Path) -> Result<File> {
    model::absolute_path(path)?;
    let parent = path.parent().context("trusted path has no parent")?;
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("trusted path must be UTF-8")?;
    Dir::absolute(parent)?.open_file(leaf, false)
}

fn check_candidate(
    config: &Execution,
    release: &str,
    authentication: &VerifiedManifest,
) -> Result<Release> {
    let manifest: Release =
        serde_json::from_slice(authentication.bytes()).context("parse authenticated manifest")?;
    check_manifest(config, release, &manifest)?;
    let bundle = crate::verify(&Path::new(&config.deployment.store).join(release))
        .context("verify authenticated candidate")?;
    ensure!(
        bundle.manifest_sha256() == authentication.sha256(),
        "authenticated manifest is not the exact candidate manifest"
    );
    Ok(manifest)
}

/// Development activation has no publisher signature. `crate::verify` freezes
/// the staged manifest bytes and verifies every artifact before the same
/// identity, target, topology, canonical-path and executable checks as an
/// authenticated candidate.
fn check_development_candidate(config: &Execution, release: &str) -> Result<String> {
    let bundle = crate::verify(&Path::new(&config.deployment.store).join(release))
        .context("verify development candidate")?;
    ensure!(
        bundle.release().development,
        "development activation requires development=true and --allow-development"
    );
    check_manifest(config, release, bundle.release())?;
    Ok(bundle.manifest_sha256().to_owned())
}

/// Shared shape checks for authenticated and development candidates. The
/// manifest must name this exact candidate directory and target the
/// deployment's OS/architecture/topology, and its Core/Panel artifacts must use
/// the canonical paths and be executable.
fn check_manifest(config: &Execution, release: &str, manifest: &Release) -> Result<()> {
    manifest.validate()?;
    ensure!(
        manifest.release_id == release,
        "manifest release_id must equal the candidate directory name"
    );
    config.deployment.check_release(manifest)?;
    let core: Vec<_> = manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind == Kind::Core)
        .collect();
    ensure!(
        core.len() == 1 && core[0].path == CORE_BINARY_PATH,
        "core artifact must be exactly {CORE_BINARY_PATH}"
    );
    for panel in manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind == Kind::Panel)
    {
        ensure!(
            panel.path == PANEL_BINARY_PATH,
            "panel artifact must be exactly {PANEL_BINARY_PATH}"
        );
    }
    let root = Dir::absolute(&Path::new(&config.deployment.store).join(release))?;
    for artifact in &manifest.artifacts {
        if matches!(artifact.kind, Kind::Core | Kind::Panel) {
            let file = root
                .relative_file(&artifact.path)
                .with_context(|| format!("open {}", artifact.path))?;
            ensure!(
                file.metadata()?.mode() & 0o111 != 0,
                "{} is not executable",
                artifact.path
            );
        }
    }
    Ok(())
}

fn canonical_config(config: &Execution) -> Result<Vec<u8>> {
    config.validate()?;
    serde_json::to_vec(config).context("serialize execution config")
}

fn snapshot_config_bytes(config: &Execution, raw: Option<&[u8]>) -> Result<Vec<u8>> {
    let canonical = canonical_config(config)?;
    let Some(bytes) = raw else {
        return Ok(canonical);
    };
    ensure!(
        bytes.len() as u64 <= MAX_CONFIG_BYTES,
        "raw execution config exceeds 1 MiB"
    );
    let parsed: Execution = serde_json::from_slice(bytes).context("parse raw execution config")?;
    ensure!(
        serde_json::to_value(&parsed)? == serde_json::to_value(config)?,
        "raw execution config does not match the Execution struct"
    );
    Ok(bytes.to_vec())
}

fn snapshot_dir_name(instance_id: &str, transaction_id: &str) -> String {
    // Deterministic for recovery, per-instance, and short enough for
    // `model::safe_id` even at the transaction id's 100-byte limit.
    let mut hasher = Sha256::new();
    hasher.update(instance_id.as_bytes());
    hasher.update([0]);
    hasher.update(transaction_id.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("snapshot-{}", &digest[..32])
}

fn unique_name(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{nanos}-{counter}", std::process::id())
}

fn read_meta(dir: &Dir) -> Result<(Vec<u8>, SnapshotMeta)> {
    let file = dir.open_file(META_NAME, false)?;
    ensure!(
        file.metadata()?.mode() & 0o022 == 0,
        "snapshot metadata must not be group/other writable"
    );
    let bytes = safe_fs::read_json_bytes(file)?;
    let meta: SnapshotMeta = serde_json::from_slice(&bytes).context("parse snapshot metadata")?;
    meta.validate()?;
    Ok((bytes, meta))
}

fn hash_file(dir: &Dir, name: &str, max: u64) -> Result<(String, u64)> {
    let mut file = dir.open_file(name, false)?;
    ensure!(
        file.metadata()?.mode() & 0o022 == 0,
        "snapshot file {name} must not be group/other writable"
    );
    hash_reader(&mut file, max)
}

fn hash_reader<R: Read>(reader: &mut R, max: u64) -> Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 65536];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total = total.checked_add(n as u64).context("size overflow")?;
        ensure!(total <= max, "input exceeds {max} bytes");
        hasher.update(&buffer[..n]);
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}

fn copy_reader<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    max: u64,
) -> Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 65536];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total = total.checked_add(n as u64).context("size overflow")?;
        ensure!(total <= max, "input exceeds {max} bytes");
        hasher.update(&buffer[..n]);
        writer.write_all(&buffer[..n])?;
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn within(path: &Path, ancestor: &Path) -> bool {
    path == ancestor || path.starts_with(ancestor)
}
