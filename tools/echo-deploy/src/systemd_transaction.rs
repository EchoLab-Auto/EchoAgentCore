//! Durable transaction backend for the bounded systemd primitives.
//!
//! This module is deliberately an *adapter*, not a second service manager:
//!
//! * [`SystemdExecution`] is the strict, administrator-owned input document. It
//!   contains no defaults; every field, including an explicit `panel_unit:
//!   null`, must be present. Topology and unit names are derived from
//!   `deployment.instance_id` and must match the canonical
//!   `echo-agent-{instance}-core.service` / `...-panel.service` identity.
//! * [`SystemdTransactionBackend`] implements [`crate::transaction::Backend`]
//!   with the existing [`SystemdBackend`]. It caches only hashes that were read
//!   from a descriptor-relative, verified release directory. Caller-supplied
//!   hashes are compared, never trusted as evidence.
//! * The previous release's manifest hash is captured from the observation made
//!   before the transaction begins and is persisted, before any switch, in an
//!   immutable per-transaction sidecar. Rollback verifies the old release
//!   against that sidecar, never against the manifest that happens to be on disk
//!   later.
//! * `switch` stops units and observes them inactive *before* moving the
//!   selector. `resume` starts units only after the selector names the target
//!   and re-verifies the target manifest after readiness. `restore` stops units
//!   before moving the selector back. A first-install rollback removes only the
//!   configured selector when it is a symlink owned by this transaction's target;
//!   it never removes an arbitrary path.
//! * The control adapter protocol is exactly [`CONTROL_PROTOCOL`]
//!   (`echo-deploy-control/1`). Any other protocol, a missing acknowledgement,
//!   or an acknowledgement that is contradicted by `Observe` is a hard failure.
//!   In particular `prepare` is not reported successful unless the adapter
//!   acknowledged `PrepareUpgrade` and the live observation still shows the
//!   pre-transaction release, no maintenance, and the same current manifest.
//!
//! Adapter contract used by this backend: `current_release` is the release the
//! adapter considers selected by the durable selector (it must track the
//! selector after a switch, including while units are stopped), while `ready`
//! is true only when the selected release is actually serving. `Observe` must
//! remain available while units are stopped (for example by reading durable
//! deployment state); a crash-restart cannot rely on a stopped core process to
//! answer. These are protocol requirements, not assumptions we paper over.
//!
//! Same-UID/root ancestor replacement remains outside this threat model, as in
//! `systemd.rs` and `safe_fs.rs`.

use crate::{
    model::{self, Deployment, Release, Topology},
    safe_fs::{self, Dir},
    systemd::{
        self, Deadlines, DrainOutcome as SystemdDrainOutcome, ExecTransport, Scope, SystemdBackend,
        SystemdSpec, Transport, CONTROL_PROTOCOL,
    },
    transaction::{Backend, Drain, Observation, Phase, Transaction},
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The only control-adapter protocol understood by this module.
///
/// `systemd.rs` validates the protocol field on every reply. This alias exists
/// so callers can print the supported value without guessing, and so an
/// unsupported adapter version fails closed instead of being silently accepted.
pub const SUPPORTED_CONTROL_PROTOCOL: &str = CONTROL_PROTOCOL;

const EVIDENCE_SCHEMA_VERSION: u32 = 1;
const MAX_EVIDENCE_BYTES: usize = 64 * 1024;
const MAX_TIMEOUT_SECONDS: u64 = 600;

/// Strict, administrator-owned configuration for one systemd transaction
/// backend.
///
/// Unknown fields are rejected. A missing optional field is still a missing
/// field: `panel_unit` must be present as `null` for `core_only`/`embedded`.
#[derive(Debug, Clone, Serialize)]
pub struct SystemdExecution {
    pub deployment: Deployment,
    pub state_dir: PathBuf,
    pub unit_directory: PathBuf,
    pub current_selector: PathBuf,
    pub core_unit: String,
    pub panel_unit: Option<String>,
    pub control_adapter: PathBuf,
    pub timeout_seconds: u64,
    pub reversible_data: bool,
}

/// Deserialization mirror. `Option` alone would silently accept a missing
/// `panel_unit`, so presence is checked explicitly before this mirror is used.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemdExecutionDocument {
    deployment: Deployment,
    state_dir: PathBuf,
    unit_directory: PathBuf,
    current_selector: PathBuf,
    core_unit: String,
    panel_unit: Option<String>,
    control_adapter: PathBuf,
    timeout_seconds: u64,
    reversible_data: bool,
}

impl<'de> Deserialize<'de> for SystemdExecution {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        const REQUIRED: [&str; 9] = [
            "deployment",
            "state_dir",
            "unit_directory",
            "current_selector",
            "core_unit",
            "panel_unit",
            "control_adapter",
            "timeout_seconds",
            "reversible_data",
        ];
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| D::Error::custom("SystemdExecution must be a JSON object"))?;
        for field in REQUIRED {
            if !object.contains_key(field) {
                return Err(D::Error::missing_field(field));
            }
        }
        let document: SystemdExecutionDocument =
            serde_json::from_value(value).map_err(D::Error::custom)?;
        Self::new(
            document.deployment,
            document.state_dir,
            document.unit_directory,
            document.current_selector,
            document.core_unit,
            document.panel_unit,
            document.control_adapter,
            document.timeout_seconds,
            document.reversible_data,
        )
        .map_err(D::Error::custom)
    }
}

