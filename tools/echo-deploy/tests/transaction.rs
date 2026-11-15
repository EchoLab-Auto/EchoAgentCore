//! Fault-injection tests for the durable deployment transaction state machine.
//!
//! No real service, systemctl, or network is used. The fake backend mutates
//! in-memory state and, before every side effect, asserts that the phase was
//! already persisted to disk (the intent-before-effect contract).

use anyhow::{bail, Result};
use echo_deploy::transaction::{
    Backend, BeginSpec, Drain, ErrorCode, Observation, Phase, RunOutcome, StepEvidence, StepKind,
    StepOutcome, Transaction, TransactionError, TransactionStore, TRANSACTION_SCHEMA_VERSION,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
};
use tempfile::{tempdir, TempDir};

fn manifest() -> String {
    "a".repeat(64)
}

fn read_persisted(state_dir: &Path, instance_id: &str) -> Transaction {
    let path = state_dir.join(format!("transaction-{instance_id}.json"));
    let bytes = fs::read(&path).unwrap_or_else(|error| {
        panic!("read {}: {error}", path.display());
    });
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!("parse {}: {error}", path.display());
    })
}

fn transaction_paths(state_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(state_dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("transaction-") && name.ends_with(".json") {
            paths.push(entry.path());
        }
    }
    paths.sort();
    paths
}

fn transaction_path(state_dir: &Path) -> PathBuf {
    let paths = transaction_paths(state_dir);
    assert_eq!(
        paths.len(),
        1,
        "expected exactly one transaction document in {}",
        state_dir.display()
    );
    paths.into_iter().next().unwrap()
}

fn assert_no_temporary_files(state_dir: &Path) {
    for entry in fs::read_dir(state_dir).unwrap() {
        let name = entry.unwrap().file_name();
        assert!(
            !name.to_string_lossy().starts_with("txn-tmp-"),
            "temporary transaction file was left behind: {:?}",
            name
        );
    }
}

struct Fake {
    release: Option<String>,
    maintenance: bool,
    ready: bool,
    active_work: u32,
    fail: BTreeMap<StepKind, usize>,
    drain_busy: bool,
    restore_failures: usize,
    fail_release_on_commit: bool,
    release_commit_noop: bool,
    fail_observe_after_commit_release: bool,
    clear_ready_on_verify: bool,
    observe_failures: usize,
    long_error_on_prepare: bool,
    calls: Vec<StepKind>,
    state_dir: PathBuf,
}

impl Fake {
    fn new(state_dir: PathBuf, previous: Option<&str>) -> Self {
        Self {
            release: previous.map(str::to_owned),
            maintenance: false,
            ready: previous.is_some(),
            active_work: 0,
            fail: BTreeMap::new(),
            drain_busy: false,
            restore_failures: 0,
            fail_release_on_commit: false,
            release_commit_noop: false,
            fail_observe_after_commit_release: false,
            clear_ready_on_verify: false,
            observe_failures: 0,
            long_error_on_prepare: false,
            calls: Vec::new(),
            state_dir,
        }
    }

    fn fail_once(&mut self, step: StepKind) {
        *self.fail.entry(step).or_insert(0) += 1;
    }

    fn take_failure(&mut self, step: StepKind) -> bool {
        if let Some(remaining) = self.fail.get_mut(&step) {
            if *remaining > 0 {
                *remaining -= 1;
                return true;
            }
        }
        false
    }

    fn record(&mut self, transaction: &Transaction, step: StepKind) {
        let persisted = read_persisted(&self.state_dir, &transaction.instance_id);
        assert_eq!(persisted.id, transaction.id);
        assert_eq!(persisted.instance_id, transaction.instance_id);
        assert_eq!(
            persisted.phase, transaction.phase,
            "phase intent for {step:?} was not persisted before its side effect"
        );
        let expected = match step {
            StepKind::Prepare => Phase::Prepared,
            StepKind::EnterMaintenance => Phase::Maintenance,
            StepKind::Drain => Phase::Drained,
            StepKind::Switch => Phase::Switching,
            StepKind::Resume | StepKind::VerifyReadiness => Phase::Verifying,
            StepKind::Restore => Phase::RollingBack,
            StepKind::ReleaseMaintenance => match transaction.phase {
                Phase::Committed => Phase::Committed,
                Phase::RollingBack => Phase::RollingBack,
                other => panic!("release_maintenance in unexpected phase {other:?}"),
            },
            other => panic!("unexpected backend step {other:?}"),
        };
        assert_eq!(
            transaction.phase, expected,
            "{step:?} called in unexpected phase"
        );
        self.calls.push(step);
    }
}

