//! Durable deployment transaction state machine.
//!
//! This module persists a deployment transaction before every side effect and
//! only records success after the real backend has been observed. It never
//! controls services itself: all service interaction goes through [`Backend`].
//!
//! # Durability contract
//!
//! A transaction is stored in one explicit state directory as a single JSON
//! document. Each update is written to a new `0600` temporary file, `fsync`ed,
//! atomically renamed over the previous document, and the directory is
//! `fsync`ed. Therefore a crash leaves either the old or the new complete
//! document, never a partially written one.
//!
//! [`Phase`] is the durable *intent*: it is persisted before the corresponding
//! backend side effect runs. If the process crashes while a step is in flight,
//! [`TransactionStore::recover`] observes the real backend and resolves the
//! ambiguity. Evidence is appended after an operation returns.
//!
//! # Commit boundary
//!
//! `Committed` is persisted only after the target release is observed ready.
//! After that boundary a maintenance-release failure is recorded as
//! [`Phase::RecoveryRequired`] but the served data is **never** automatically
//! rolled back: recovery retries only the idempotent maintenance release.
//! Before the boundary, recovery always verifies and performs an idempotent
//! rollback to `previous_release`.
//!
//! # Locking and state directory
//!
//! The caller must pass an explicit absolute, private (`0700`, owned by the
//! current euid) state directory and must hold whatever exclusive store lock
//! the surrounding deployment store defines. This module deliberately does not
//! default to `$HOME`, does not create the directory, and does not acquire a
//! lock of its own. One transaction document exists per instance:
//! `transaction-<instance_id>.json`.

use crate::{model, safe_fs::Dir};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::CString,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// Current durable transaction schema version.
pub const TRANSACTION_SCHEMA_VERSION: u32 = 1;

/// Maximum size of one transaction document accepted from disk.
const MAX_TRANSACTION_BYTES: u64 = 1024 * 1024;

/// Maximum number of evidence records in one transaction. Kept small enough
/// that a document full of maximum-length details still fits the 1 MiB bound.
const MAX_STEPS: usize = 256;

/// Maximum length (in characters) of a persisted human-readable message.
const MAX_MESSAGE_CHARS: usize = 2048;

/// Durable phase of a deployment transaction.
///
/// The phase is the intent that was made durable *before* the matching backend
/// side effect. Consequently a crashed transaction can be interpreted as "the
/// step named by this phase may or may not have reached the backend"; recovery
/// must query [`Backend::observe`] instead of assuming either outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Initial state; the candidate release may be prepared out of band.
    Prepared,
    /// The instance is (or is about to be) in maintenance.
    Maintenance,
    /// Checked drain is (or is about to be) performed.
    Drained,
    /// The active release pointer is (or is about to be) switched.
    Switching,
    /// The target is resumed and readiness is (or is about to be) verified.
    Verifying,
    /// Commit boundary: target readiness was observed. Served data must not be
    /// automatically rolled back from this point on.
    Committed,
    /// An idempotent rollback is in progress.
    RollingBack,
    /// Rollback completed and was verified against the backend.
    RolledBack,
    /// Automatic recovery could not prove a safe terminal state. A human or a
    /// later recovery attempt is required; this phase is **not** terminal.
    RecoveryRequired,
}

impl Phase {
    /// Whether no further work is expected for this transaction.
    ///
    /// `RecoveryRequired` is intentionally not terminal: [`TransactionStore::recover`]
    /// may still resolve it.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::RolledBack)
    }

    /// Whether the transaction crossed the commit boundary.
    pub fn is_committed(self) -> bool {
        matches!(self, Self::Committed)
    }
}

/// A step that may be recorded in [`StepEvidence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Preflight observation of the real backend.
    Preflight,
    /// Candidate release preparation.
    Prepare,
    /// Maintenance entry.
    EnterMaintenance,
    /// Checked drain.
    Drain,
    /// Active release switch.
    Switch,
    /// Resume of the target release.
    Resume,
    /// Readiness verification.
    VerifyReadiness,
    /// Commit boundary checkpoint.
    Commit,
    /// Maintenance release (traffic restore).
    ReleaseMaintenance,
    /// Idempotent restore of the previous release.
    Restore,
    /// Readiness check of the restored previous release.
    RestoreReadiness,
    /// Final rollback verification against the backend.
    RollbackVerify,
    /// Recovery attempt (crash or `RecoveryRequired`).
    Recover,
    /// The transaction was found interrupted during recovery.
    Interrupted,
}

/// Outcome of a recorded step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// The operation completed and was observed as successful.
    Succeeded,
    /// The operation failed. The transaction must not claim success.
    Failed,
    /// The operation was unnecessary because the backend already reflected it.
    Skipped,
}

/// Machine-readable failure class stored in [`TransactionError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The backend changed after the transaction was begun.
    PreflightDrift,
    /// Candidate preparation failed.
    PrepareFailed,
    /// Maintenance entry failed.
    MaintenanceFailed,
    /// Checked drain did not complete.
    DrainIncomplete,
    /// Release switch failed.
    SwitchFailed,
    /// Target resume failed.
    ResumeFailed,
    /// Target readiness failed (probe or observation gate).
    ReadinessFailed,
    /// The commit is durable but maintenance could not be released.
    CommitMaintenanceReleaseFailed,
    /// The commit is durable but the post-commit backend observation failed.
    /// Recovery must retry observation/maintenance release, never roll back.
    CommitObservationFailed,
    /// Idempotent restore of the previous release failed.
    RollbackRestoreFailed,
    /// The restored previous release was not observed ready.
    RollbackReadinessFailed,
    /// Maintenance could not be released during rollback.
    RollbackMaintenanceReleaseFailed,
    /// The real backend could not be observed.
    ObservationFailed,
    /// The transaction was interrupted by a crash or abandoned process.
    Interrupted,
    /// A committed transaction's target is no longer active.
    CommittedStateDrift,
}