#[allow(clippy::too_many_arguments)]
impl SystemdExecution {
    /// Construct and fully validate an execution document.
    pub fn new(
        deployment: Deployment,
        state_dir: impl Into<PathBuf>,
        unit_directory: impl Into<PathBuf>,
        current_selector: impl Into<PathBuf>,
        core_unit: impl Into<String>,
        panel_unit: Option<String>,
        control_adapter: impl Into<PathBuf>,
        timeout_seconds: u64,
        reversible_data: bool,
    ) -> Result<Self> {
        let value = Self {
            deployment,
            state_dir: state_dir.into(),
            unit_directory: unit_directory.into(),
            current_selector: current_selector.into(),
            core_unit: core_unit.into(),
            panel_unit,
            control_adapter: control_adapter.into(),
            timeout_seconds,
            reversible_data,
        };
        value.validate()?;
        Ok(value)
    }

    /// Validate paths, topology, unit identity, timeout policy, and the
    /// reversible-data declaration. No filesystem object is created or
    /// executed.
    pub fn validate(&self) -> Result<()> {
        self.deployment.validate()?;
        model::absolute_path(&self.state_dir)?;
        model::absolute_path(&self.unit_directory)?;
        model::absolute_path(&self.current_selector)?;
        model::absolute_path(&self.control_adapter)?;
        let store = Path::new(&self.deployment.store);
        model::absolute_path(store)?;

        ensure!(
            self.state_dir != store && !self.state_dir.starts_with(store),
            "state_dir and candidate store must be distinct and non-nested"
        );
        ensure!(
            self.unit_directory != self.state_dir && !self.unit_directory.starts_with(store),
            "unit_directory must differ from state_dir and stay outside the candidate store"
        );
        ensure!(
            self.current_selector != self.state_dir
                && self.current_selector.starts_with(&self.state_dir),
            "current_selector must be a descendant of state_dir"
        );
        let selector_name = self
            .current_selector
            .file_name()
            .and_then(|name| name.to_str())
            .context("current_selector must have a UTF-8 file name")?;
        model::safe_id(selector_name)?;
        ensure!(
            !self.control_adapter.starts_with(store),
            "control_adapter must not live inside the candidate store"
        );
        ensure!(
            (1..=MAX_TIMEOUT_SECONDS).contains(&self.timeout_seconds),
            "timeout_seconds outside 1..={MAX_TIMEOUT_SECONDS}"
        );
        ensure!(
            self.reversible_data,
            "automatic rollback requires explicit reversible_data=true"
        );

        // Unit identity is derived, not guessed. `install.rs` uses the same
        // canonical names for a given instance and topology.
        let base = format!("echo-agent-{}", self.deployment.instance_id);
        let expected_core = format!("{base}-core.service");
        ensure!(
            self.core_unit == expected_core,
            "core_unit {:?} does not match deployment instance {:?} (expected {expected_core:?})",
            self.core_unit,
            self.deployment.instance_id
        );
        match self.deployment.topology {
            Topology::CoreOnly | Topology::Embedded => ensure!(
                self.panel_unit.is_none(),
                "topology {:?} must not specify panel_unit",
                self.deployment.topology
            ),
            Topology::Standalone => {
                let expected_panel = format!("{base}-panel.service");
                ensure!(
                    self.panel_unit.as_deref() == Some(expected_panel.as_str()),
                    "standalone topology for instance {:?} requires panel_unit {expected_panel:?}",
                    self.deployment.instance_id
                );
            }
        }

        // Reuse SystemdSpec's bounded unit/path checks without opening files.
        self.systemd_spec()?;
        Ok(())
    }

    pub fn systemd_scope(&self) -> Scope {
        match &self.deployment.scope {
            model::Scope::User => Scope::User,
            model::Scope::System => Scope::System,
        }
    }

    pub fn deadlines(&self) -> Deadlines {
        let timeout = Duration::from_secs(self.timeout_seconds);
        Deadlines::new(timeout, timeout, timeout, timeout)
    }

    /// Build the immutable systemd description. This performs no I/O beyond
    /// lexical validation.
    pub fn systemd_spec(&self) -> Result<SystemdSpec> {
        SystemdSpec::new(
            self.deployment.instance_id.clone(),
            self.systemd_scope(),
            self.core_unit.clone(),
            self.panel_unit.clone(),
            self.unit_directory.clone(),
            self.state_dir.clone(),
            self.current_selector.clone(),
            PathBuf::from(&self.deployment.store),
            self.control_adapter.clone(),
            self.deadlines(),
        )
    }

    /// Real backend construction: [`SystemdSpec`] plus [`ExecTransport`].
    /// Construction verifies the trusted adapter file exists, but never invokes
    /// systemctl or the adapter.
    pub fn create_backend(&self) -> Result<SystemdBackend<ExecTransport>> {
        let spec = self.systemd_spec()?;
        SystemdBackend::new(spec, ExecTransport::new())
    }

    /// Real transaction backend construction. The transport is still
    /// [`ExecTransport`]; tests construct [`SystemdTransactionBackend`] with a
    /// fake transport instead.
    pub fn create_transaction_backend(&self) -> Result<SystemdTransactionBackend<ExecTransport>> {
        SystemdTransactionBackend::new(self.clone(), ExecTransport::new())
    }
}

/// Construct the real [`SystemdBackend`] from an execution document.
pub fn create_backend(execution: &SystemdExecution) -> Result<SystemdBackend<ExecTransport>> {
    execution.create_backend()
}

/// Construct the real transaction backend from an execution document.
pub fn create_transaction_backend(
    execution: &SystemdExecution,
) -> Result<SystemdTransactionBackend<ExecTransport>> {
    execution.create_transaction_backend()
}

/// A release identity read from a verified release directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseIdentity {
    release_id: String,
    manifest_sha256: String,
}

impl ReleaseIdentity {
    pub fn release_id(&self) -> &str {
        &self.release_id
    }
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }
}