impl Backend for Fake {
    fn observe(&mut self, _instance_id: &str) -> Result<Observation> {
        if self.observe_failures > 0 {
            self.observe_failures -= 1;
            bail!("injected observe failure");
        }
        Ok(Observation::new(
            self.release.clone(),
            self.maintenance,
            self.ready,
            self.active_work,
        ))
    }

    fn prepare(&mut self, transaction: &Transaction) -> Result<()> {
        self.record(transaction, StepKind::Prepare);
        if self.take_failure(StepKind::Prepare) {
            bail!("injected prepare failure");
        }
        if self.long_error_on_prepare {
            bail!("{}", "x".repeat(10_000));
        }
        Ok(())
    }

    fn enter_maintenance(&mut self, transaction: &Transaction) -> Result<()> {
        self.record(transaction, StepKind::EnterMaintenance);
        if self.take_failure(StepKind::EnterMaintenance) {
            bail!("injected maintenance failure");
        }
        self.maintenance = true;
        self.ready = false;
        self.active_work = self.active_work.max(1);
        Ok(())
    }

    fn drain(&mut self, transaction: &Transaction) -> Result<Drain> {
        self.record(transaction, StepKind::Drain);
        if self.drain_busy {
            return Ok(Drain::Busy {
                remaining: self.active_work.max(2),
            });
        }
        self.active_work = 0;
        Ok(Drain::Drained)
    }

    fn switch(&mut self, transaction: &Transaction) -> Result<()> {
        self.record(transaction, StepKind::Switch);
        if self.take_failure(StepKind::Switch) {
            bail!("injected switch failure");
        }
        self.release = Some(transaction.target_release.clone());
        self.ready = false;
        Ok(())
    }

    fn resume(&mut self, transaction: &Transaction) -> Result<()> {
        self.record(transaction, StepKind::Resume);
        if self.take_failure(StepKind::Resume) {
            bail!("injected resume failure");
        }
        self.ready = self.release.as_deref() == Some(transaction.target_release.as_str());
        Ok(())
    }

    fn verify_ready(&mut self, transaction: &Transaction) -> Result<()> {
        self.record(transaction, StepKind::VerifyReadiness);
        if self.take_failure(StepKind::VerifyReadiness) {
            bail!("injected readiness failure");
        }
        if self.clear_ready_on_verify {
            // Simulate a backend that returns success while the real service
            // is not actually ready. The state machine must catch this with a
            // real observation before committing.
            self.ready = false;
            return Ok(());
        }
        if self.release.as_deref() != Some(transaction.target_release.as_str()) {
            bail!("target release is not active");
        }
        if !self.ready {
            bail!("target release is not ready");
        }
        Ok(())
    }

    fn release_maintenance(&mut self, transaction: &Transaction) -> Result<()> {
        self.record(transaction, StepKind::ReleaseMaintenance);
        if transaction.phase == Phase::Committed {
            if self.fail_release_on_commit {
                bail!("injected commit maintenance-release failure");
            }
            if self.release_commit_noop {
                // Pretend success but leave maintenance active.
                return Ok(());
            }
        }
        self.maintenance = false;
        if transaction.phase == Phase::Committed && self.fail_observe_after_commit_release {
            self.observe_failures = self.observe_failures.max(1);
        }
        Ok(())
    }

    fn restore(&mut self, transaction: &Transaction) -> Result<()> {
        self.record(transaction, StepKind::Restore);
        if self.restore_failures > 0 {
            self.restore_failures -= 1;
            bail!("injected restore failure");
        }
        self.release = transaction.previous_release.clone();
        self.ready = transaction.previous_release.is_some();
        self.active_work = 0;
        Ok(())
    }
}

struct Harness {
    store: TransactionStore,
    fake: Fake,
    _temp: TempDir,
}

impl Harness {
    fn new(previous: Option<&str>) -> Self {
        let temp = tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = TransactionStore::open(temp.path()).unwrap();
        let fake = Fake::new(temp.path().to_path_buf(), previous);
        Self {
            store,
            fake,
            _temp: temp,
        }
    }