/// Persisted failure information. `code` is stable for automation, `message` is
/// for humans and is truncated to a bounded length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransactionError {
    /// Stable machine-readable class.
    pub code: ErrorCode,
    /// Human-readable detail (non-empty, bounded).
    pub message: String,
    /// Step whose failure caused this error, when applicable.
    pub step: Option<StepKind>,
}

impl TransactionError {
    /// Build an error, truncating an over-long message to a safe bound.
    pub fn new(code: ErrorCode, step: Option<StepKind>, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.trim().is_empty() {
            message = "backend operation failed".into();
        }
        let message = bounded_message(message);
        Self {
            code,
            message,
            step,
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.message.trim().is_empty(),
            "transaction error message must not be empty"
        );
        ensure!(
            self.message.chars().count() <= MAX_MESSAGE_CHARS,
            "transaction error message too long"
        );
        Ok(())
    }
}

/// Append-only audit evidence for one transaction step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepEvidence {
    /// Consecutive, one-based sequence number.
    pub sequence: u64,
    /// The step this evidence belongs to.
    pub step: StepKind,
    /// What happened.
    pub outcome: StepOutcome,
    /// Wall-clock observation time in Unix milliseconds (audit only).
    pub at_unix_ms: u64,
    /// Optional bounded detail.
    pub detail: Option<String>,
}

impl StepEvidence {
    fn validate(&self) -> Result<()> {
        ensure!(self.sequence > 0, "step sequence must be one-based");
        if let Some(detail) = &self.detail {
            ensure!(
                detail.chars().count() <= MAX_MESSAGE_CHARS,
                "step detail too long"
            );
        }
        Ok(())
    }
}

/// Strict persisted deployment transaction.
///
/// Unknown fields are rejected by serde. [`Transaction::validate`] additionally
/// enforces schema version, identifier/path safety, digest format, consecutive
/// evidence sequences, and phase/error invariants. Both `previous_release` and
/// `error` fields must be present in the JSON document (a JSON `null` is the
/// explicit empty value); a missing field is rejected rather than defaulted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Transaction {
    /// Durable schema version; must equal [`TRANSACTION_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Caller-supplied unique transaction identifier.
    pub id: String,
    /// Deployment instance this transaction belongs to.
    pub instance_id: String,
    /// Release active before this transaction, if any.
    pub previous_release: Option<String>,
    /// Release this transaction wants to activate.
    pub target_release: String,
    /// SHA-256 of the verified release manifest.
    pub manifest_sha256: String,
    /// Durable intent / completed state.
    pub phase: Phase,
    /// Failure detail; required for `RollingBack`, `RolledBack`,
    /// `RecoveryRequired`, and forbidden for healthy in-flight phases and
    /// `Committed`.
    pub error: Option<TransactionError>,
    /// Step evidence in append order.
    pub steps: Vec<StepEvidence>,
}

/// Deserialization mirror. Presence of `previous_release` and `error` is
/// checked by [`Transaction::deserialize`] before this mirror is used, because
/// serde's derive treats a missing `Option` field as `None`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionDocument {
    schema_version: u32,
    id: String,
    instance_id: String,
    previous_release: Option<String>,
    target_release: String,
    manifest_sha256: String,
    phase: Phase,
    error: Option<TransactionError>,
    steps: Vec<StepEvidence>,
}

impl<'de> Deserialize<'de> for Transaction {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        const REQUIRED: [&str; 9] = [
            "schema_version",
            "id",
            "instance_id",
            "previous_release",
            "target_release",
            "manifest_sha256",
            "phase",
            "error",
            "steps",
        ];
        let value = serde_json::Value::deserialize(deserializer).map_err(D::Error::custom)?;
        let object = value
            .as_object()
            .ok_or_else(|| D::Error::custom("transaction document must be a JSON object"))?;
        for field in REQUIRED {
            if !object.contains_key(field) {
                return Err(D::Error::custom(format!("missing field `{field}`")));
            }
        }
        let document: TransactionDocument =
            serde_json::from_value(value).map_err(D::Error::custom)?;
        Ok(Self {
            schema_version: document.schema_version,
            id: document.id,
            instance_id: document.instance_id,
            previous_release: document.previous_release,
            target_release: document.target_release,
            manifest_sha256: document.manifest_sha256,
            phase: document.phase,
            error: document.error,
            steps: document.steps,
        })
    }
}
impl Transaction {
    /// Validate all persisted invariants. This is called after every load and
    /// before every save.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == TRANSACTION_SCHEMA_VERSION,
            "unsupported transaction schema"
        );
        model::safe_id(&self.id)?;
        model::safe_id(&self.instance_id)?;
        validate_release_id(&self.target_release)?;
        if let Some(previous) = &self.previous_release {
            validate_release_id(previous)?;
            ensure!(
                previous != &self.target_release,
                "previous and target release are identical"
            );
        }
        model::digest(&self.manifest_sha256)?;
        ensure!(self.steps.len() <= MAX_STEPS, "too many transaction steps");
        for (index, step) in self.steps.iter().enumerate() {
            ensure!(
                step.sequence == index as u64 + 1,
                "step evidence sequence must be consecutive"
            );
            step.validate()?;
        }
        if let Some(error) = &self.error {
            error.validate()?;
        }
        match self.phase {
            Phase::Committed => ensure!(
                self.error.is_none(),
                "committed transaction must not carry an error"
            ),
            Phase::RollingBack | Phase::RolledBack | Phase::RecoveryRequired => ensure!(
                self.error.is_some(),
                "phase {:?} requires an error",
                self.phase
            ),
            Phase::Prepared
            | Phase::Maintenance
            | Phase::Drained
            | Phase::Switching
            | Phase::Verifying => ensure!(
                self.error.is_none(),
                "in-flight phase {:?} must not carry an error",
                self.phase
            ),
        }
        Ok(())
    }

    fn push_step(&mut self, step: StepKind, outcome: StepOutcome, detail: Option<String>) {
        let sequence = self.steps.last().map_or(1, |last| last.sequence + 1);
        self.steps.push(StepEvidence {
            sequence,
            step,
            outcome,
            at_unix_ms: now_unix_millis(),
            detail,
        });
    }
}