/// Immutable per-transaction evidence for the previous release.
///
/// The sidecar is created with `O_EXCL` and mode `0600` in the private
/// `state_dir`; an existing file is only accepted when every field matches. It
/// is written before any switch (in `prepare`, and rechecked before `switch`),
/// so a crash cannot leave a state machine that has to guess the old hash from
/// the current disk contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviousReleaseEvidence {
    pub schema_version: u32,
    pub transaction_id: String,
    pub instance_id: String,
    pub previous_release: String,
    pub previous_manifest_sha256: String,
    pub target_release: String,
    pub target_manifest_sha256: String,
}

impl PreviousReleaseEvidence {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == EVIDENCE_SCHEMA_VERSION,
            "unsupported previous-release evidence schema"
        );
        model::safe_id(&self.transaction_id)?;
        model::safe_id(&self.instance_id)?;
        model::safe_id(&self.previous_release)?;
        model::digest(&self.previous_manifest_sha256)?;
        model::safe_id(&self.target_release)?;
        model::digest(&self.target_manifest_sha256)?;
        ensure!(
            self.previous_release != self.target_release,
            "previous and target release must differ"
        );
        Ok(())
    }
}

/// `transaction::Backend` implementation over a bounded systemd backend.
///
/// The wrapper owns no long-lived listener and never retries a failed side
/// effect behind the state machine's back. Every method returns an error unless
/// the real transport and the control adapter both confirm the result.
pub struct SystemdTransactionBackend<T> {
    execution: SystemdExecution,
    backend: SystemdBackend<T>,
    /// Latest release identity read from a verified directory and observed as
    /// the selected/current release (or set after a successful switch/restore).
    current: Option<ReleaseIdentity>,
    /// Target identity verified for the current transaction.
    target: Option<ReleaseIdentity>,
    /// Begin-time previous identity, captured before any switch.
    previous: Option<ReleaseIdentity>,
    /// Transaction currently bound to the in-memory evidence cache.
    transaction_id: Option<String>,
}

impl<T: Transport> SystemdTransactionBackend<T> {
    pub fn new(execution: SystemdExecution, transport: T) -> Result<Self> {
        execution.validate()?;
        let spec = execution.systemd_spec()?;
        let backend = SystemdBackend::new(spec, transport)?;
        Ok(Self {
            execution,
            backend,
            current: None,
            target: None,
            previous: None,
            transaction_id: None,
        })
    }

    /// Wrap an already-constructed [`SystemdBackend`] (for example a backend
    /// whose transport was injected by a test). The spec is checked against the
    /// execution document so a caller cannot silently mix two instances.
    pub fn from_backend(execution: SystemdExecution, backend: SystemdBackend<T>) -> Result<Self> {
        execution.validate()?;
        let spec = backend.spec();
        ensure!(
            spec.instance() == execution.deployment.instance_id,
            "backend instance {:?} does not match execution instance {:?}",
            spec.instance(),
            execution.deployment.instance_id
        );
        ensure!(
            spec.core_unit() == execution.core_unit
                && spec.panel_unit() == execution.panel_unit.as_deref(),
            "backend units do not match execution units"
        );
        ensure!(
            spec.state_directory() == execution.state_dir
                && spec.current_selector() == execution.current_selector
                && spec.unit_directory() == execution.unit_directory,
            "backend directories do not match execution directories"
        );
        ensure!(
            spec.store() == Path::new(&execution.deployment.store)
                && spec.control_adapter() == execution.control_adapter,
            "backend store/adapter do not match execution"
        );
        Ok(Self {
            execution,
            backend,
            current: None,
            target: None,
            previous: None,
            transaction_id: None,
        })
    }

    pub fn execution(&self) -> &SystemdExecution {
        &self.execution
    }
    pub fn backend(&self) -> &SystemdBackend<T> {
        &self.backend
    }
    pub fn backend_mut(&mut self) -> &mut SystemdBackend<T> {
        &mut self.backend
    }
    pub fn current_identity(&self) -> Option<&ReleaseIdentity> {
        self.current.as_ref()
    }
    pub fn target_identity(&self) -> Option<&ReleaseIdentity> {
        self.target.as_ref()
    }
    pub fn previous_identity(&self) -> Option<&ReleaseIdentity> {
        self.previous.as_ref()
    }
    pub fn current_manifest_sha256(&self) -> Option<&str> {
        self.current.as_ref().map(ReleaseIdentity::manifest_sha256)
    }
    pub fn target_manifest_sha256(&self) -> Option<&str> {
        self.target.as_ref().map(ReleaseIdentity::manifest_sha256)
    }
    pub fn previous_manifest_sha256(&self) -> Option<&str> {
        self.previous.as_ref().map(ReleaseIdentity::manifest_sha256)
    }
    pub fn spec(&self) -> &SystemdSpec {
        self.backend.spec()
    }
    pub fn transport(&self) -> &T {
        self.backend.transport()
    }
    pub fn transport_mut(&mut self) -> &mut T {
        self.backend.transport_mut()
    }

    fn check_instance(&self, instance_id: &str) -> Result<()> {
        ensure!(
            instance_id == self.execution.deployment.instance_id,
            "wrong installation instance {instance_id:?}, expected {:?}",
            self.execution.deployment.instance_id
        );
        Ok(())
    }

    fn bind_transaction(&mut self, transaction: &Transaction) -> Result<()> {
        self.check_instance(&transaction.instance_id)?;
        if self.transaction_id.as_deref() != Some(transaction.id.as_str()) {
            self.transaction_id = Some(transaction.id.clone());
            self.target = None;
            self.previous = None;
        }
        Ok(())
    }