    fn begin(&mut self, id: &str, instance: &str, target: &str) -> Transaction {
        self.store
            .begin(
                BeginSpec::new(id, instance, target, manifest()),
                &mut self.fake,
            )
            .unwrap()
    }
}

#[test]
fn happy_path_commits_and_persists_evidence() {
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx-1", "inst", "new");
    assert_eq!(transaction.previous_release.as_deref(), Some("old"));
    assert_eq!(transaction.phase, Phase::Prepared);
    assert_eq!(transaction.schema_version, TRANSACTION_SCHEMA_VERSION);

    let outcome = harness
        .store
        .execute(&mut transaction, &mut harness.fake)
        .unwrap();
    assert_eq!(outcome, RunOutcome::Committed);
    assert_eq!(transaction.phase, Phase::Committed);
    assert!(transaction.error.is_none());
    assert_eq!(harness.fake.release.as_deref(), Some("new"));
    assert!(!harness.fake.maintenance);
    assert!(harness.fake.ready);
    assert_eq!(
        harness.fake.calls,
        vec![
            StepKind::Prepare,
            StepKind::EnterMaintenance,
            StepKind::Drain,
            StepKind::Switch,
            StepKind::Resume,
            StepKind::VerifyReadiness,
            StepKind::ReleaseMaintenance,
        ]
    );

    let evidence: Vec<_> = transaction
        .steps
        .iter()
        .map(|step| (step.step, step.outcome))
        .collect();
    assert_eq!(
        evidence,
        vec![
            (StepKind::Preflight, StepOutcome::Succeeded),
            (StepKind::Prepare, StepOutcome::Succeeded),
            (StepKind::EnterMaintenance, StepOutcome::Succeeded),
            (StepKind::Drain, StepOutcome::Succeeded),
            (StepKind::Switch, StepOutcome::Succeeded),
            (StepKind::Resume, StepOutcome::Succeeded),
            (StepKind::VerifyReadiness, StepOutcome::Succeeded),
            (StepKind::Commit, StepOutcome::Succeeded),
            (StepKind::ReleaseMaintenance, StepOutcome::Succeeded),
        ]
    );
    assert_eq!(
        transaction
            .steps
            .iter()
            .map(|step| step.sequence)
            .collect::<Vec<_>>(),
        (1..=9).collect::<Vec<_>>()
    );

    let persisted = harness.store.load("inst").unwrap().unwrap();
    assert_eq!(persisted, transaction);
    assert_eq!(harness.store.instances().unwrap(), vec!["inst".to_string()]);
    let path = transaction_path(harness.store.state_dir());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "transaction document must be private"
    );
    assert_no_temporary_files(harness.store.state_dir());
}

#[test]
fn every_forward_failure_rolls_back_without_false_success() {
    let cases = [
        (StepKind::Prepare, ErrorCode::PrepareFailed),
        (StepKind::EnterMaintenance, ErrorCode::MaintenanceFailed),
        (StepKind::Switch, ErrorCode::SwitchFailed),
        (StepKind::Resume, ErrorCode::ResumeFailed),
        (StepKind::VerifyReadiness, ErrorCode::ReadinessFailed),
    ];
    for (step, code) in cases {
        let mut harness = Harness::new(Some("old"));
        let mut transaction = harness.begin("tx", "inst", "new");
        harness.fake.fail_once(step);

        let outcome = harness
            .store
            .execute(&mut transaction, &mut harness.fake)
            .unwrap();
        assert_eq!(outcome, RunOutcome::RolledBack, "failure at {step:?}");
        assert_eq!(transaction.phase, Phase::RolledBack, "failure at {step:?}");
        assert_eq!(
            transaction.error.as_ref().unwrap().code,
            code,
            "failure at {step:?}"
        );
        assert_eq!(
            harness.fake.release.as_deref(),
            Some("old"),
            "failure at {step:?}"
        );
        assert!(!harness.fake.maintenance, "failure at {step:?}");
        assert!(harness.fake.ready, "failure at {step:?}");
        assert!(
            transaction
                .steps
                .iter()
                .any(|evidence| evidence.step == step && evidence.outcome == StepOutcome::Failed),
            "missing failed evidence for {step:?}"
        );
        assert_eq!(
            harness.store.load("inst").unwrap().unwrap().phase,
            Phase::RolledBack
        );
        assert_no_temporary_files(harness.store.state_dir());
    }

    // Checked drain must never force work; a busy drain rolls back.
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx", "inst", "new");
    harness.fake.drain_busy = true;
    let outcome = harness
        .store
        .execute(&mut transaction, &mut harness.fake)
        .unwrap();
    assert_eq!(outcome, RunOutcome::RolledBack);
    assert_eq!(
        transaction.error.as_ref().unwrap().code,
        ErrorCode::DrainIncomplete
    );
    assert_eq!(harness.fake.release.as_deref(), Some("old"));
    assert!(!harness.fake.maintenance);
}