/// Result of a checked drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drain {
    /// No active work remains; switching is safe.
    Drained,
    /// Work remains. The transaction must roll back; the drain is never forced.
    Busy {
        /// Number of work items the backend still observes.
        remaining: u32,
    },
}

/// Point-in-time observation of the real backend.
///
/// [`Backend::observe`] must be a read-only query of the actual service state,
/// not of the desired state. Crash recovery and rollback verification rely on
/// this being truthful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// Release currently active according to the backend.
    pub current_release: Option<String>,
    /// Whether maintenance mode is currently active.
    pub maintenance: bool,
    /// Whether the active release is currently ready to serve.
    pub ready: bool,
    /// Number of active work items observed by the backend (0 means drained).
    pub active_work: u32,
}

impl Observation {
    /// Convenience constructor for backend implementations.
    pub fn new(
        current_release: Option<String>,
        maintenance: bool,
        ready: bool,
        active_work: u32,
    ) -> Self {
        Self {
            current_release,
            maintenance,
            ready,
            active_work,
        }
    }

    /// Whether the backend reports no active work.
    pub fn is_drained(&self) -> bool {
        self.active_work == 0
    }
}

/// All service interaction required by the transaction state machine.
///
/// Implementations must be truthful and idempotent: recovery may call
/// `restore`, `release_maintenance`, and `observe` again after a crash. A
/// method returning `Ok(())` is treated as a completed side effect and is
/// re-checked with [`Backend::observe`] where the state machine requires it.
/// Implementations must never silently ignore a failed operation.
pub trait Backend {
    /// Read-only query of the real service. Must not mutate state.
    fn observe(&mut self, instance_id: &str) -> Result<Observation>;

    /// Prepare/stage the candidate release without affecting live traffic.
    /// Idempotent.
    fn prepare(&mut self, transaction: &Transaction) -> Result<()>;

    /// Enter maintenance. Idempotent. Must fail rather than pretend success if
    /// maintenance could not be established.
    fn enter_maintenance(&mut self, transaction: &Transaction) -> Result<()>;

    /// Perform a checked drain. Must not force-kill work. Return
    /// [`Drain::Busy`] when active work remains.
    fn drain(&mut self, transaction: &Transaction) -> Result<Drain>;

    /// Switch the active release pointer to `target_release`. Idempotent.
    fn switch(&mut self, transaction: &Transaction) -> Result<()>;

    /// Resume/start the target release while still in maintenance. Idempotent.
    fn resume(&mut self, transaction: &Transaction) -> Result<()>;

    /// Probe readiness of the target release. Must return an error unless the
    /// real service is ready to serve.
    fn verify_ready(&mut self, transaction: &Transaction) -> Result<()>;

    /// Release maintenance / restore traffic. Idempotent. Used after commit and
    /// during rollback (only after the previous release is ready).
    fn release_maintenance(&mut self, transaction: &Transaction) -> Result<()>;

    /// Idempotently restore `previous_release`, or stop the instance when
    /// `previous_release` is `None`, and bring the restored release to a ready
    /// state. Must not touch `target_release` data after a committed
    /// transaction (recovery will not call it then).
    fn restore(&mut self, transaction: &Transaction) -> Result<()>;
}

/// Specification for beginning a transaction.
///
/// `previous_release` is intentionally absent: [`TransactionStore::begin`]
/// derives it from the real backend so the rollback target cannot be forged by
/// a caller.
#[derive(Debug, Clone)]
pub struct BeginSpec {
    /// Unique transaction identifier.
    pub transaction_id: String,
    /// Deployment instance.
    pub instance_id: String,
    /// Release to activate.
    pub target_release: String,
    /// SHA-256 of the verified release manifest.
    pub manifest_sha256: String,
}

impl BeginSpec {
    /// Construct a specification.
    pub fn new(
        transaction_id: impl Into<String>,
        instance_id: impl Into<String>,
        target_release: impl Into<String>,
        manifest_sha256: impl Into<String>,
    ) -> Self {
        Self {
            transaction_id: transaction_id.into(),
            instance_id: instance_id.into(),
            target_release: target_release.into(),
            manifest_sha256: manifest_sha256.into(),
        }
    }
}

/// Terminal or actionable outcome of a transaction drive/recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// Target release is active and maintenance was released.
    Committed,
    /// Previous release was restored, observed ready, and maintenance released.
    RolledBack,
    /// The state machine could not prove a safe terminal state. Inspect the
    /// transaction error and retry [`TransactionStore::recover`] or intervene.
    RecoveryRequired,
}

impl RunOutcome {
    /// Corresponding durable phase.
    pub fn phase(self) -> Phase {
        match self {
            Self::Committed => Phase::Committed,
            Self::RolledBack => Phase::RolledBack,
            Self::RecoveryRequired => Phase::RecoveryRequired,
        }
    }
}