    /// Read a release manifest through descriptor-relative, no-follow opens,
    /// verify its strict release model and deployment topology, and return the
    /// bytes used for the digest.
    fn read_release(&self, release: &str) -> Result<(Vec<u8>, Release)> {
        model::safe_id(release)?;
        let store = Dir::absolute(Path::new(&self.execution.deployment.store))
            .context("open candidate store for release verification")?;
        let directory = store
            .child(release)
            .with_context(|| format!("open staged release {release}"))?;
        let file = directory
            .open_file("release.json", false)
            .context("open release.json")?;
        let bytes = safe_fs::read_json_bytes(file).context("read release.json")?;
        let parsed: Release =
            serde_json::from_slice(&bytes).context("release manifest is not a strict Release")?;
        self.execution
            .deployment
            .check_release(&parsed)
            .context("release does not match deployment topology")?;
        ensure!(
            parsed.release_id == release,
            "release manifest id {:?} does not match directory {release:?}",
            parsed.release_id
        );
        Ok((bytes, parsed))
    }

    fn verify_release_identity(&self, release: &str) -> Result<ReleaseIdentity> {
        let (bytes, document) = self.read_release(release)?;
        let store = Dir::absolute(Path::new(&self.execution.deployment.store))?;
        let directory = store.child(release)?;
        // Revalidate artifact bytes at every transaction identity barrier, not
        // merely release.json. Includes Panel and every declared Web resource.
        for artifact in &document.artifacts {
            let mut file = directory
                .relative_file(&artifact.path)
                .with_context(|| format!("open artifact {}", artifact.path))?;
            let metadata = file.metadata()?;
            ensure!(
                metadata.len() == artifact.size,
                "artifact {} size mismatch",
                artifact.path
            );
            ensure!(
                !artifact.executable || metadata.mode() & 0o111 != 0,
                "artifact {} is not executable",
                artifact.path
            );
            let mut digest = Sha256::new();
            let mut count = 0u64;
            let mut buffer = [0u8; 65536];
            loop {
                let length = file.read(&mut buffer)?;
                if length == 0 {
                    break;
                }
                count = count
                    .checked_add(length as u64)
                    .context("artifact size overflow")?;
                ensure!(count <= artifact.size, "artifact {} grew", artifact.path);
                digest.update(&buffer[..length]);
            }
            ensure!(
                count == artifact.size && format!("{:x}", digest.finalize()) == artifact.sha256,
                "artifact {} SHA256/size mismatch",
                artifact.path
            );
        }
        Ok(ReleaseIdentity {
            release_id: release.to_owned(),
            manifest_sha256: hex_sha256(&bytes),
        })
    }

    fn verify_target_identity(&self, transaction: &Transaction) -> Result<ReleaseIdentity> {
        systemd::verify_release_directory(
            self.backend.spec(),
            &transaction.target_release,
            &transaction.manifest_sha256,
        )
        .context("verify target release manifest digest/id")?;
        let identity = self.verify_release_identity(&transaction.target_release)?;
        ensure!(
            identity.manifest_sha256 == transaction.manifest_sha256,
            "target manifest digest changed while verifying"
        );
        Ok(identity)
    }

    fn observe_runtime(&mut self, instance_id: &str) -> Result<Observation> {
        self.check_instance(instance_id)?;
        let runtime = self
            .backend
            .observe()
            .context("observe live systemd/control state")?;
        ensure!(
            runtime.selector_matches(),
            "control adapter reports current release {:?} but selector points at {:?}; refusing to act on drift",
            runtime.current_release,
            runtime.selector_release
        );

        let identity = match runtime.selector_release.as_deref() {
            Some(release) => Some(
                self.verify_release_identity(release)
                    .with_context(|| format!("verify current release {release}"))?,
            ),
            None => None,
        };
        if let (Some(cached), Some(actual)) = (&self.current, &identity) {
            if cached.release_id == actual.release_id {
                ensure!(
                    cached.manifest_sha256 == actual.manifest_sha256,
                    "current release {} manifest changed since it was observed; refusing to act",
                    actual.release_id
                );
            }
        }
        ensure!(
            !(identity.is_none() && runtime.ready),
            "control adapter reports ready without any selected release"
        );
        self.current = identity;
        Ok(Observation::new(
            runtime.current_release,
            runtime.maintenance,
            runtime.ready,
            runtime.active_work,
        ))
    }

    fn private_state_dir(&self) -> Result<Dir> {
        let dir = Dir::absolute(&self.execution.state_dir)
            .with_context(|| format!("open state_dir {}", self.execution.state_dir.display()))?;
        let metadata = dir.0.metadata()?;
        ensure!(metadata.is_dir(), "state_dir must be a directory");
        ensure!(
            metadata.uid() == current_euid(),
            "state_dir must be owned by the current euid"
        );
        ensure!(
            metadata.mode() & 0o077 == 0,
            "state_dir must be private (no group/other permissions)"
        );
        Ok(dir)
    }