#[test]
fn rollback_failure_is_recovery_required_not_rolled_back() {
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx", "inst", "new");
    harness.fake.fail_once(StepKind::Switch);
    harness.fake.restore_failures = 1;

    let outcome = harness
        .store
        .execute(&mut transaction, &mut harness.fake)
        .unwrap();
    assert_eq!(outcome, RunOutcome::RecoveryRequired);
    assert_eq!(transaction.phase, Phase::RecoveryRequired);
    assert_ne!(transaction.phase, Phase::RolledBack);
    assert_eq!(
        transaction.error.as_ref().unwrap().code,
        ErrorCode::RollbackRestoreFailed
    );
    assert_eq!(harness.fake.release.as_deref(), Some("old"));
    assert!(harness.fake.maintenance);
    assert!(transaction
        .steps
        .iter()
        .any(|evidence| evidence.step == StepKind::Restore
            && evidence.outcome == StepOutcome::Failed));

    // Recovery retries the idempotent restore and only then records rollback.
    let recovered = harness
        .store
        .recover("inst", &mut harness.fake)
        .unwrap()
        .unwrap();
    assert_eq!(recovered, RunOutcome::RolledBack);
    let persisted = harness.store.load("inst").unwrap().unwrap();
    assert_eq!(persisted.phase, Phase::RolledBack);
    assert_eq!(harness.fake.release.as_deref(), Some("old"));
    assert!(!harness.fake.maintenance);
    assert!(harness.fake.ready);
    assert!(persisted
        .steps
        .iter()
        .any(|evidence| evidence.step == StepKind::Recover
            && evidence.outcome == StepOutcome::Succeeded));
}

#[test]
fn commit_maintenance_failure_never_rolls_back_served_data() {
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx", "inst", "new");
    harness.fake.fail_release_on_commit = true;

    let outcome = harness
        .store
        .execute(&mut transaction, &mut harness.fake)
        .unwrap();
    assert_eq!(outcome, RunOutcome::RecoveryRequired);
    assert_eq!(transaction.phase, Phase::RecoveryRequired);
    assert_eq!(
        transaction.error.as_ref().unwrap().code,
        ErrorCode::CommitMaintenanceReleaseFailed
    );
    assert_eq!(harness.fake.release.as_deref(), Some("new"));
    assert!(harness.fake.maintenance);
    assert!(
        !harness.fake.calls.contains(&StepKind::Restore),
        "post-commit failure must not restore previous data"
    );
    assert_eq!(
        harness.store.load("inst").unwrap().unwrap().phase,
        Phase::RecoveryRequired
    );

    harness.fake.fail_release_on_commit = false;
    let recovered = harness
        .store
        .recover("inst", &mut harness.fake)
        .unwrap()
        .unwrap();
    assert_eq!(recovered, RunOutcome::Committed);
    assert_eq!(
        harness.store.load("inst").unwrap().unwrap().phase,
        Phase::Committed
    );
    assert_eq!(harness.fake.release.as_deref(), Some("new"));
    assert!(!harness.fake.maintenance);
    assert!(!harness.fake.calls.contains(&StepKind::Restore));
}

#[test]
fn post_commit_observation_guard_rejects_lying_backend() {
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx", "inst", "new");
    harness.fake.release_commit_noop = true;

    let outcome = harness
        .store
        .execute(&mut transaction, &mut harness.fake)
        .unwrap();
    assert_eq!(outcome, RunOutcome::RecoveryRequired);
    assert_eq!(
        transaction.error.as_ref().unwrap().code,
        ErrorCode::CommitMaintenanceReleaseFailed
    );
    assert!(harness.fake.maintenance);
    assert!(!harness.fake.calls.contains(&StepKind::Restore));

    harness.fake.release_commit_noop = false;
    let recovered = harness
        .store
        .recover("inst", &mut harness.fake)
        .unwrap()
        .unwrap();
    assert_eq!(recovered, RunOutcome::Committed);
    assert!(!harness.fake.maintenance);
}

