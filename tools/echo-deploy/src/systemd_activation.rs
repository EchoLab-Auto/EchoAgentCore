//! Authorized systemd activation plus durable full-config snapshot evidence.
//!
//! [`crate::activation::ActivationContext`] freezes the administrator-owned
//! generic [`Execution`] document (deployment, state directory, adapter path,
//! timeout and the reversible-data declaration) and recovery re-verifies that
//! exact projection. The systemd transaction backend, however, reads a much
//! larger [`SystemdExecution`] document: `unit_directory`, `current_selector`,
//! `core_unit`, `panel_unit`, `control_adapter` and the unit identity are not
//! part of the generic projection. Applying or recovering a transaction from
//! that live document can therefore silently drift even when the generic
//! projection still matches.
//!
//! This module closes that gap without modifying the existing activation API:
//!
//! * [`mapped_execution`] projects a [`SystemdExecution`] onto the generic
//!   [`Execution`] contract, so every existing authorization boundary (private
//!   state/store, no symlinked ancestors, trusted adapter, cgroup, authenticated
//!   or explicit-development candidate) is reused unchanged.
//! * [`SystemdActivationContext`] additionally hashes the *complete* original
//!   systemd document with [`systemd_config_sha256`]. Every field participates,
//!   including the fields that do not exist in [`Execution`].
//! * [`SystemdActivationContext::prepare_snapshot`] lets the activation module
//!   pin the trusted adapter bytes and the mapped execution JSON, then publishes
//!   a sealed `systemd-snapshot-<digest>` evidence directory next to the
//!   snapshot. It contains the original full systemd config bytes and every
//!   binding hash (full config, mapped config, adapter, manifest, snapshot).
//! * [`SystemdActivationContext::load_snapshot_recovery`] only reopens those
//!   durable bytes. It never creates a snapshot, never needs a trust store, and
//!   binds recovery to the durable transaction's manifest hash plus the existing
//!   snapshot/evidence hashes.
//! * [`SystemdSnapshot::pinned_systemd_execution`] returns a clone of the frozen
//!   full document whose `control_adapter` points at the immutable pinned copy,
//!   so the transaction backend can never execute a mutable adapter path.
//!
//! Typical wiring (the caller owns the state lock; this module never locks):
//!
//! ```text
//! // apply: authentication is a VerifiedManifest, or an explicit development
//! // authorization for a staged manifest with development=true.
//! let context = SystemdActivationContext::open_with_cgroup(
//!     &execution, release, &authentication, &cgroup_text)?;
//! let snapshot = context.prepare_snapshot(&execution, &transaction, Some(&raw_config))?;
//! let pinned = snapshot.pinned_systemd_execution()?;
//! let mut backend = pinned.create_transaction_backend()?;
//!
//! // recover: no live trust store; the durable transaction is the anchor.
//! let context = SystemdActivationContext::open_recovery_with_cgroup(
//!     &execution, &transaction, &cgroup_text)?;
//! let snapshot = context.load_snapshot_recovery(&execution, &transaction)?;
//! let pinned = snapshot.pinned_systemd_execution()?;
//! ```
//!
//! Apply ordering note: `TransactionStore::begin` observes the backend before it
//! returns the durable transaction. Prepare the snapshot first with the same
//! transaction id (a provisional `Prepared` transaction from
//! [`provisional_transaction`] is sufficient: the snapshot records id, instance,
//! target release and manifest hash, while `begin` fills `previous_release` from
//! the pinned backend observation), then pass the pinned backend to `begin`.
//! Recovery loads the durable transaction with `TransactionStore::load` and
//! never creates a snapshot.
//!
//! A crash may leave a private `systemd-snapshot-tmp-*` directory. It is never
//! read as evidence; recovery neither cleans nor recreates anything.
//!
//! Same-UID/root attackers remain outside this threat model, exactly as in
//! `activation.rs`, `systemd.rs` and `safe_fs.rs`.