    /// Persist only observations obtained from the configured transport. The
    /// runtime adapter opts in by pointing its first_install.evidence at this
    /// fixed private path; no missing-socket fallback manufactures this proof.
    fn persist_first_install_evidence(&mut self, transaction: &Transaction) -> Result<()> {
        ensure!(
            self.backend.current_release()?.is_none(),
            "first-install evidence requires an absent selector"
        );
        let units = self.backend.verify_units_inactive()?;
        ensure!(
            self.backend.current_release()?.is_none(),
            "selector appeared while verifying inactive units"
        );
        let observed_at_unix_ms =
            u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
        let evidence = crate::runtime_adapter::FirstInstallEvidence {
            schema_version: crate::runtime_adapter::FIRST_INSTALL_EVIDENCE_SCHEMA_VERSION,
            source: crate::runtime_adapter::FIRST_INSTALL_EVIDENCE_SOURCE.into(),
            instance_id: transaction.instance_id.clone(),
            target_release: transaction.target_release.clone(),
            target_manifest_sha256: transaction.manifest_sha256.clone(),
            selector_absent: true,
            units: units
                .into_iter()
                .map(|unit| crate::runtime_adapter::FirstInstallUnit {
                    unit,
                    active_state: "inactive".into(),
                })
                .collect(),
            observed_at_unix_ms,
        };
        evidence.validate_shape()?;
        let directory = self.private_state_dir()?;
        let name = "first-install-evidence.json";
        if let Ok(file) = directory.open_file(name, false) {
            let metadata = file.metadata()?;
            ensure!(
                metadata.uid() == current_euid()
                    && metadata.nlink() == 1
                    && metadata.mode() & 0o077 == 0,
                "existing first-install evidence must be a private single-link file"
            );
            let previous: crate::runtime_adapter::FirstInstallEvidence =
                serde_json::from_slice(&safe_fs::read_json_bytes(file)?)?;
            previous.validate_shape()?;
            ensure!(
                previous.instance_id == evidence.instance_id,
                "first-install evidence belongs to another instance"
            );
        } else {
            ensure!(
                !self.execution.state_dir.join(name).exists(),
                "first-install evidence path is not a readable regular file"
            );
        }
        let temporary = format!(
            "first-install-{}-{}.tmp",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        directory.write_new(&temporary, &serde_json::to_vec(&evidence)?, 0o600)?;
        // Both names live in the pinned private directory. rename atomically
        // refreshes stale first-install observations; no data/config is touched.
        use std::os::fd::AsRawFd;
        let old = std::ffi::CString::new(temporary)?;
        let new = std::ffi::CString::new(name)?;
        // SAFETY: owned dir fd and single-component C strings remain live.
        let result = unsafe {
            libc::renameat(
                directory.0.as_raw_fd(),
                old.as_ptr(),
                directory.0.as_raw_fd(),
                new.as_ptr(),
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        directory.sync()?;
        Ok(())
    }

    fn persist_previous_evidence(
        &self,
        transaction: &Transaction,
        previous: &ReleaseIdentity,
    ) -> Result<()> {
        let evidence = PreviousReleaseEvidence {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            transaction_id: transaction.id.clone(),
            instance_id: transaction.instance_id.clone(),
            previous_release: previous.release_id.clone(),
            previous_manifest_sha256: previous.manifest_sha256.clone(),
            target_release: transaction.target_release.clone(),
            target_manifest_sha256: transaction.manifest_sha256.clone(),
        };
        evidence.validate()?;
        let bytes = serde_json::to_vec(&evidence).context("serialize previous-release evidence")?;
        ensure!(
            bytes.len() <= MAX_EVIDENCE_BYTES,
            "previous-release evidence exceeds size limit"
        );
        let directory = self.private_state_dir()?;
        let name = evidence_file_name(&transaction.instance_id, &transaction.id);

        match directory.open_file(&name, false) {
            Ok(file) => {
                let existing_bytes = safe_fs::read_json_bytes(file)
                    .context("read existing previous-release evidence")?;
                let existing: PreviousReleaseEvidence = serde_json::from_slice(&existing_bytes)
                    .context("parse existing previous-release evidence")?;
                existing.validate()?;
                ensure!(
                    existing == evidence,
                    "existing previous-release evidence differs; refusing to overwrite immutable evidence"
                );
                // Re-sync the directory even when the sidecar already exists:
                // a previous attempt may have failed after writing the file but
                // before making the entry durable.
                directory.sync()?;
                return Ok(());
            }
            Err(error) if is_not_found(&error) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect existing previous-release evidence {name}"));
            }
        }

        let mut file = directory
            .create(&name)
            .with_context(|| format!("create immutable previous-release evidence {name}"))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == current_euid()
                && metadata.mode() & 0o077 == 0,
            "previous-release evidence is not a private single-link file"
        );
        directory.sync()?;
        Ok(())
    }

    fn load_previous_evidence(&self, transaction: &Transaction) -> Result<PreviousReleaseEvidence> {
        let previous_release = transaction
            .previous_release
            .as_deref()
            .context("transaction has no previous release")?;
        let directory = self.private_state_dir()?;
        let name = evidence_file_name(&transaction.instance_id, &transaction.id);
        let file = directory.open_file(&name, false).with_context(|| {
            format!(
                "open immutable previous-release evidence {name}; \
                 refusing to touch an existing installation without begin-time old-manifest evidence"
            )
        })?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == current_euid()
                && metadata.mode() & 0o077 == 0,
            "previous-release evidence must be a private single-link file"
        );
        let bytes =
            safe_fs::read_json_bytes(file).context("read previous-release evidence bytes")?;
        let evidence: PreviousReleaseEvidence = serde_json::from_slice(&bytes)
            .context("parse previous-release evidence as strict JSON")?;
        evidence.validate()?;
        ensure!(
            evidence.transaction_id == transaction.id,
            "previous-release evidence belongs to transaction {:?}",
            evidence.transaction_id
        );
        ensure!(
            evidence.instance_id == transaction.instance_id,
            "previous-release evidence belongs to instance {:?}",
            evidence.instance_id
        );
        ensure!(
            evidence.previous_release == previous_release,
            "previous-release evidence names {:?}, transaction expects {:?}",
            evidence.previous_release,
            previous_release
        );
        ensure!(
            evidence.target_release == transaction.target_release,
            "previous-release evidence target {:?}, transaction expects {:?}",
            evidence.target_release,
            transaction.target_release
        );
        ensure!(
            evidence.target_manifest_sha256 == transaction.manifest_sha256,
            "previous-release evidence target digest does not match transaction"
        );
        Ok(evidence)
    }