#[test]
fn readiness_observation_gate_rejects_lying_probe() {
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx", "inst", "new");
    harness.fake.clear_ready_on_verify = true;

    let outcome = harness
        .store
        .execute(&mut transaction, &mut harness.fake)
        .unwrap();
    assert_eq!(outcome, RunOutcome::RolledBack);
    assert_eq!(
        transaction.error.as_ref().unwrap().code,
        ErrorCode::ReadinessFailed
    );
    assert_eq!(harness.fake.release.as_deref(), Some("old"));
    assert!(harness.fake.ready);
    assert!(!harness.fake.maintenance);
}

#[test]
fn preflight_observation_failure_is_not_reported_as_rollback() {
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx", "inst", "new");
    harness.fake.observe_failures = 1;

    let outcome = harness
        .store
        .execute(&mut transaction, &mut harness.fake)
        .unwrap();
    assert_eq!(outcome, RunOutcome::RecoveryRequired);
    assert_eq!(transaction.phase, Phase::RecoveryRequired);
    assert_eq!(
        transaction.error.as_ref().unwrap().code,
        ErrorCode::ObservationFailed
    );
    assert!(harness.fake.calls.is_empty());

    let recovered = harness
        .store
        .recover("inst", &mut harness.fake)
        .unwrap()
        .unwrap();
    assert_eq!(recovered, RunOutcome::RolledBack);
    assert_eq!(
        harness.store.load("inst").unwrap().unwrap().phase,
        Phase::RolledBack
    );
    assert_eq!(harness.fake.release.as_deref(), Some("old"));
    assert!(harness.fake.ready);
}

#[test]
fn crash_in_any_pre_commit_phase_rolls_back_from_real_backend() {
    let cases = [
        (Phase::Prepared, false, Some("old"), false, 0),
        (Phase::Maintenance, true, Some("old"), false, 2),
        (Phase::Drained, true, Some("old"), false, 0),
        (Phase::Switching, true, Some("new"), false, 0),
        (Phase::Verifying, true, Some("new"), true, 0),
    ];
    for (phase, maintenance, release, ready, active_work) in cases {
        let mut harness = Harness::new(Some("old"));
        let mut transaction = harness.begin("tx", "inst", "new");
        transaction.phase = phase;
        harness.store.save(&transaction).unwrap();

        harness.fake.maintenance = maintenance;
        harness.fake.release = release.map(str::to_owned);
        harness.fake.ready = ready;
        harness.fake.active_work = active_work;

        let outcome = harness
            .store
            .recover("inst", &mut harness.fake)
            .unwrap()
            .unwrap();
        assert_eq!(outcome, RunOutcome::RolledBack, "crash in {phase:?}");
        let persisted = harness.store.load("inst").unwrap().unwrap();
        assert_eq!(persisted.phase, Phase::RolledBack, "crash in {phase:?}");
        assert_eq!(
            persisted.error.as_ref().unwrap().code,
            ErrorCode::Interrupted,
            "crash in {phase:?}"
        );
        assert_eq!(
            harness.fake.release.as_deref(),
            Some("old"),
            "crash in {phase:?}"
        );
        assert!(!harness.fake.maintenance, "crash in {phase:?}");
        assert!(harness.fake.ready, "crash in {phase:?}");
        assert!(
            persisted
                .steps
                .iter()
                .any(|evidence| evidence.step == StepKind::Interrupted
                    && evidence.outcome == StepOutcome::Failed),
            "missing interrupted evidence for {phase:?}"
        );
        assert!(
            persisted
                .steps
                .iter()
                .any(|evidence| evidence.step == StepKind::RollbackVerify
                    && evidence.outcome == StepOutcome::Succeeded),
            "missing rollback verification for {phase:?}"
        );
    }
}

#[test]
fn crash_after_commit_only_releases_maintenance() {
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx", "inst", "new");
    transaction.phase = Phase::Committed;
    harness.store.save(&transaction).unwrap();
    harness.fake.release = Some("new".into());
    harness.fake.maintenance = true;
    harness.fake.ready = true;

    let outcome = harness
        .store
        .recover("inst", &mut harness.fake)
        .unwrap()
        .unwrap();
    assert_eq!(outcome, RunOutcome::Committed);
    let persisted = harness.store.load("inst").unwrap().unwrap();
    assert_eq!(persisted.phase, Phase::Committed);
    assert_eq!(harness.fake.release.as_deref(), Some("new"));
    assert!(!harness.fake.maintenance);
    assert!(!harness.fake.calls.contains(&StepKind::Restore));
}