/// Durable, per-instance transaction store.
///
/// The directory is supplied explicitly by the caller and must already exist,
/// be an absolute normal path, be owned by the current euid, and have private
/// `0700` permissions. The caller must hold the deployment store's exclusive
/// lock; this type performs no locking.
pub struct TransactionStore {
    dir: Dir,
    state_dir: PathBuf,
}

impl TransactionStore {
    /// Open an existing private state directory.
    ///
    /// Rejects relative paths, missing paths, symlinked components, files,
    /// non-private modes, and directories not owned by the current euid.
    pub fn open(state_dir: &Path) -> Result<Self> {
        model::absolute_path(state_dir)
            .context("transaction state directory must be an explicit absolute path")?;
        let dir = Dir::absolute(state_dir)
            .with_context(|| format!("open transaction state directory {}", state_dir.display()))?;
        let metadata = dir.0.metadata()?;
        ensure!(
            metadata.is_dir(),
            "transaction state path is not a directory"
        );
        // SAFETY: geteuid has no preconditions.
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "transaction state directory must be owned by the current user"
        );
        ensure!(
            metadata.mode() & 0o077 == 0 && metadata.mode() & 0o700 == 0o700,
            "transaction state directory must be private (0700)"
        );
        Ok(Self {
            dir,
            state_dir: state_dir.to_path_buf(),
        })
    }

    /// The explicit state directory this store was opened with.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// List instance identifiers that have a persisted transaction document.
    /// Corrupt or non-matching file names are ignored here; [`Self::load`]
    /// still fails closed if a selected document is invalid.
    pub fn instances(&self) -> Result<Vec<String>> {
        let mut instances = BTreeSet::new();
        for entry in std::fs::read_dir(&self.state_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(rest) = name.strip_prefix("transaction-") else {
                continue;
            };
            let Some(instance) = rest.strip_suffix(".json") else {
                continue;
            };
            if model::safe_id(instance).is_ok() {
                instances.insert(instance.to_owned());
            }
        }
        Ok(instances.into_iter().collect())
    }

    /// Load and validate the transaction for `instance_id`.
    pub fn load(&self, instance_id: &str) -> Result<Option<Transaction>> {
        let leaf = transaction_file_name(instance_id)?;
        let mut file = match self.dir.open_file(&leaf, false) {
            Ok(file) => file,
            Err(error) if is_not_found(&error) => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("open transaction document for {instance_id}"))
            }
        };
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_TRANSACTION_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_TRANSACTION_BYTES,
            "transaction document exceeds size limit"
        );
        let transaction: Transaction = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse transaction document for {instance_id}"))?;
        transaction.validate()?;
        ensure!(
            transaction.instance_id == instance_id,
            "transaction instance does not match its document name"
        );
        Ok(Some(transaction))
    }

    /// Low-level durable checkpoint.
    ///
    /// Normal deployments use [`Self::begin`] and [`Self::execute`], which call
    /// this automatically. It is public so recovery tooling and crash tests can
    /// reconstruct a checkpoint. It refuses to replace a different active
    /// transaction for the same instance, but otherwise does not invent state
    /// transitions; callers are responsible for passing a validated state.
    pub fn save(&self, transaction: &Transaction) -> Result<()> {
        transaction.validate()?;
        let leaf = transaction_file_name(&transaction.instance_id)?;
        if let Some(existing) = self.load(&transaction.instance_id)? {
            ensure!(
                existing.id == transaction.id || existing.phase.is_terminal(),
                "instance {} already has unfinished transaction {}",
                transaction.instance_id,
                existing.id
            );
        }
        let bytes = serde_json::to_vec(transaction)?;
        ensure!(
            bytes.len() as u64 <= MAX_TRANSACTION_BYTES,
            "transaction document exceeds size limit"
        );
        self.write_atomic(&leaf, &bytes)
    }

    /// Begin a transaction after observing the real backend.
    ///
    /// Rejects a second non-terminal transaction for the same instance, a
    /// reused transaction id, an instance already in maintenance, and a target
    /// that is already active. The previous release is taken from
    /// [`Backend::observe`], not from the caller.
    pub fn begin(&self, spec: BeginSpec, backend: &mut dyn Backend) -> Result<Transaction> {
        model::safe_id(&spec.transaction_id)?;
        model::safe_id(&spec.instance_id)?;
        validate_release_id(&spec.target_release)?;
        model::digest(&spec.manifest_sha256)?;

        if let Some(existing) = self.load(&spec.instance_id)? {
            ensure!(
                existing.phase.is_terminal(),
                "instance {} already has unfinished transaction {} in phase {:?}",
                spec.instance_id,
                existing.id,
                existing.phase
            );
            ensure!(
                existing.id != spec.transaction_id,
                "transaction id {} was already used for instance {}",
                spec.transaction_id,
                spec.instance_id
            );
        }

        let observation = backend
            .observe(&spec.instance_id)
            .context("observe backend before beginning transaction")?;
        ensure!(
            !observation.maintenance,
            "instance {} is already in maintenance",
            spec.instance_id
        );
        ensure!(
            observation.current_release.as_deref() != Some(spec.target_release.as_str()),
            "release {} is already active",
            spec.target_release
        );

        let transaction = Transaction {
            schema_version: TRANSACTION_SCHEMA_VERSION,
            id: spec.transaction_id,
            instance_id: spec.instance_id,
            previous_release: observation.current_release,
            target_release: spec.target_release,
            manifest_sha256: spec.manifest_sha256,
            phase: Phase::Prepared,
            error: None,
            steps: Vec::new(),
        };
        transaction.validate()?;
        self.save(&transaction)?;
        Ok(transaction)
    }

    /// Drive a freshly begun transaction through prepare, maintenance, checked
    /// drain, switch, resume/readiness, commit, and maintenance release.
    ///
    /// On a pre-commit failure the transaction is rolled back and this returns
    /// [`RunOutcome::RolledBack`] only after the backend is observed to have the
    /// previous release ready and maintenance released. A rollback failure
    /// returns [`RunOutcome::RecoveryRequired`] and is never reported as
    /// `RolledBack`. A post-commit maintenance-release failure also returns
    /// `RecoveryRequired`, without restoring data.
    ///
    /// `Err` is reserved for persistence/validation failures where the durable
    /// state could not be updated; deployment failures are reflected in the
    /// returned outcome and the transaction document.
    pub fn execute(
        &self,
        transaction: &mut Transaction,
        backend: &mut dyn Backend,
    ) -> Result<RunOutcome> {
        transaction.validate()?;
        ensure!(
            transaction.phase == Phase::Prepared,
            "execute requires phase prepared, got {:?}",
            transaction.phase
        );
        let stored = self
            .load(&transaction.instance_id)?
            .context("transaction is not persisted")?;
        ensure!(
            stored == *transaction,
            "in-memory transaction does not match persisted record"
        );

        let observation = match backend.observe(&transaction.instance_id) {
            Ok(observation) => observation,
            Err(error) => {
                return self.need_recovery(
                    transaction,
                    ErrorCode::ObservationFailed,
                    StepKind::Preflight,
                    format!("preflight observation failed: {error:#}"),
                );
            }
        };
        if observation.current_release != transaction.previous_release {
            return self.need_recovery(
                transaction,
                ErrorCode::PreflightDrift,
                StepKind::Preflight,
                format!(
                    "backend release changed from {:?} to {:?} after transaction began",
                    transaction.previous_release, observation.current_release
                ),
            );
        }
        if observation.maintenance {
            return self.need_recovery(
                transaction,
                ErrorCode::PreflightDrift,
                StepKind::Preflight,
                "backend entered maintenance after transaction began".into(),
            );
        }
        self.checkpoint(
            transaction,
            Phase::Prepared,
            None,
            Some((StepKind::Preflight, StepOutcome::Succeeded, None)),
        )?;
        self.forward(transaction, backend)
    }

    fn forward(
        &self,
        transaction: &mut Transaction,
        backend: &mut dyn Backend,
    ) -> Result<RunOutcome> {
        if let Err(error) = backend.prepare(transaction) {
            return self.fail_forward(
                transaction,
                backend,
                ErrorCode::PrepareFailed,
                StepKind::Prepare,
                &error,
            );
        }
        self.checkpoint(
            transaction,
            Phase::Prepared,
            None,
            Some((StepKind::Prepare, StepOutcome::Succeeded, None)),
        )?;

        self.checkpoint(transaction, Phase::Maintenance, None, None)?;
        if let Err(error) = backend.enter_maintenance(transaction) {
            return self.fail_forward(
                transaction,
                backend,
                ErrorCode::MaintenanceFailed,
                StepKind::EnterMaintenance,
                &error,
            );
        }
        self.checkpoint(
            transaction,
            Phase::Maintenance,
            None,
            Some((StepKind::EnterMaintenance, StepOutcome::Succeeded, None)),
        )?;

        self.checkpoint(transaction, Phase::Drained, None, None)?;
        match backend.drain(transaction) {
            Ok(Drain::Drained) => {}
            Ok(Drain::Busy { remaining }) => {
                let error = anyhow::anyhow!("checked drain still has {remaining} work item(s)");
                return self.fail_forward(
                    transaction,
                    backend,
                    ErrorCode::DrainIncomplete,
                    StepKind::Drain,
                    &error,
                );
            }
            Err(error) => {
                return self.fail_forward(
                    transaction,
                    backend,
                    ErrorCode::DrainIncomplete,
                    StepKind::Drain,
                    &error,
                );
            }
        }
        self.checkpoint(
            transaction,
            Phase::Drained,
            None,
            Some((StepKind::Drain, StepOutcome::Succeeded, None)),
        )?;

        self.checkpoint(transaction, Phase::Switching, None, None)?;
        if let Err(error) = backend.switch(transaction) {
            return self.fail_forward(
                transaction,
                backend,
                ErrorCode::SwitchFailed,
                StepKind::Switch,
                &error,
            );
        }
        self.checkpoint(
            transaction,
            Phase::Switching,
            None,
            Some((StepKind::Switch, StepOutcome::Succeeded, None)),
        )?;

        self.checkpoint(transaction, Phase::Verifying, None, None)?;
        if let Err(error) = backend.resume(transaction) {
            return self.fail_forward(
                transaction,
                backend,
                ErrorCode::ResumeFailed,
                StepKind::Resume,
                &error,
            );
        }
        self.checkpoint(
            transaction,
            Phase::Verifying,
            None,
            Some((StepKind::Resume, StepOutcome::Succeeded, None)),
        )?;

        if let Err(error) = backend.verify_ready(transaction) {
            return self.fail_forward(
                transaction,
                backend,
                ErrorCode::ReadinessFailed,
                StepKind::VerifyReadiness,
                &error,
            );
        }
        match backend.observe(&transaction.instance_id) {
            Ok(observation)
                if observation.current_release.as_deref()
                    == Some(transaction.target_release.as_str())
                    && observation.ready => {}
            Ok(observation) => {
                let error = anyhow::anyhow!(
                    "readiness gate rejected backend state: release {:?}, ready {}",
                    observation.current_release,
                    observation.ready
                );
                return self.fail_forward(
                    transaction,
                    backend,
                    ErrorCode::ReadinessFailed,
                    StepKind::VerifyReadiness,
                    &error,
                );
            }
            Err(error) => {
                return self.fail_forward(
                    transaction,
                    backend,
                    ErrorCode::ObservationFailed,
                    StepKind::VerifyReadiness,
                    &error,
                );
            }
        }
        self.checkpoint(
            transaction,
            Phase::Verifying,
            None,
            Some((StepKind::VerifyReadiness, StepOutcome::Succeeded, None)),
        )?;

        self.checkpoint(
            transaction,
            Phase::Committed,
            None,
            Some((StepKind::Commit, StepOutcome::Succeeded, None)),
        )?;
        self.finish_commit(transaction, backend)
    }

    /// Release maintenance for a durable commit. Never restores data.
    fn finish_commit(
        &self,
        transaction: &mut Transaction,
        backend: &mut dyn Backend,
    ) -> Result<RunOutcome> {
        ensure!(
            transaction.phase == Phase::Committed,
            "finish_commit requires committed phase"
        );
        if let Err(error) = backend.release_maintenance(transaction) {
            return self.need_recovery(
                transaction,
                ErrorCode::CommitMaintenanceReleaseFailed,
                StepKind::ReleaseMaintenance,
                format!("commit is durable but maintenance release failed: {error:#}"),
            );
        }
        self.checkpoint(
            transaction,
            Phase::Committed,
            None,
            Some((StepKind::ReleaseMaintenance, StepOutcome::Succeeded, None)),
        )?;

        let observation = match backend.observe(&transaction.instance_id) {
            Ok(observation) => observation,
            Err(error) => {
                return self.need_recovery(
                    transaction,
                    ErrorCode::CommitObservationFailed,
                    StepKind::ReleaseMaintenance,
                    format!("post-commit observation failed: {error:#}"),
                );
            }
        };
        if observation.maintenance {
            return self.need_recovery(
                transaction,
                ErrorCode::CommitMaintenanceReleaseFailed,
                StepKind::ReleaseMaintenance,
                "backend still reports maintenance after commit".into(),
            );
        }
        if observation.current_release.as_deref() != Some(transaction.target_release.as_str()) {
            return self.need_recovery(
                transaction,
                ErrorCode::CommittedStateDrift,
                StepKind::ReleaseMaintenance,
                format!(
                    "committed target {} is not active (backend reports {:?}); refusing automatic rollback",
                    transaction.target_release, observation.current_release
                ),
            );
        }
        Ok(RunOutcome::Committed)
    }

    fn fail_forward(
        &self,
        transaction: &mut Transaction,
        backend: &mut dyn Backend,
        code: ErrorCode,
        step: StepKind,
        error: &anyhow::Error,
    ) -> Result<RunOutcome> {
        let detail = format!("{error:#}");
        let persisted = TransactionError::new(code, Some(step), detail.clone());
        self.checkpoint(
            transaction,
            Phase::RollingBack,
            Some(persisted),
            Some((step, StepOutcome::Failed, Some(detail))),
        )?;
        self.rollback(transaction, backend)
    }

    /// Idempotent rollback verified against the real backend.
    ///
    /// Requires `RollingBack` plus a recorded error. Only writes `RolledBack`
    /// after observing the previous release active (or none for a first
    /// install), ready when applicable, and maintenance released.
    fn rollback(
        &self,
        transaction: &mut Transaction,
        backend: &mut dyn Backend,
    ) -> Result<RunOutcome> {
        ensure!(
            transaction.phase == Phase::RollingBack,
            "rollback requires rolling_back phase"
        );
        let original = transaction
            .error
            .clone()
            .context("rollback requires a recorded failure")?;

        let observation = match backend.observe(&transaction.instance_id) {
            Ok(observation) => observation,
            Err(error) => {
                return self.need_recovery(
                    transaction,
                    ErrorCode::ObservationFailed,
                    StepKind::Restore,
                    format!("cannot observe backend before restore: {error:#}"),
                );
            }
        };

        let needs_restore = observation.current_release != transaction.previous_release
            || (transaction.previous_release.is_some() && !observation.ready);
        if !needs_restore {
            self.checkpoint(
                transaction,
                Phase::RollingBack,
                Some(original.clone()),
                Some((
                    StepKind::Restore,
                    StepOutcome::Skipped,
                    Some("backend already reports the previous release ready".into()),
                )),
            )?;
        } else {
            match backend.restore(transaction) {
                Ok(()) => self.checkpoint(
                    transaction,
                    Phase::RollingBack,
                    Some(original.clone()),
                    Some((StepKind::Restore, StepOutcome::Succeeded, None)),
                )?,
                Err(error) => {
                    return self.need_recovery(
                        transaction,
                        ErrorCode::RollbackRestoreFailed,
                        StepKind::Restore,
                        format!("idempotent restore failed: {error:#}"),
                    );
                }
            }
        }

        let observation = match backend.observe(&transaction.instance_id) {
            Ok(observation) => observation,
            Err(error) => {
                return self.need_recovery(
                    transaction,
                    ErrorCode::ObservationFailed,
                    StepKind::RestoreReadiness,
                    format!("cannot observe restored backend state: {error:#}"),
                );
            }
        };
        if observation.current_release != transaction.previous_release {
            return self.need_recovery(
                transaction,
                ErrorCode::RollbackRestoreFailed,
                StepKind::Restore,
                format!(
                    "backend still reports release {:?}, expected {:?}",
                    observation.current_release, transaction.previous_release
                ),
            );
        }
        if transaction.previous_release.is_some() && !observation.ready {
            return self.need_recovery(
                transaction,
                ErrorCode::RollbackReadinessFailed,
                StepKind::RestoreReadiness,
                "restored previous release is not ready".into(),
            );
        }
        self.checkpoint(
            transaction,
            Phase::RollingBack,
            Some(original.clone()),
            Some((StepKind::RestoreReadiness, StepOutcome::Succeeded, None)),
        )?;

        if observation.maintenance {
            match backend.release_maintenance(transaction) {
                Ok(()) => self.checkpoint(
                    transaction,
                    Phase::RollingBack,
                    Some(original.clone()),
                    Some((StepKind::ReleaseMaintenance, StepOutcome::Succeeded, None)),
                )?,
                Err(error) => {
                    return self.need_recovery(
                        transaction,
                        ErrorCode::RollbackMaintenanceReleaseFailed,
                        StepKind::ReleaseMaintenance,
                        format!("rollback maintenance release failed: {error:#}"),
                    );
                }
            }
        } else {
            self.checkpoint(
                transaction,
                Phase::RollingBack,
                Some(original.clone()),
                Some((
                    StepKind::ReleaseMaintenance,
                    StepOutcome::Skipped,
                    Some("maintenance was not active".into()),
                )),
            )?;
        }

        let observation = match backend.observe(&transaction.instance_id) {
            Ok(observation) => observation,
            Err(error) => {
                return self.need_recovery(
                    transaction,
                    ErrorCode::ObservationFailed,
                    StepKind::RollbackVerify,
                    format!("cannot observe final rollback state: {error:#}"),
                );
            }
        };
        if observation.maintenance {
            return self.need_recovery(
                transaction,
                ErrorCode::RollbackMaintenanceReleaseFailed,
                StepKind::ReleaseMaintenance,
                "backend still reports maintenance after rollback".into(),
            );
        }
        if observation.current_release != transaction.previous_release {
            return self.need_recovery(
                transaction,
                ErrorCode::RollbackRestoreFailed,
                StepKind::RollbackVerify,
                format!(
                    "rollback verification saw release {:?}, expected {:?}",
                    observation.current_release, transaction.previous_release
                ),
            );
        }
        if transaction.previous_release.is_some() && !observation.ready {
            return self.need_recovery(
                transaction,
                ErrorCode::RollbackReadinessFailed,
                StepKind::RollbackVerify,
                "previous release is not ready at final rollback verification".into(),
            );
        }

        self.checkpoint(
            transaction,
            Phase::RolledBack,
            Some(original),
            Some((
                StepKind::RollbackVerify,
                StepOutcome::Succeeded,
                Some("previous release active; maintenance released".into()),
            )),
        )?;
        Ok(RunOutcome::RolledBack)
    }

    fn need_recovery(
        &self,
        transaction: &mut Transaction,
        code: ErrorCode,
        step: StepKind,
        message: String,
    ) -> Result<RunOutcome> {
        let persisted = TransactionError::new(code, Some(step), message.clone());
        self.checkpoint(
            transaction,
            Phase::RecoveryRequired,
            Some(persisted),
            Some((step, StepOutcome::Failed, Some(message))),
        )?;
        Ok(RunOutcome::RecoveryRequired)
    }

    /// Recover one instance transaction from disk.
    ///
    /// * Pre-commit phases are always rolled back after observing the real
    ///   backend (the commit intent was never made durable).
    /// * `Committed` only retries the idempotent maintenance release and never
    ///   restores data.
    /// * `RollingBack` / `RecoveryRequired` retry idempotent restore and
    ///   readiness verification.
    ///
    /// Returns `Ok(None)` when the instance has no transaction document.
    pub fn recover(
        &self,
        instance_id: &str,
        backend: &mut dyn Backend,
    ) -> Result<Option<RunOutcome>> {
        let Some(mut transaction) = self.load(instance_id)? else {
            return Ok(None);
        };
        let outcome = match transaction.phase {
            Phase::RolledBack => RunOutcome::RolledBack,
            Phase::Committed => self.recover_committed(&mut transaction, backend)?,
            Phase::RecoveryRequired => self.recover_required(&mut transaction, backend)?,
            Phase::RollingBack => self.recover_rolling_back(&mut transaction, backend)?,
            Phase::Prepared
            | Phase::Maintenance
            | Phase::Drained
            | Phase::Switching
            | Phase::Verifying => self.recover_pre_commit(&mut transaction, backend)?,
        };
        Ok(Some(outcome))
    }

    fn recover_pre_commit(
        &self,
        transaction: &mut Transaction,
        backend: &mut dyn Backend,
    ) -> Result<RunOutcome> {
        let interrupted_phase = transaction.phase;
        let detail = format!("transaction interrupted in phase {interrupted_phase:?}");
        let interrupted = TransactionError::new(
            ErrorCode::Interrupted,
            Some(StepKind::Interrupted),
            detail.clone(),
        );
        self.checkpoint(
            transaction,
            Phase::RollingBack,
            Some(interrupted),
            Some((StepKind::Interrupted, StepOutcome::Failed, Some(detail))),
        )?;
        let outcome = self.rollback(transaction, backend)?;
        self.note_recovery(transaction, outcome, "pre-commit crash recovery")?;
        Ok(outcome)
    }

    fn recover_rolling_back(
        &self,
        transaction: &mut Transaction,
        backend: &mut dyn Backend,
    ) -> Result<RunOutcome> {
        self.note(
            transaction,
            StepKind::Recover,
            StepOutcome::Succeeded,
            Some("resuming interrupted rollback".into()),
        )?;
        let outcome = self.rollback(transaction, backend)?;
        self.note_recovery(transaction, outcome, "interrupted rollback")?;
        Ok(outcome)
    }

    fn recover_required(
        &self,
        transaction: &mut Transaction,
        backend: &mut dyn Backend,
    ) -> Result<RunOutcome> {
        let code = transaction
            .error
            .as_ref()
            .map(|error| error.code)
            .context("recovery_required transaction lacks an error")?;
        if matches!(
            code,
            ErrorCode::CommitMaintenanceReleaseFailed
                | ErrorCode::CommitObservationFailed
                | ErrorCode::CommittedStateDrift
        ) {
            return self.recover_committed(transaction, backend);
        }

        let original = transaction
            .error
            .clone()
            .context("recovery_required transaction lacks an error")?;
        self.checkpoint(
            transaction,
            Phase::RollingBack,
            Some(original),
            Some((
                StepKind::Recover,
                StepOutcome::Succeeded,
                Some("retrying rollback from recovery_required".into()),
            )),
        )?;
        let outcome = self.rollback(transaction, backend)?;
        self.note_recovery(transaction, outcome, "rollback retry")?;
        Ok(outcome)
    }

    fn recover_committed(
        &self,
        transaction: &mut Transaction,
        backend: &mut dyn Backend,
    ) -> Result<RunOutcome> {
        let observation = match backend.observe(&transaction.instance_id) {
            Ok(observation) => observation,
            Err(error) => {
                return self.need_recovery(
                    transaction,
                    ErrorCode::CommitObservationFailed,
                    StepKind::Recover,
                    format!("cannot observe committed transaction: {error:#}"),
                );
            }
        };
        if observation.current_release.as_deref() != Some(transaction.target_release.as_str()) {
            return self.need_recovery(
                transaction,
                ErrorCode::CommittedStateDrift,
                StepKind::Recover,
                format!(
                    "committed target {} is not active (backend reports {:?}); refusing automatic rollback",
                    transaction.target_release, observation.current_release
                ),
            );
        }
        if transaction.phase != Phase::Committed || transaction.error.is_some() {
            self.checkpoint(
                transaction,
                Phase::Committed,
                None,
                Some((
                    StepKind::Recover,
                    StepOutcome::Succeeded,
                    Some("committed target confirmed active; retrying maintenance release".into()),
                )),
            )?;
        }
        self.finish_commit(transaction, backend)
    }

    fn note_recovery(
        &self,
        transaction: &mut Transaction,
        outcome: RunOutcome,
        context: &str,
    ) -> Result<()> {
        let (step_outcome, detail) = match outcome {
            RunOutcome::RolledBack => (
                StepOutcome::Succeeded,
                format!("{context}: rollback verified; previous release active"),
            ),
            RunOutcome::Committed => (
                StepOutcome::Succeeded,
                format!("{context}: commit completed"),
            ),
            RunOutcome::RecoveryRequired => (
                StepOutcome::Failed,
                format!("{context}: automatic recovery still requires intervention"),
            ),
        };
        self.note(transaction, StepKind::Recover, step_outcome, Some(detail))
    }

    fn note(
        &self,
        transaction: &mut Transaction,
        step: StepKind,
        outcome: StepOutcome,
        detail: Option<String>,
    ) -> Result<()> {
        let phase = transaction.phase;
        let error = transaction.error.clone();
        self.checkpoint(transaction, phase, error, Some((step, outcome, detail)))
    }

    fn checkpoint(
        &self,
        transaction: &mut Transaction,
        phase: Phase,
        error: Option<TransactionError>,
        evidence: Option<(StepKind, StepOutcome, Option<String>)>,
    ) -> Result<()> {
        if let Some((step, outcome, detail)) = evidence {
            transaction.push_step(step, outcome, detail.map(bounded_message));
        }
        transaction.phase = phase;
        transaction.error = error;
        transaction.validate()?;
        self.save(transaction)
    }

    fn write_atomic(&self, leaf: &str, bytes: &[u8]) -> Result<()> {
        model::safe_id(leaf)?;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let temporary = format!("txn-tmp-{}-{nanos}-{counter}", std::process::id());
        model::safe_id(&temporary)?;

        let mut file = self.dir.create(&temporary)?;
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            let _ = self.unlink(&temporary);
            return Err(error).context("write transaction temporary file");
        }
        drop(file);

        if let Err(error) = self.rename(&temporary, leaf).and_then(|()| self.dir.sync()) {
            let _ = self.unlink(&temporary);
            return Err(error).context("publish transaction document");
        }
        Ok(())
    }

    fn rename(&self, old: &str, new: &str) -> Result<()> {
        model::safe_id(old)?;
        model::safe_id(new)?;
        let old = CString::new(old)?;
        let new = CString::new(new)?;
        // SAFETY: both names are validated single components and `dir` is a
        // pinned directory descriptor. renameat atomically replaces the target.
        let result = unsafe {
            libc::renameat(
                self.dir.0.as_raw_fd(),
                old.as_ptr(),
                self.dir.0.as_raw_fd(),
                new.as_ptr(),
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("rename transaction document");
        }
        Ok(())
    }

    fn unlink(&self, leaf: &str) -> Result<()> {
        model::safe_id(leaf)?;
        let leaf = CString::new(leaf)?;
        // SAFETY: validated single component in the pinned directory.
        let result = unsafe { libc::unlinkat(self.dir.0.as_raw_fd(), leaf.as_ptr(), 0) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("unlink transaction temporary file");
        }
        Ok(())
    }
}

fn transaction_file_name(instance_id: &str) -> Result<String> {
    model::safe_id(instance_id)?;
    Ok(format!("transaction-{instance_id}.json"))
}

fn validate_release_id(value: &str) -> Result<()> {
    model::safe_id(value)?;
    ensure!(
        value != "store-format" && value != "store-lock" && !value.starts_with("pending-"),
        "reserved release identifier"
    );
    Ok(())
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

fn bounded_message(message: String) -> String {
    if message.chars().count() <= MAX_MESSAGE_CHARS {
        message
    } else {
        message.chars().take(MAX_MESSAGE_CHARS).collect()
    }
}

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