    /// Ensure the immutable sidecar exists before a switch. If this process
    /// still holds the begin-time identity, write/confirm it now; otherwise the
    /// sidecar from the earlier attempt must already exist.
    fn require_previous_evidence(&mut self, transaction: &Transaction) -> Result<()> {
        if transaction.previous_release.is_none() {
            return Ok(());
        }
        if let Some(previous) = &self.previous {
            self.persist_previous_evidence(transaction, previous)?;
        }
        let evidence = self.load_previous_evidence(transaction)?;
        ensure!(
            evidence.previous_release
                == transaction.previous_release.as_deref().unwrap_or_default(),
            "previous-release evidence differs from transaction"
        );
        Ok(())
    }

    /// Remove only the configured selector when it is a symlink that points at
    /// this transaction's target release. No other path is considered, even if
    /// it looks like a selector.
    fn remove_owned_selector(&self, transaction: &Transaction) -> Result<()> {
        let selector = self.backend.spec().current_selector();
        let metadata = match fs::symlink_metadata(selector) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("stat current_selector"),
        };
        ensure!(
            metadata.file_type().is_symlink(),
            "refusing to remove current_selector: it is not a symlink"
        );
        let target = fs::read_link(selector).context("read current_selector target")?;
        model::absolute_path(&target)?;
        model::safe_id(&transaction.target_release)?;
        let expected = self
            .backend
            .spec()
            .store()
            .join(&transaction.target_release);
        ensure!(
            target == expected,
            "refusing to remove current_selector: target {} is not this transaction's target {}",
            target.display(),
            expected.display()
        );

        let parent = selector
            .parent()
            .context("current_selector has no parent directory")?;
        let leaf = selector
            .file_name()
            .and_then(|name| name.to_str())
            .context("current_selector file name is not UTF-8")?;
        model::safe_id(leaf)?;
        let directory = Dir::absolute(parent).context("open current_selector parent")?;
        let leaf = CString::new(leaf).context("current_selector file name")?;
        // SAFETY: `directory` is a pinned directory fd and `leaf` is a single
        // validated component. `unlinkat(..., 0)` removes the symlink itself.
        let result = unsafe { libc::unlinkat(directory.0.as_raw_fd(), leaf.as_ptr(), 0) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(());
            }
            return Err(error).context("remove owned current_selector symlink");
        }
        directory.sync()?;
        match fs::symlink_metadata(selector) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("confirm current_selector removal"),
            Ok(_) => bail!("current_selector still exists after removal"),
        }
    }

    fn restore_previous(
        &mut self,
        transaction: &Transaction,
        previous_release: &str,
    ) -> Result<()> {
        let evidence = self.load_previous_evidence(transaction)?;
        ensure!(
            evidence.previous_release == previous_release,
            "previous-release evidence names {:?}, transaction expects {:?}",
            evidence.previous_release,
            previous_release
        );

        // Verify the old release against the durable begin-time digest before
        // any side effect. A manifest that changed on disk is not "the old
        // release" merely because its directory name is unchanged.
        let previous = self.verify_release_identity(previous_release)?;
        ensure!(
            previous.manifest_sha256 == evidence.previous_manifest_sha256,
            "previous release manifest digest does not match begin-time evidence"
        );

        let selector = self
            .backend
            .current_release()
            .context("read current_selector before restore")?;
        ensure!(
            selector.is_none()
                || selector.as_deref() == Some(previous_release)
                || selector.as_deref() == Some(transaction.target_release.as_str()),
            "restore refuses to move selector pointing at unexpected release {selector:?}"
        );

        let observation = self.observe_runtime(&transaction.instance_id)?;
        ensure!(observation.maintenance, "restore requires maintenance mode");
        ensure!(
            observation.active_work == 0,
            "restore requires a drained instance"
        );
        if selector.as_deref() == Some(previous_release)
            && observation.current_release.as_deref() == Some(previous_release)
            && observation.ready
        {
            self.current = Some(previous);
            return Ok(());
        }

        // Stop before moving the pointer back, otherwise `systemctl start` on
        // an already-active target would be a no-op and old code would stay
        // alive while the selector claimed the previous release.
        self.backend
            .stop_units()
            .context("stop target units before restore switch")?;
        let switched = self
            .backend
            .switch_release(previous_release, &evidence.previous_manifest_sha256)
            .context("switch selector back to previous release")?;
        ensure!(
            switched.current == self.backend.spec().store().join(previous_release),
            "selector switch did not land on the previous release directory"
        );
        self.backend
            .start_units()
            .context("start previous release units")?;
        self.backend
            .verify_ready(previous_release)
            .context("verify previous release readiness")?;

        let after = self.verify_release_identity(previous_release)?;
        ensure!(
            after == previous,
            "previous release manifest changed during restore"
        );
        let observation = self.observe_runtime(&transaction.instance_id)?;
        ensure!(
            observation.current_release.as_deref() == Some(previous_release),
            "restore observed release {:?}, expected {previous_release:?}",
            observation.current_release
        );
        ensure!(observation.ready, "restored previous release is not ready");
        ensure!(
            observation.maintenance,
            "restore must leave maintenance active for the caller"
        );
        self.current = Some(previous);
        Ok(())
    }

    fn restore_first_install(&mut self, transaction: &Transaction) -> Result<()> {
        // Stop first so a failed first install is never left serving, even if
        // the selector path was tampered with. Removal remains strictly
        // limited to the configured selector and this transaction's target.
        self.backend
            .stop_units()
            .context("stop units for first-install rollback")?;
        let selector = self
            .backend
            .current_release()
            .context("read current_selector before first-install restore")?;
        ensure!(
            selector.is_none() || selector.as_deref() == Some(transaction.target_release.as_str()),
            "first-install restore refuses selector pointing at unexpected release {selector:?}"
        );
        if selector.is_some() {
            self.remove_owned_selector(transaction)?;
        }
        ensure!(
            self.backend
                .current_release()
                .context("confirm selector removal")?
                .is_none(),
            "current_selector still names a release after first-install rollback"
        );
        // Rollback may happen long after prepare. Refresh only after the real
        // units were stopped and the owned selector was removed, so an old
        // timestamp cannot strand a safe empty installation in maintenance.
        self.persist_first_install_evidence(transaction)?;
        self.current = None;
        Ok(())
    }
}