use crate::{
    activation::{ActivationContext, Snapshot},
    execution::Execution,
    model,
    safe_fs::{self, Dir},
    systemd::{ExecTransport, SystemdBackend},
    systemd_transaction::{SystemdExecution, SystemdTransactionBackend},
    transaction::{BeginSpec, Phase, Transaction, TRANSACTION_SCHEMA_VERSION},
    trust::VerifiedManifest,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// Version of the durable evidence document written by this module.
pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;
/// File name of the original full [`SystemdExecution`] bytes inside evidence.
pub const SYSTEMD_CONFIG_FILE: &str = "systemd.json";
/// File name of the binding-hash document inside evidence.
pub const EVIDENCE_FILE: &str = "evidence.json";
/// Upper bound for one full systemd config document (mirrors activation).
pub const MAX_SYSTEMD_CONFIG_BYTES: u64 = 1024 * 1024;
/// Upper bound for the binding-hash document.
pub const MAX_EVIDENCE_BYTES: u64 = 64 * 1024;
/// Upper bound for the pinned control adapter bytes.
pub const MAX_ADAPTER_BYTES: u64 = 256 * 1024 * 1024;

const TEMP_PREFIX: &str = "systemd-snapshot-tmp";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Project a full systemd document onto the generic execution contract.
///
/// This is a pure projection, not an authorization: it validates only the
/// resulting [`Execution`]. Systemd-only fields (`unit_directory`,
/// `current_selector`, `core_unit`, `panel_unit`) deliberately do not appear in
/// the result. Callers must use [`SystemdActivationContext`] (which validates
/// the full document and hashes every field) before any side effect.
pub fn mapped_execution(config: &SystemdExecution) -> Result<Execution> {
    let mapped = Execution {
        schema_version: 1,
        deployment: config.deployment.clone(),
        state_dir: config.state_dir.clone(),
        adapter: config.control_adapter.clone(),
        timeout_seconds: config.timeout_seconds,
        reversible_data: config.reversible_data,
    };
    mapped.validate()?;
    Ok(mapped)
}

/// Canonical (compact serde) bytes of the complete systemd document.
///
/// The value is only serialization: no default is invented and no field is
/// dropped. This is the exact byte stream covered by
/// [`systemd_config_sha256`].
pub fn canonical_systemd_config(config: &SystemdExecution) -> Result<Vec<u8>> {
    serde_json::to_vec(config).context("serialize full systemd execution config")
}

/// SHA-256 of [`canonical_systemd_config`], covering every systemd field.
pub fn systemd_config_sha256(config: &SystemdExecution) -> Result<String> {
    Ok(hash_bytes(&canonical_systemd_config(config)?))
}

/// Build the provisional `Prepared` transaction needed to prepare a snapshot
/// before [`crate::transaction::TransactionStore::begin`] observes the pinned
/// backend.
///
/// The result carries exactly the durable identity fields from `spec`
/// (`transaction_id`, `instance_id`, `target_release`, `manifest_sha256`) with
/// `previous_release: None`, `phase: Prepared` and no steps. `begin` later fills
/// `previous_release` from its observation; the snapshot only binds the identity
/// fields, so the same id remains valid. Constructing the snapshot first lets
/// `begin` and `execute` share one backend built from
/// [`SystemdSnapshot::pinned_systemd_execution`].
pub fn provisional_transaction(spec: &BeginSpec) -> Result<Transaction> {
    let transaction = Transaction {
        schema_version: TRANSACTION_SCHEMA_VERSION,
        id: spec.transaction_id.clone(),
        instance_id: spec.instance_id.clone(),
        previous_release: None,
        target_release: spec.target_release.clone(),
        manifest_sha256: spec.manifest_sha256.clone(),
        phase: Phase::Prepared,
        error: None,
        steps: Vec::new(),
    };
    transaction.validate()?;
    Ok(transaction)
}

/// Authorized systemd activation input.
///
/// Construction reuses [`ActivationContext`] for all generic boundaries and
/// additionally freezes the complete [`SystemdExecution`] document, including
/// fields that never reach [`Execution`]. The caller owns the state lock.
#[derive(Debug, Clone)]
pub struct SystemdActivationContext {
    activation: ActivationContext,
    systemd: SystemdExecution,
    mapped: Execution,
    instance_id: String,
    release_id: String,
    manifest_sha256: String,
    systemd_config_sha256: String,
    mapped_config_sha256: String,
    control_adapter_sha256: String,
    state_dir: PathBuf,
    store_dir: PathBuf,
}

impl SystemdActivationContext {
    /// Production apply authorization: an authenticated manifest plus the fake
    /// or real cgroup text. The Core/Panel service cgroups are rejected.
    pub fn open_with_cgroup(
        config: &SystemdExecution,
        release: &str,
        authentication: &VerifiedManifest,
        cgroup_text: &str,
    ) -> Result<Self> {
        config
            .validate()
            .context("validate full systemd execution config")?;
        let mapped = mapped_execution(config)?;
        check_systemd_boundaries(config)?;
        let activation =
            ActivationContext::open_with_cgroup(&mapped, release, authentication, cgroup_text)?;
        Self::finish(config.clone(), activation)
    }

    /// Production apply authorization reading the real `/proc/self/cgroup`.
    pub fn open(
        config: &SystemdExecution,
        release: &str,
        authentication: &VerifiedManifest,
    ) -> Result<Self> {
        config
            .validate()
            .context("validate full systemd execution config")?;
        let mapped = mapped_execution(config)?;
        check_systemd_boundaries(config)?;
        let activation = ActivationContext::open(&mapped, release, authentication)?;
        Self::finish(config.clone(), activation)
    }

    /// Explicit development apply authorization. The staged manifest must
    /// declare `development=true` and the caller must pass `true`; no
    /// [`VerifiedManifest`] is fabricated. The real entry point tolerates the
    /// legacy global Core/Panel units, matching activation's migration exception.
    pub fn open_development(
        config: &SystemdExecution,
        release: &str,
        allow_development: bool,
    ) -> Result<Self> {
        config
            .validate()
            .context("validate full systemd execution config")?;
        let mapped = mapped_execution(config)?;
        check_systemd_boundaries(config)?;
        let activation = ActivationContext::open_development(&mapped, release, allow_development)?;
        Self::finish(config.clone(), activation)
    }

    /// Deterministic development authorization with frozen cgroup text. The
    /// full production cgroup guard applies, including legacy unit rejection.
    pub fn open_development_with_cgroup(
        config: &SystemdExecution,
        release: &str,
        allow_development: bool,
        cgroup_text: &str,
    ) -> Result<Self> {
        config
            .validate()
            .context("validate full systemd execution config")?;
        let mapped = mapped_execution(config)?;
        check_systemd_boundaries(config)?;
        let activation = ActivationContext::open_development_with_cgroup(
            &mapped,
            release,
            allow_development,
            cgroup_text,
        )?;
        Self::finish(config.clone(), activation)
    }

    /// Recovery authorization without a live trust store. Trust is anchored in
    /// the durable transaction's manifest SHA-256 and the existing snapshot;
    /// this constructor never creates a snapshot and never accepts a trust flag.
    pub fn open_recovery(config: &SystemdExecution, transaction: &Transaction) -> Result<Self> {
        config
            .validate()
            .context("validate full systemd execution config")?;
        let mapped = mapped_execution(config)?;
        check_systemd_boundaries(config)?;
        let activation = ActivationContext::open_recovery(&mapped, transaction)?;
        Self::finish(config.clone(), activation)
    }

    /// Deterministic recovery authorization with frozen cgroup text.
    pub fn open_recovery_with_cgroup(
        config: &SystemdExecution,
        transaction: &Transaction,
        cgroup_text: &str,
    ) -> Result<Self> {
        config
            .validate()
            .context("validate full systemd execution config")?;
        let mapped = mapped_execution(config)?;
        check_systemd_boundaries(config)?;
        let activation =
            ActivationContext::open_recovery_with_cgroup(&mapped, transaction, cgroup_text)?;
        Self::finish(config.clone(), activation)
    }

    fn finish(config: SystemdExecution, activation: ActivationContext) -> Result<Self> {
        config.validate()?;
        let mapped = mapped_execution(&config)?;
        ensure!(
            activation.instance_id() == config.deployment.instance_id,
            "activation instance differs from the systemd deployment"
        );
        check_systemd_boundaries(&config)?;
        let control_adapter_sha256 = inspect_control_adapter(
            &config.control_adapter,
            Path::new(&config.deployment.store),
            &config.state_dir,
        )?;
        ensure!(
            control_adapter_sha256 == activation.adapter_sha256(),
            "control adapter changed during systemd authorization"
        );
        let canonical = canonical_systemd_config(&config)?;
        ensure!(
            canonical.len() as u64 <= MAX_SYSTEMD_CONFIG_BYTES,
            "full systemd config exceeds {MAX_SYSTEMD_CONFIG_BYTES} bytes"
        );
        let systemd_config_sha256 = hash_bytes(&canonical);
        let mapped_bytes =
            serde_json::to_vec(&mapped).context("serialize mapped execution config")?;
        ensure!(
            mapped_bytes.len() as u64 <= MAX_SYSTEMD_CONFIG_BYTES,
            "mapped execution config is unexpectedly large"
        );
        let mapped_config_sha256 = hash_bytes(&mapped_bytes);
        Ok(Self {
            instance_id: activation.instance_id().to_owned(),
            release_id: activation.release_id().to_owned(),
            manifest_sha256: activation.manifest_sha256().to_owned(),
            systemd_config_sha256,
            mapped_config_sha256,
            control_adapter_sha256,
            state_dir: config.state_dir.clone(),
            store_dir: PathBuf::from(&config.deployment.store),
            activation,
            systemd: config,
            mapped,
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
    /// SHA-256 of the complete original systemd document. This is the binding
    /// that covers `unit_directory`, `current_selector`, `core_unit`,
    /// `panel_unit`, `control_adapter`, timeouts and deployment together.
    pub fn systemd_config_sha256(&self) -> &str {
        &self.systemd_config_sha256
    }
    /// SHA-256 of the generic [`Execution`] projection. This is what the
    /// existing activation snapshot metadata records; it is not sufficient on
    /// its own.
    pub fn mapped_config_sha256(&self) -> &str {
        &self.mapped_config_sha256
    }
    /// SHA-256 of the administrator's trusted control adapter at authorization.
    pub fn control_adapter_sha256(&self) -> &str {
        &self.control_adapter_sha256
    }
    /// Administrator control adapter path used for authorization. This is the
    /// mutable input path; transaction execution must use
    /// [`SystemdSnapshot::pinned_systemd_execution`] instead.
    pub fn control_adapter(&self) -> &Path {
        &self.systemd.control_adapter
    }
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
    pub fn store_dir(&self) -> &Path {
        &self.store_dir
    }
    pub fn unit_directory(&self) -> &Path {
        &self.systemd.unit_directory
    }
    pub fn current_selector(&self) -> &Path {
        &self.systemd.current_selector
    }
    pub fn core_unit(&self) -> &str {
        &self.systemd.core_unit
    }
    pub fn panel_unit(&self) -> Option<&str> {
        self.systemd.panel_unit.as_deref()
    }
    pub fn timeout_seconds(&self) -> u64 {
        self.systemd.timeout_seconds
    }
    pub fn reversible_data(&self) -> bool {
        self.systemd.reversible_data
    }
    pub fn deployment(&self) -> &model::Deployment {
        &self.systemd.deployment
    }
    /// Frozen full systemd document (borrowed). It still names the mutable
    /// administrator adapter path; use
    /// [`SystemdSnapshot::pinned_systemd_execution`] on a prepared snapshot for
    /// transaction execution.
    pub fn systemd_execution(&self) -> &SystemdExecution {
        &self.systemd
    }
    /// Frozen full systemd document (explicit clone). It still names the mutable
    /// adapter path; use [`SystemdSnapshot::pinned_systemd_execution`] for
    /// execution.
    pub fn systemd_execution_clone(&self) -> SystemdExecution {
        self.systemd.clone()
    }
    /// Frozen generic projection (borrowed). It names the mutable adapter path;
    /// use [`SystemdSnapshot::pinned_execution`] for execution.
    pub fn mapped_execution(&self) -> &Execution {
        &self.mapped
    }
    /// Frozen generic projection (explicit clone). It names the mutable adapter
    /// path; use [`SystemdSnapshot::pinned_execution`] for execution.
    pub fn mapped_execution_clone(&self) -> Execution {
        self.mapped.clone()
    }
    /// Verified candidate directory for the authorized release.
    pub fn candidate_root(&self) -> PathBuf {
        self.activation.candidate_root()
    }
    /// Canonical Core binary inside the verified candidate.
    pub fn core_binary_path(&self) -> PathBuf {
        self.activation.core_binary_path()
    }
    /// Deterministic activation snapshot directory for this transaction.
    pub fn snapshot_path(&self, transaction: &Transaction) -> PathBuf {
        self.activation.snapshot_path(transaction)
    }
    /// Deterministic full-config evidence directory for this transaction.
    pub fn evidence_path(&self, transaction: &Transaction) -> PathBuf {
        self.state_dir
            .join(evidence_dir_name(&self.instance_id, &transaction.id))
    }

    /// Create or idempotently reopen the activation snapshot, then publish the
    /// durable full-systemd-config evidence bound to that exact snapshot.
    ///
    /// `raw_config` is the optional original execution JSON document. When
    /// supplied it must deserialize to the same [`SystemdExecution`] and its
    /// exact bytes are retained; when absent the canonical full document is
    /// retained. The adapter bytes are pinned by the activation snapshot, never
    /// copied from the candidate store.
    pub fn prepare_snapshot(
        &self,
        config: &SystemdExecution,
        transaction: &Transaction,
        raw_config: Option<&[u8]>,
    ) -> Result<SystemdSnapshot> {
        self.ensure_transaction(transaction)?;
        let canonical = self.ensure_frozen_config(config)?;
        let config_bytes = full_config_bytes(config, raw_config, &canonical)?;
        let snapshot = self
            .activation
            .prepare_snapshot(&self.mapped, transaction, None)?;
        let evidence =
            self.publish_evidence(transaction, &snapshot, config, &config_bytes, raw_config)?;
        self.wrap(snapshot, evidence, config)
    }

    /// Reopen a snapshot authorized by an authenticated manifest. Never creates
    /// a snapshot; the full systemd config and adapter hashes must still match.
    pub fn load_snapshot(
        &self,
        config: &SystemdExecution,
        transaction: &Transaction,
        authentication: &VerifiedManifest,
    ) -> Result<SystemdSnapshot> {
        self.ensure_transaction(transaction)?;
        self.ensure_frozen_config(config)?;
        let snapshot = self
            .activation
            .load_snapshot(&self.mapped, transaction, authentication)?;
        let evidence = self.read_evidence(transaction, &snapshot, config, None)?;
        self.wrap(snapshot, evidence, config)
    }

    /// Recovery without a live trust store. Only the durable transaction hash
    /// and the existing snapshot/evidence are accepted; a missing snapshot or a
    /// missing evidence directory is an error, never recreated here.
    pub fn load_snapshot_recovery(
        &self,
        config: &SystemdExecution,
        transaction: &Transaction,
    ) -> Result<SystemdSnapshot> {
        self.ensure_transaction(transaction)?;
        self.ensure_frozen_config(config)?;
        let snapshot = self
            .activation
            .load_snapshot_recovery(&self.mapped, transaction)?;
        let evidence = self.read_evidence(transaction, &snapshot, config, None)?;
        self.wrap(snapshot, evidence, config)
    }

    fn ensure_transaction(&self, transaction: &Transaction) -> Result<()> {
        transaction.validate()?;
        ensure!(
            transaction.instance_id == self.instance_id,
            "transaction belongs to another instance"
        );
        ensure!(
            transaction.target_release == self.release_id,
            "transaction target release differs from the activation context"
        );
        ensure!(
            transaction.manifest_sha256 == self.manifest_sha256,
            "transaction manifest differs from the activation context"
        );
        Ok(())
    }

    /// Compare the full document *before* validating it, so drifting a
    /// systemd-only field is reported as a hash mismatch rather than being
    /// masked by a later field-level check.
    fn ensure_frozen_config(&self, config: &SystemdExecution) -> Result<Vec<u8>> {
        let canonical = canonical_systemd_config(config)?;
        ensure!(
            canonical.len() as u64 <= MAX_SYSTEMD_CONFIG_BYTES,
            "full systemd config exceeds {MAX_SYSTEMD_CONFIG_BYTES} bytes"
        );
        ensure!(
            hash_bytes(&canonical) == self.systemd_config_sha256,
            "full systemd config changed after SystemdActivationContext::open"
        );
        config.validate()?;
        let mapped = mapped_execution(config)?;
        let mapped_bytes =
            serde_json::to_vec(&mapped).context("serialize mapped execution config")?;
        ensure!(
            hash_bytes(&mapped_bytes) == self.mapped_config_sha256,
            "mapped execution config changed after SystemdActivationContext::open"
        );
        Ok(canonical)
    }

    fn publish_evidence(
        &self,
        transaction: &Transaction,
        snapshot: &Snapshot,
        config: &SystemdExecution,
        config_bytes: &[u8],
        raw_config: Option<&[u8]>,
    ) -> Result<Evidence> {
        let state = open_private_state(&self.state_dir).context("state directory")?;
        let name = evidence_dir_name(&self.instance_id, &transaction.id);
        let meta = self.evidence_meta(transaction, snapshot, config_bytes)?;
        let meta_bytes =
            serde_json::to_vec(&meta).context("serialize systemd snapshot evidence")?;
        ensure!(
            meta_bytes.len() as u64 <= MAX_EVIDENCE_BYTES,
            "systemd snapshot evidence exceeds {MAX_EVIDENCE_BYTES} bytes"
        );
        if state.child(&name).is_ok() {
            return self.read_evidence(transaction, snapshot, config, raw_config);
        }
        let temp = unique_name(TEMP_PREFIX);
        let dir = state.mkdir(&temp)?;
        dir.write_new(SYSTEMD_CONFIG_FILE, config_bytes, 0o400)?;
        dir.write_new(EVIDENCE_FILE, &meta_bytes, 0o400)?;
        dir.seal()?;
        if let Err(error) = state.publish(&temp, &name) {
            if state.child(&name).is_ok() {
                return self
                    .read_evidence(transaction, snapshot, config, raw_config)
                    .with_context(|| format!("publish systemd snapshot evidence: {error}"));
            }
            return Err(error).context("publish systemd snapshot evidence");
        }
        let _published = state.child(&name)?;
        self.read_evidence(transaction, snapshot, config, raw_config)
    }

    fn evidence_meta(
        &self,
        transaction: &Transaction,
        snapshot: &Snapshot,
        config_bytes: &[u8],
    ) -> Result<EvidenceMeta> {
        ensure!(
            snapshot.release_id() == self.release_id,
            "snapshot release differs from the activation context"
        );
        ensure!(
            snapshot.manifest_sha256() == self.manifest_sha256,
            "snapshot manifest differs from the activation context"
        );
        ensure!(
            snapshot.config_sha256() == self.mapped_config_sha256,
            "snapshot mapped config differs from the activation context"
        );
        ensure!(
            snapshot.adapter_sha256() == self.control_adapter_sha256,
            "snapshot adapter differs from the trusted control adapter"
        );
        let adapter = std::fs::symlink_metadata(snapshot.adapter_path())
            .context("inspect pinned control adapter")?;
        ensure!(
            adapter.file_type().is_file(),
            "pinned control adapter must be a regular file"
        );
        ensure!(
            adapter.len() > 0 && adapter.len() <= MAX_ADAPTER_BYTES,
            "pinned control adapter size is invalid"
        );
        Ok(EvidenceMeta {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            instance_id: self.instance_id.clone(),
            transaction_id: transaction.id.clone(),
            release_id: self.release_id.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
            systemd_config_sha256: self.systemd_config_sha256.clone(),
            mapped_config_sha256: self.mapped_config_sha256.clone(),
            config_file_sha256: hash_bytes(config_bytes),
            adapter_sha256: self.control_adapter_sha256.clone(),
            adapter_bytes: adapter.len(),
            snapshot_digest: snapshot.digest().to_owned(),
        })
    }

    fn read_evidence(
        &self,
        transaction: &Transaction,
        snapshot: &Snapshot,
        config: &SystemdExecution,
        raw_config: Option<&[u8]>,
    ) -> Result<Evidence> {
        let path = self.evidence_path(transaction);
        let dir = open_evidence_dir(&path)?;
        let meta_bytes = read_bounded(&dir, EVIDENCE_FILE, MAX_EVIDENCE_BYTES)?;
        let meta: EvidenceMeta =
            serde_json::from_slice(&meta_bytes).context("parse systemd snapshot evidence")?;
        meta.validate()?;
        ensure!(
            meta.instance_id == self.instance_id
                && meta.transaction_id == transaction.id
                && meta.release_id == self.release_id
                && meta.manifest_sha256 == self.manifest_sha256,
            "systemd snapshot evidence identity differs from the activation context"
        );
        ensure!(
            meta.systemd_config_sha256 == self.systemd_config_sha256,
            "full systemd config differs from the pinned snapshot"
        );
        ensure!(
            meta.mapped_config_sha256 == self.mapped_config_sha256
                && meta.mapped_config_sha256 == snapshot.config_sha256(),
            "mapped execution config differs from the pinned snapshot"
        );
        ensure!(
            meta.adapter_sha256 == self.control_adapter_sha256
                && meta.adapter_sha256 == snapshot.adapter_sha256(),
            "trusted control adapter differs from the pinned snapshot"
        );
        ensure!(
            meta.snapshot_digest == snapshot.digest(),
            "systemd snapshot evidence does not match the activation snapshot"
        );
        let config_bytes = read_bounded(&dir, SYSTEMD_CONFIG_FILE, MAX_SYSTEMD_CONFIG_BYTES)?;
        ensure!(
            hash_bytes(&config_bytes) == meta.config_file_sha256,
            "pinned systemd config was modified"
        );
        let frozen: SystemdExecution =
            serde_json::from_slice(&config_bytes).context("parse pinned full systemd config")?;
        let frozen_sha256 = systemd_config_sha256(&frozen)?;
        ensure!(
            frozen_sha256 == meta.systemd_config_sha256
                && frozen_sha256 == self.systemd_config_sha256,
            "pinned full systemd config hash mismatch"
        );
        ensure!(
            serde_json::to_value(&frozen)? == serde_json::to_value(config)?,
            "pinned full systemd config differs from the supplied config"
        );
        if let Some(raw) = raw_config {
            ensure!(
                hash_bytes(raw) == meta.config_file_sha256,
                "raw systemd config differs from the pinned snapshot"
            );
        }
        let adapter = std::fs::symlink_metadata(snapshot.adapter_path())
            .context("inspect pinned control adapter")?;
        ensure!(
            adapter.file_type().is_file()
                && adapter.len() == meta.adapter_bytes
                && adapter.len() > 0,
            "pinned control adapter size differs from the evidence"
        );
        Ok(Evidence {
            dir: path,
            digest: hash_bytes(&meta_bytes),
            meta,
        })
    }

    fn wrap(
        &self,
        snapshot: Snapshot,
        evidence: Evidence,
        config: &SystemdExecution,
    ) -> Result<SystemdSnapshot> {
        ensure!(
            evidence.meta.systemd_config_sha256 == self.systemd_config_sha256,
            "evidence full config differs from the activation context"
        );
        ensure!(
            serde_json::to_value(config)? == serde_json::to_value(&self.systemd)?,
            "supplied full systemd config differs from the frozen context"
        );
        Ok(SystemdSnapshot {
            snapshot,
            evidence_dir: evidence.dir,
            evidence_digest: evidence.digest,
            meta: evidence.meta,
            systemd: self.systemd.clone(),
            mapped: self.mapped.clone(),
            systemd_config_sha256: self.systemd_config_sha256.clone(),
            mapped_config_sha256: self.mapped_config_sha256.clone(),
        })
    }
}

/// Immutable pinned systemd snapshot returned to `main`.
#[derive(Debug, Clone)]
pub struct SystemdSnapshot {
    snapshot: Snapshot,
    evidence_dir: PathBuf,
    evidence_digest: String,
    meta: EvidenceMeta,
    systemd: SystemdExecution,
    mapped: Execution,
    systemd_config_sha256: String,
    mapped_config_sha256: String,
}

impl SystemdSnapshot {
    /// Activation snapshot directory (pinned `execution.json` + `adapter`).
    pub fn path(&self) -> &Path {
        self.snapshot.path()
    }
    /// SHA-256 of the exact activation `snapshot.json` bytes.
    pub fn digest(&self) -> &str {
        self.snapshot.digest()
    }
    /// Sealed directory holding the original full systemd config and hashes.
    pub fn evidence_path(&self) -> &Path {
        &self.evidence_dir
    }
    /// SHA-256 of the exact evidence `evidence.json` bytes.
    pub fn evidence_digest(&self) -> &str {
        &self.evidence_digest
    }
    /// Path of the original full systemd config bytes.
    pub fn systemd_config_path(&self) -> PathBuf {
        self.evidence_dir.join(SYSTEMD_CONFIG_FILE)
    }
    /// Path of the binding-hash document.
    pub fn evidence_file_path(&self) -> PathBuf {
        self.evidence_dir.join(EVIDENCE_FILE)
    }
    /// Path of the pinned control adapter bytes (inside the activation snapshot).
    pub fn adapter_path(&self) -> PathBuf {
        self.snapshot.adapter_path()
    }
    /// Path of the pinned mapped `execution.json`.
    pub fn config_path(&self) -> PathBuf {
        self.snapshot.config_path()
    }
    pub fn instance_id(&self) -> &str {
        &self.meta.instance_id
    }
    pub fn transaction_id(&self) -> &str {
        &self.meta.transaction_id
    }
    pub fn release_id(&self) -> &str {
        self.snapshot.release_id()
    }
    pub fn manifest_sha256(&self) -> &str {
        self.snapshot.manifest_sha256()
    }
    /// SHA-256 of the pinned mapped [`Execution`] document.
    pub fn config_sha256(&self) -> &str {
        self.snapshot.config_sha256()
    }
    /// SHA-256 of the pinned complete systemd document.
    pub fn systemd_config_sha256(&self) -> &str {
        &self.systemd_config_sha256
    }
    /// SHA-256 of the pinned mapped [`Execution`] document (alias).
    pub fn mapped_config_sha256(&self) -> &str {
        &self.mapped_config_sha256
    }
    /// SHA-256 of the pinned control adapter bytes.
    pub fn adapter_sha256(&self) -> &str {
        self.snapshot.adapter_sha256()
    }
    /// SHA-256 of the exact stored full-config bytes (may be the original
    /// document formatting when `prepare_snapshot` was given `raw_config`).
    pub fn systemd_config_file_sha256(&self) -> &str {
        &self.meta.config_file_sha256
    }
    /// Frozen full systemd document (borrowed). It still names the mutable
    /// administrator adapter path; use [`Self::pinned_systemd_execution`] on a
    /// prepared snapshot for transaction execution.
    pub fn systemd_execution(&self) -> &SystemdExecution {
        &self.systemd
    }
    /// Frozen full systemd document (explicit clone). It still names the mutable
    /// adapter path; use [`Self::pinned_systemd_execution`] for execution.
    pub fn systemd_execution_clone(&self) -> SystemdExecution {
        self.systemd.clone()
    }
    /// Frozen generic projection (borrowed). It names the mutable adapter path;
    /// use [`Self::pinned_execution`] for execution.
    pub fn execution(&self) -> &Execution {
        &self.mapped
    }
    /// Frozen generic projection (explicit clone). It names the mutable adapter
    /// path; use [`Self::pinned_execution`] for execution.
    pub fn execution_clone(&self) -> Execution {
        self.mapped.clone()
    }
    /// The underlying activation snapshot.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
    /// Mapped execution with `adapter` redirected to the pinned copy.
    pub fn pinned_execution(&self) -> Result<Execution> {
        self.snapshot.pinned_execution(&self.mapped)
    }

    /// Clone of the frozen full systemd document whose `control_adapter` is the
    /// immutable pinned copy. Re-reads the durable evidence first, so a modified
    /// or removed evidence directory fails closed instead of executing a
    /// mutable administrator path.
    pub fn pinned_systemd_execution(&self) -> Result<SystemdExecution> {
        let dir = open_evidence_dir(&self.evidence_dir)?;
        let meta_bytes = read_bounded(&dir, EVIDENCE_FILE, MAX_EVIDENCE_BYTES)?;
        ensure!(
            hash_bytes(&meta_bytes) == self.evidence_digest,
            "systemd snapshot evidence changed after it was loaded"
        );
        let meta: EvidenceMeta =
            serde_json::from_slice(&meta_bytes).context("parse systemd snapshot evidence")?;
        meta.validate()?;
        ensure!(
            meta == self.meta,
            "systemd snapshot evidence metadata changed after it was loaded"
        );
        let config_bytes = read_bounded(&dir, SYSTEMD_CONFIG_FILE, MAX_SYSTEMD_CONFIG_BYTES)?;
        ensure!(
            hash_bytes(&config_bytes) == meta.config_file_sha256,
            "pinned systemd config was modified"
        );
        let frozen: SystemdExecution =
            serde_json::from_slice(&config_bytes).context("parse pinned full systemd config")?;
        ensure!(
            systemd_config_sha256(&frozen)? == self.systemd_config_sha256,
            "pinned full systemd config hash mismatch"
        );
        ensure!(
            serde_json::to_value(&frozen)? == serde_json::to_value(&self.systemd)?,
            "pinned full systemd config differs from the frozen context"
        );
        let mapped = self.snapshot.pinned_execution(&self.mapped)?;
        let mut pinned = self.systemd.clone();
        pinned.control_adapter = mapped.adapter;
        pinned.validate()?;
        let mut check = pinned.clone();
        check.control_adapter = self.systemd.control_adapter.clone();
        ensure!(
            systemd_config_sha256(&check)? == self.systemd_config_sha256,
            "pinned systemd config drifted from the frozen full config"
        );
        Ok(pinned)
    }

    /// Real backend over the pinned adapter. Construction verifies the adapter
    /// file but never invokes `systemctl` or the adapter.
    pub fn create_backend(&self) -> Result<SystemdBackend<ExecTransport>> {
        self.pinned_systemd_execution()?.create_backend()
    }

    /// Real transaction backend over the pinned adapter.
    pub fn create_transaction_backend(&self) -> Result<SystemdTransactionBackend<ExecTransport>> {
        self.pinned_systemd_execution()?
            .create_transaction_backend()
    }
}

/// Durable binding document. It never contains the adapter bytes; those live
/// only in the activation snapshot, which was copied from the trusted
/// administrator path and never from the candidate store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceMeta {
    schema_version: u32,
    instance_id: String,
    transaction_id: String,
    release_id: String,
    manifest_sha256: String,
    systemd_config_sha256: String,
    mapped_config_sha256: String,
    config_file_sha256: String,
    adapter_sha256: String,
    adapter_bytes: u64,
    snapshot_digest: String,
}

impl EvidenceMeta {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == EVIDENCE_SCHEMA_VERSION,
            "unsupported systemd snapshot evidence schema"
        );
        model::safe_id(&self.instance_id)?;
        model::safe_id(&self.transaction_id)?;
        model::safe_id(&self.release_id)?;
        for digest in [
            &self.manifest_sha256,
            &self.systemd_config_sha256,
            &self.mapped_config_sha256,
            &self.config_file_sha256,
            &self.adapter_sha256,
            &self.snapshot_digest,
        ] {
            model::digest(digest)?;
        }
        ensure!(
            self.adapter_bytes > 0 && self.adapter_bytes <= MAX_ADAPTER_BYTES,
            "systemd snapshot evidence adapter size is invalid"
        );
        Ok(())
    }
}

struct Evidence {
    dir: PathBuf,
    digest: String,
    meta: EvidenceMeta,
}

fn full_config_bytes(
    config: &SystemdExecution,
    raw: Option<&[u8]>,
    canonical: &[u8],
) -> Result<Vec<u8>> {
    ensure!(
        canonical.len() as u64 <= MAX_SYSTEMD_CONFIG_BYTES,
        "full systemd config exceeds {MAX_SYSTEMD_CONFIG_BYTES} bytes"
    );
    let Some(bytes) = raw else {
        return Ok(canonical.to_vec());
    };
    ensure!(
        bytes.len() as u64 <= MAX_SYSTEMD_CONFIG_BYTES,
        "raw systemd config exceeds {MAX_SYSTEMD_CONFIG_BYTES} bytes"
    );
    let parsed: SystemdExecution =
        serde_json::from_slice(bytes).context("parse raw full systemd config")?;
    ensure!(
        serde_json::to_value(&parsed)? == serde_json::to_value(config)?,
        "raw systemd config does not match the SystemdExecution struct"
    );
    Ok(bytes.to_vec())
}

fn check_systemd_boundaries(config: &SystemdExecution) -> Result<()> {
    let store_path = Path::new(&config.deployment.store);
    let _state = open_private_state(&config.state_dir).context("state directory")?;
    let store = Dir::absolute(store_path).context("open candidate store (no symlink ancestors)")?;
    safe_fs::private_store(&store).context("candidate store")?;
    ensure!(
        !within(&config.state_dir, store_path) && !within(store_path, &config.state_dir),
        "state directory and candidate store must not nest in either direction"
    );
    ensure!(
        !within(&config.unit_directory, store_path),
        "unit_directory must stay outside the candidate store"
    );
    ensure!(
        !within(&config.current_selector, store_path),
        "current_selector must stay outside the candidate store"
    );
    ensure!(
        !within(&config.control_adapter, store_path),
        "control_adapter must not come from the candidate store"
    );
    ensure!(
        !within(&config.control_adapter, &config.state_dir),
        "control_adapter must not live inside the transaction state directory"
    );
    Ok(())
}

fn open_private_state(path: &Path) -> Result<Dir> {
    model::absolute_path(path)?;
    let dir = Dir::absolute(path).context("open state directory (no symlink ancestors)")?;
    let meta = dir.0.metadata()?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    ensure!(meta.is_dir(), "state path is not a directory");
    ensure!(
        meta.uid() == euid,
        "state directory must be owned by the caller"
    );
    ensure!(
        (meta.mode() & 0o077) == 0 && (meta.mode() & 0o700) == 0o700,
        "state directory must be private (0700)"
    );
    Ok(dir)
}

fn open_evidence_dir(path: &Path) -> Result<Dir> {
    model::absolute_path(path)?;
    let dir =
        Dir::absolute(path).context("open systemd snapshot evidence (no symlink ancestors)")?;
    let meta = dir.0.metadata()?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    ensure!(
        meta.is_dir(),
        "systemd snapshot evidence path is not a directory"
    );
    ensure!(
        meta.uid() == euid,
        "systemd snapshot evidence must be owned by the caller"
    );
    ensure!(
        (meta.mode() & 0o022) == 0,
        "systemd snapshot evidence must not be group/other writable"
    );
    Ok(dir)
}

/// Inspect the administrator's trusted adapter without following symlinks and
/// reject candidate-store or state-directory copies. The candidate store is
/// never an adapter source, even for a same-UID file.
fn inspect_control_adapter(path: &Path, store: &Path, state: &Path) -> Result<String> {
    ensure!(
        !within(path, store),
        "control_adapter must not come from the candidate store"
    );
    ensure!(
        !within(path, state),
        "control_adapter must not live inside the transaction state directory"
    );
    model::absolute_path(path)?;
    let parent = path.parent().context("control_adapter has no parent")?;
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("control_adapter path must be UTF-8")?;
    let mut file = Dir::absolute(parent)
        .context("open control_adapter directory (no symlink ancestors)")?
        .open_file(leaf, false)?;
    let meta = file.metadata()?;
    ensure!(
        meta.nlink() == 1,
        "control_adapter must have exactly one link"
    );
    ensure!(meta.len() > 0, "control_adapter must not be empty");
    ensure!(
        (meta.mode() & 0o022) == 0,
        "control_adapter must not be group/other writable"
    );
    ensure!(
        (meta.mode() & 0o111) != 0,
        "control_adapter must be executable"
    );
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    ensure!(
        meta.uid() == euid || meta.uid() == 0,
        "control_adapter must be owned by the caller or root"
    );
    hash_reader(&mut file, MAX_ADAPTER_BYTES)
}

fn read_bounded(dir: &Dir, name: &str, max: u64) -> Result<Vec<u8>> {
    let mut file = dir
        .open_file(name, false)
        .with_context(|| format!("open {name}"))?;
    let meta = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    ensure!(meta.uid() == euid, "{name} must be owned by the caller");
    ensure!(
        (meta.mode() & 0o022) == 0,
        "{name} must not be group/other writable"
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "{name} exceeds {max} bytes");
    Ok(bytes)
}

fn hash_reader<R: Read>(reader: &mut R, max: u64) -> Result<String> {
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
    Ok(format!("{:x}", hasher.finalize()))
}

fn evidence_dir_name(instance_id: &str, transaction_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(instance_id.as_bytes());
    hasher.update([0]);
    hasher.update(transaction_id.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    // Deterministic for recovery and short enough for `model::safe_id`.
    format!("systemd-snapshot-{}", &digest[..32])
}

fn unique_name(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{nanos}-{counter}", std::process::id())
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn within(path: &Path, ancestor: &Path) -> bool {
    path == ancestor || path.starts_with(ancestor)
}