#[test]
fn committed_state_drift_is_never_rolled_back() {
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx", "inst", "new");
    transaction.phase = Phase::Committed;
    harness.store.save(&transaction).unwrap();
    // Committed target is not active: recovery must not silently restore.
    harness.fake.release = Some("old".into());
    harness.fake.ready = true;

    let outcome = harness
        .store
        .recover("inst", &mut harness.fake)
        .unwrap()
        .unwrap();
    assert_eq!(outcome, RunOutcome::RecoveryRequired);
    assert_eq!(
        harness
            .store
            .load("inst")
            .unwrap()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .code,
        ErrorCode::CommittedStateDrift
    );
    assert_eq!(harness.fake.release.as_deref(), Some("old"));
    assert!(!harness.fake.calls.contains(&StepKind::Restore));

    // Once the committed target is observed again, only maintenance is retried.
    harness.fake.release = Some("new".into());
    let recovered = harness
        .store
        .recover("inst", &mut harness.fake)
        .unwrap()
        .unwrap();
    assert_eq!(recovered, RunOutcome::Committed);
    assert_eq!(harness.fake.release.as_deref(), Some("new"));
    assert!(!harness.fake.calls.contains(&StepKind::Restore));
}

#[test]
fn interrupted_rollback_and_recovery_required_are_idempotent() {
    // A crash while rolling back resumes the idempotent restore.
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx", "inst", "new");
    transaction.phase = Phase::RollingBack;
    transaction.error = Some(TransactionError::new(
        ErrorCode::PrepareFailed,
        Some(StepKind::Prepare),
        "original failure",
    ));
    harness.store.save(&transaction).unwrap();
    harness.fake.release = Some("new".into());
    harness.fake.maintenance = true;
    harness.fake.ready = false;

    let outcome = harness
        .store
        .recover("inst", &mut harness.fake)
        .unwrap()
        .unwrap();
    assert_eq!(outcome, RunOutcome::RolledBack);
    assert_eq!(harness.fake.release.as_deref(), Some("old"));
    assert!(!harness.fake.maintenance);
    assert!(harness.fake.ready);

    // Recovery is idempotent once terminal.
    let calls = harness.fake.calls.len();
    assert_eq!(
        harness
            .store
            .recover("inst", &mut harness.fake)
            .unwrap()
            .unwrap(),
        RunOutcome::RolledBack
    );
    assert_eq!(harness.fake.calls.len(), calls);

    // A failed rollback stays RecoveryRequired until a retry succeeds.
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx", "inst", "new");
    transaction.phase = Phase::RecoveryRequired;
    transaction.error = Some(TransactionError::new(
        ErrorCode::RollbackRestoreFailed,
        Some(StepKind::Restore),
        "previous rollback failed",
    ));
    harness.store.save(&transaction).unwrap();
    harness.fake.release = Some("new".into());
    harness.fake.maintenance = true;
    harness.fake.ready = false;
    harness.fake.restore_failures = 1;

    let outcome = harness
        .store
        .recover("inst", &mut harness.fake)
        .unwrap()
        .unwrap();
    assert_eq!(outcome, RunOutcome::RecoveryRequired);
    assert_eq!(
        harness.store.load("inst").unwrap().unwrap().phase,
        Phase::RecoveryRequired
    );

    let outcome = harness
        .store
        .recover("inst", &mut harness.fake)
        .unwrap()
        .unwrap();
    assert_eq!(outcome, RunOutcome::RolledBack);
    assert_eq!(
        harness.store.load("inst").unwrap().unwrap().phase,
        Phase::RolledBack
    );
    assert_eq!(harness.fake.release.as_deref(), Some("old"));
    assert!(harness.fake.ready);
}