impl<T: Transport> Backend for SystemdTransactionBackend<T> {
    fn observe(&mut self, instance_id: &str) -> Result<Observation> {
        self.observe_runtime(instance_id)
    }

    fn prepare(&mut self, transaction: &Transaction) -> Result<()> {
        self.bind_transaction(transaction)?;
        let target = self.verify_target_identity(transaction)?;

        match &transaction.previous_release {
            Some(previous_release) => {
                let observed = self.current.clone().context(
                    "no begin-time observation of the previous release; refusing to upgrade an existing installation without durable old-manifest evidence",
                )?;
                ensure!(
                    observed.release_id == *previous_release,
                    "begin-time observation was release {:?}, transaction previous is {:?}",
                    observed.release_id,
                    previous_release
                );
                if let Some(cached) = &self.previous {
                    ensure!(
                        cached == &observed,
                        "begin-time previous-release identity changed during prepare"
                    );
                }
                self.previous = Some(observed.clone());
                // Failure here is intentionally fatal for existing installs:
                // without a durable begin-time old digest, rollback cannot be
                // truthful. First installs have no old release and skip this.
                self.persist_previous_evidence(transaction, &observed)?;
            }
            None => {
                ensure!(
                    self.current.is_none(),
                    "first-install transaction but backend observes release {:?}",
                    self.current
                        .as_ref()
                        .map(|identity| identity.release_id.as_str())
                );
                self.persist_first_install_evidence(transaction)?;
            }
        }

        // The control adapter must acknowledge PrepareUpgrade; local target
        // verification alone is not "prepared".
        let verified = self
            .backend
            .prepare_upgrade(&transaction.target_release, &transaction.manifest_sha256)
            .context("control adapter PrepareUpgrade preflight")?;
        ensure!(
            verified.release_id() == target.release_id
                && verified.manifest_sha256() == target.manifest_sha256,
            "control preflight verified a different release identity"
        );

        // A reply of success:true that also moved live state is not a valid
        // prepare. Re-read the real observation instead of trusting the ACK.
        let observation = self.observe_runtime(&transaction.instance_id)?;
        ensure!(
            !observation.maintenance,
            "control adapter entered maintenance during prepare"
        );
        ensure!(
            observation.current_release == transaction.previous_release,
            "prepare changed the current release from {:?} to {:?}",
            transaction.previous_release,
            observation.current_release
        );
        self.target = Some(target);
        Ok(())
    }

    fn enter_maintenance(&mut self, transaction: &Transaction) -> Result<()> {
        self.bind_transaction(transaction)?;
        self.backend
            .enter_maintenance()
            .context("control adapter MaintenanceEnter")?;
        let observation = self.observe_runtime(&transaction.instance_id)?;
        ensure!(
            observation.maintenance,
            "control adapter acknowledged maintenance but observation still reports false"
        );
        ensure!(
            observation.current_release == transaction.previous_release,
            "entering maintenance changed the current release"
        );
        Ok(())
    }

    fn drain(&mut self, transaction: &Transaction) -> Result<Drain> {
        self.bind_transaction(transaction)?;
        let before = self.observe_runtime(&transaction.instance_id)?;
        ensure!(
            before.maintenance,
            "checked drain requires maintenance mode"
        );
        ensure!(
            before.current_release == transaction.previous_release,
            "checked drain requires the pre-transaction release {:?}, observed {:?}",
            transaction.previous_release,
            before.current_release
        );
        if transaction.previous_release.is_none() {
            self.persist_first_install_evidence(transaction)?;
        }
        match self
            .backend
            .drain()
            .context("control adapter checked drain")?
        {
            SystemdDrainOutcome::Drained => {
                let after = self.observe_runtime(&transaction.instance_id)?;
                ensure!(
                    after.maintenance,
                    "drain ACK released maintenance unexpectedly"
                );
                ensure!(
                    after.active_work == 0,
                    "drain ACK reported empty but observation reports {} active work item(s)",
                    after.active_work
                );
                Ok(Drain::Drained)
            }
            SystemdDrainOutcome::Busy { remaining } => Ok(Drain::Busy { remaining }),
        }
    }

    fn switch(&mut self, transaction: &Transaction) -> Result<()> {
        self.bind_transaction(transaction)?;
        let target = self.verify_target_identity(transaction)?;
        if let Some(cached) = &self.target {
            ensure!(
                cached == &target,
                "target release identity changed since prepare"
            );
        }

        let selector = self
            .backend
            .current_release()
            .context("read current_selector before switch")?;
        if selector.as_deref() == Some(transaction.target_release.as_str()) {
            // Idempotent retry: the pointer already names the verified target.
            // Do not stop/start here; `resume` owns the start side.
            self.require_previous_evidence(transaction)?;
            self.target = Some(target.clone());
            self.current = Some(target);
            return Ok(());
        }
        ensure!(
            selector.as_deref() == transaction.previous_release.as_deref(),
            "selector points at {selector:?}, transaction previous is {:?}",
            transaction.previous_release
        );

        let observation = self.observe_runtime(&transaction.instance_id)?;
        ensure!(observation.maintenance, "switch requires maintenance mode");
        ensure!(
            observation.active_work == 0,
            "switch requires a drained instance"
        );
        ensure!(
            observation.current_release == transaction.previous_release,
            "switch requires the pre-transaction release {:?}, observed {:?}",
            transaction.previous_release,
            observation.current_release
        );

        // The immutable old digest must already be durable before we touch a
        // unit or the pointer. If it cannot be written, existing-install
        // upgrades refuse to proceed.
        self.require_previous_evidence(transaction)?;

        // Absolutely stop before moving the pointer: never leave the old
        // process alive while the selector claims the target.
        self.backend
            .stop_units()
            .context("stop units before selector switch")?;
        let switched = self
            .backend
            .switch_release(&target.release_id, &target.manifest_sha256)
            .context("switch selector to target release")?;
        ensure!(
            switched.current == self.backend.spec().store().join(&target.release_id),
            "selector switch did not land on the target release directory"
        );
        let observed = self
            .backend
            .current_release()
            .context("confirm selector target after switch")?;
        ensure!(
            observed.as_deref() == Some(target.release_id.as_str()),
            "selector did not move to the target release"
        );
        self.target = Some(target.clone());
        self.current = Some(target);
        Ok(())
    }

    fn resume(&mut self, transaction: &Transaction) -> Result<()> {
        self.bind_transaction(transaction)?;
        let target = self.verify_target_identity(transaction)?;
        if let Some(cached) = &self.target {
            ensure!(
                cached == &target,
                "target release identity changed since prepare"
            );
        }
        ensure!(
            self.backend
                .current_release()
                .context("read selector before resume")?
                .as_deref()
                == Some(target.release_id.as_str()),
            "resume requires the selector to name the target release"
        );

        self.backend.start_units().context("start target units")?;
        self.backend
            .verify_ready(&target.release_id)
            .context("control adapter Ready for target")?;

        // Hash verification after start and readiness: a release directory
        // swapped mid-start is not the release we prepared.
        let after = self.verify_release_identity(&target.release_id)?;
        ensure!(
            after == target,
            "target release manifest changed during start/readiness"
        );
        let observation = self.observe_runtime(&transaction.instance_id)?;
        ensure!(
            observation.current_release.as_deref() == Some(target.release_id.as_str()),
            "resume observed release {:?}, expected {:?}",
            observation.current_release,
            target.release_id
        );
        ensure!(observation.ready, "target release did not become ready");
        ensure!(
            observation.maintenance,
            "resume must complete before maintenance is released"
        );
        self.target = Some(target.clone());
        self.current = Some(target);
        Ok(())
    }

    fn verify_ready(&mut self, transaction: &Transaction) -> Result<()> {
        self.bind_transaction(transaction)?;
        let target = self.verify_target_identity(transaction)?;
        if let Some(cached) = &self.target {
            ensure!(
                cached == &target,
                "target release identity changed since prepare"
            );
        }
        ensure!(
            self.backend
                .current_release()
                .context("read selector before readiness")?
                .as_deref()
                == Some(target.release_id.as_str()),
            "readiness requires the selector to name the target release"
        );

        self.backend
            .verify_ready(&target.release_id)
            .context("control adapter Ready for target")?;
        let after = self.verify_release_identity(&target.release_id)?;
        ensure!(
            after == target,
            "target release manifest changed during readiness verification"
        );
        let observation = self.observe_runtime(&transaction.instance_id)?;
        ensure!(
            observation.current_release.as_deref() == Some(target.release_id.as_str()),
            "readiness observed release {:?}, expected {:?}",
            observation.current_release,
            target.release_id
        );
        ensure!(observation.ready, "target release is not ready");
        ensure!(
            observation.maintenance,
            "readiness must be verified before maintenance release"
        );
        self.target = Some(target);
        Ok(())
    }

    fn release_maintenance(&mut self, transaction: &Transaction) -> Result<()> {
        self.bind_transaction(transaction)?;
        let expected = match transaction.phase {
            Phase::Committed => Some(transaction.target_release.as_str()),
            _ => transaction.previous_release.as_deref(),
        };
        if expected.is_none() {
            self.persist_first_install_evidence(transaction)?;
        }
        self.backend
            .leave_maintenance()
            .context("control adapter MaintenanceLeave")?;
        let observation = self.observe_runtime(&transaction.instance_id)?;
        ensure!(
            !observation.maintenance,
            "control adapter acknowledged maintenance release but observation still reports true"
        );
        ensure!(
            observation.current_release.as_deref() == expected,
            "maintenance release observed release {:?}, expected {expected:?}",
            observation.current_release
        );
        match expected {
            Some(_) => ensure!(
                observation.ready,
                "maintenance release requires a ready release"
            ),
            None => ensure!(
                !observation.ready,
                "maintenance release with no release must not report ready"
            ),
        }
        Ok(())
    }

    fn restore(&mut self, transaction: &Transaction) -> Result<()> {
        self.bind_transaction(transaction)?;
        match &transaction.previous_release {
            Some(previous_release) => self.restore_previous(transaction, previous_release),
            None => self.restore_first_install(transaction),
        }
    }
}

fn evidence_file_name(instance_id: &str, transaction_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(instance_id.as_bytes());
    digest.update([0]);
    digest.update(transaction_id.as_bytes());
    format!("previous-{:x}.json", digest.finalize())
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn current_euid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}