#[test]
fn first_install_rollback_removes_release_and_happy_path_commits() {
    let mut harness = Harness::new(None);
    let mut transaction = harness.begin("tx", "inst", "new");
    assert_eq!(transaction.previous_release, None);
    harness.fake.fail_once(StepKind::Resume);

    let outcome = harness
        .store
        .execute(&mut transaction, &mut harness.fake)
        .unwrap();
    assert_eq!(outcome, RunOutcome::RolledBack);
    assert_eq!(harness.fake.release, None);
    assert!(!harness.fake.maintenance);
    assert!(!harness.fake.ready);
    assert!(harness.fake.calls.contains(&StepKind::Restore));

    let mut harness = Harness::new(None);
    let mut transaction = harness.begin("tx", "inst", "new");
    let outcome = harness
        .store
        .execute(&mut transaction, &mut harness.fake)
        .unwrap();
    assert_eq!(outcome, RunOutcome::Committed);
    assert_eq!(harness.fake.release.as_deref(), Some("new"));
    assert!(harness.fake.ready);
    assert!(!harness.fake.maintenance);
}

#[test]
fn one_non_terminal_transaction_per_instance_is_allowed() {
    let mut harness = Harness::new(Some("old"));
    let _first = harness.begin("tx-1", "inst", "new1");

    assert!(
        harness
            .store
            .begin(
                BeginSpec::new("tx-2", "inst", "new2", manifest()),
                &mut harness.fake
            )
            .is_err(),
        "second non-terminal transaction for the same instance must be rejected"
    );
    // A different instance is independent.
    let other = harness
        .store
        .begin(
            BeginSpec::new("tx-3", "other", "new2", manifest()),
            &mut harness.fake,
        )
        .unwrap();
    assert_eq!(other.previous_release.as_deref(), Some("old"));
    assert_eq!(
        harness.store.instances().unwrap(),
        vec!["inst".to_string(), "other".to_string()]
    );

    // Complete the first transaction, then a new transaction is allowed.
    let mut first = harness.store.load("inst").unwrap().unwrap();
    assert_eq!(
        harness
            .store
            .execute(&mut first, &mut harness.fake)
            .unwrap(),
        RunOutcome::Committed
    );
    assert!(
        harness
            .store
            .begin(
                BeginSpec::new("tx-1", "inst", "new4", manifest()),
                &mut harness.fake
            )
            .is_err(),
        "transaction id must not be reused for the instance"
    );
    let next = harness
        .store
        .begin(
            BeginSpec::new("tx-4", "inst", "new3", manifest()),
            &mut harness.fake,
        )
        .unwrap();
    assert_eq!(next.previous_release.as_deref(), Some("new1"));

    // RecoveryRequired is non-terminal and blocks a new task.
    let mut blocked = Harness::new(Some("old"));
    let mut transaction = blocked.begin("tx-recover", "inst", "new");
    transaction.phase = Phase::RecoveryRequired;
    transaction.error = Some(TransactionError::new(
        ErrorCode::RollbackRestoreFailed,
        Some(StepKind::Restore),
        "needs intervention",
    ));
    blocked.store.save(&transaction).unwrap();
    assert!(blocked
        .store
        .begin(
            BeginSpec::new("tx-next", "inst", "newer", manifest()),
            &mut blocked.fake
        )
        .is_err());
}

#[test]
fn strict_serde_and_state_invariants_fail_closed() {
    let mut harness = Harness::new(Some("old"));
    let transaction = harness.begin("tx", "inst", "new");

    let value = serde_json::to_value(&transaction).unwrap();
    let round_trip: Transaction = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(round_trip, transaction);

    let mut unknown = value.clone();
    unknown["unexpected"] = json!(true);
    assert!(serde_json::from_value::<Transaction>(unknown).is_err());

    let mut missing = value.clone();
    missing.as_object_mut().unwrap().remove("error");
    assert!(serde_json::from_value::<Transaction>(missing).is_err());

    let mut bad_phase = value;
    bad_phase["phase"] = json!("not_a_phase");
    assert!(serde_json::from_value::<Transaction>(bad_phase).is_err());

    let mut bad = transaction.clone();
    bad.schema_version = 2;
    assert!(bad.validate().is_err());

    let mut bad = transaction.clone();
    bad.previous_release = Some(transaction.target_release.clone());
    assert!(bad.validate().is_err());

    let mut bad = transaction.clone();
    bad.manifest_sha256 = "short".into();
    assert!(bad.validate().is_err());

    let mut bad = transaction.clone();
    bad.phase = Phase::Committed;
    bad.error = Some(TransactionError::new(
        ErrorCode::PrepareFailed,
        Some(StepKind::Prepare),
        "not allowed after commit",
    ));
    assert!(bad.validate().is_err());

    let mut bad = transaction.clone();
    bad.phase = Phase::RecoveryRequired;
    bad.error = None;
    assert!(bad.validate().is_err());

    let mut bad = transaction.clone();
    bad.phase = Phase::RollingBack;
    bad.error = Some(TransactionError {
        code: ErrorCode::PrepareFailed,
        message: String::new(),
        step: Some(StepKind::Prepare),
    });
    assert!(bad.validate().is_err());

    let mut bad = transaction.clone();
    bad.steps.push(StepEvidence {
        sequence: 2,
        step: StepKind::Commit,
        outcome: StepOutcome::Succeeded,
        at_unix_ms: 0,
        detail: None,
    });
    assert!(bad.validate().is_err());

    let mut bad = transaction.clone();
    bad.schema_version = 2;
    assert!(harness.store.save(&bad).is_err());
    assert_eq!(harness.store.load("inst").unwrap().unwrap(), transaction);

    // Corrupt documents are rejected, never silently repaired.
    let path = transaction_path(harness.store.state_dir());
    fs::write(&path, b"{not json").unwrap();
    assert!(harness.store.load("inst").is_err());
}

#[test]
fn post_commit_observation_failure_never_rolls_back() {
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx", "inst", "new");
    harness.fake.fail_observe_after_commit_release = true;

    let outcome = harness
        .store
        .execute(&mut transaction, &mut harness.fake)
        .unwrap();
    assert_eq!(outcome, RunOutcome::RecoveryRequired);
    assert_eq!(
        transaction.error.as_ref().unwrap().code,
        ErrorCode::CommitObservationFailed
    );
    assert_eq!(harness.fake.release.as_deref(), Some("new"));
    assert!(!harness.fake.maintenance);
    assert!(
        !harness.fake.calls.contains(&StepKind::Restore),
        "post-commit observation failure must not restore previous data"
    );

    harness.fake.fail_observe_after_commit_release = false;
    let recovered = harness
        .store
        .recover("inst", &mut harness.fake)
        .unwrap()
        .unwrap();
    assert_eq!(recovered, RunOutcome::Committed);
    assert_eq!(
        harness.store.load("inst").unwrap().unwrap().phase,
        Phase::Committed
    );
    assert_eq!(harness.fake.release.as_deref(), Some("new"));
    assert!(!harness.fake.maintenance);
    assert!(!harness.fake.calls.contains(&StepKind::Restore));
}

#[test]
fn oversized_backend_error_is_truncated_and_recorded() {
    let mut harness = Harness::new(Some("old"));
    let mut transaction = harness.begin("tx", "inst", "new");
    harness.fake.long_error_on_prepare = true;

    let outcome = harness
        .store
        .execute(&mut transaction, &mut harness.fake)
        .unwrap();
    assert_eq!(outcome, RunOutcome::RolledBack);
    assert_eq!(
        transaction.error.as_ref().unwrap().code,
        ErrorCode::PrepareFailed
    );
    assert!(
        transaction.error.as_ref().unwrap().message.chars().count() <= 2048,
        "transaction error must be bounded"
    );
    let failed = transaction
        .steps
        .iter()
        .find(|evidence| {
            evidence.step == StepKind::Prepare && evidence.outcome == StepOutcome::Failed
        })
        .expect("failed prepare evidence");
    assert!(
        failed.detail.as_ref().unwrap().chars().count() <= 2048,
        "step evidence detail must be bounded"
    );
    assert_eq!(
        harness.store.load("inst").unwrap().unwrap().phase,
        Phase::RolledBack
    );
    assert_eq!(harness.fake.release.as_deref(), Some("old"));
    assert!(harness.fake.ready);
}

#[test]
fn state_directory_must_be_explicit_private_and_not_symlinked() {
    let temp = tempdir().unwrap();
    assert!(TransactionStore::open(Path::new("relative/path")).is_err());
    assert!(TransactionStore::open(&temp.path().join("missing")).is_err());

    let file = temp.path().join("file");
    fs::write(&file, b"x").unwrap();
    assert!(TransactionStore::open(&file).is_err());

    let public = temp.path().join("public");
    fs::create_dir(&public).unwrap();
    fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(TransactionStore::open(&public).is_err());

    let private = temp.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let store = TransactionStore::open(&private).unwrap();
    assert_eq!(store.state_dir(), private.as_path());

    let link = temp.path().join("link");
    symlink(&private, &link).unwrap();
    assert!(TransactionStore::open(&link).is_err());
}
