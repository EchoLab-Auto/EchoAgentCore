//! Bounded, shell-free systemd execution primitives for one explicit instance.
//!
//! Not a CLI/hook/updater: one validated [`SystemdSpec`] plus apply-step
//! primitives a transaction backend can compose. Mutations are followed by real
//! observations; ambiguity is an error.
//!
//! * `systemctl` is always [`SYSTEMCTL_PROGRAM`], invoked as argv (no shell,
//!   no ambient `PATH`). Units must be frozen in the spec.
//! * A stop succeeds only after `ActiveState=inactive` is observed before the
//!   stop-confirmation deadline; a killed/timed-out client is never "stopped".
//! * Selector switching refuses an existing non-symlink, a target outside the
//!   store, or anything but exactly one release directory below the store; the
//!   replacement is an atomic rename (no-replace when absent).
//! * Releases must match an explicit manifest SHA-256 and `release_id`;
//!   rollback verifies the old release the same way before switching.
//! * The absolute trusted control adapter must acknowledge every PrepareUpgrade,
//!   Ready, maintenance and drain request; missing/untrusted/protocol-mismatched
//!   or `success:false` replies reject the step.
//! * Standalone Panel readiness additionally proves the *bytes* running in the
//!   Panel unit: the selector's strictly parsed release manifest must name one
//!   executable `Kind::Panel` at `bin/echo-agent-panel`, and the live
//!   `MainPID`'s `/proc/<pid>/exe` must hash to that manifest digest. A
//!   transport that cannot make the observation must reject, never pass on
//!   `ActiveState=active` alone.
//!
//! Adapter protocol: direct execution, no args, JSON request on stdin
//! (`protocol=echo-deploy-control/1`, `operation`, `instance`, optional
//! `release`/`manifest_sha256`) and one JSON reply on stdout echoing
//! protocol/operation/instance and `success:true` plus operation-specific
//! `ready`, `maintenance`, `active_work`/observation fields. Unknown reply
//! fields are rejected. Tests never execute systemctl or an adapter; they inject
//! a [`Transport`]. Same-UID ancestor/symlink races are outside this threat model.

use crate::model::{Artifact, Kind, Release};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CString, OsString},
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{symlink, MetadataExt, OpenOptionsExt},
        process::{CommandExt, ExitStatusExt},
    },
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

pub const SYSTEMCTL_PROGRAM: &str = "/usr/bin/systemctl";
pub const CONTROL_PROTOCOL: &str = "echo-deploy-control/1";
pub const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_ADAPTER_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_ADAPTER_RESPONSE_BYTES: usize = 256 * 1024;

const MAX_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_PROC_STAT_BYTES: u64 = 64 * 1024;
const MAX_PROCESS_IMAGE_BYTES: u64 = 1024 * 1024 * 1024;
const PROCESS_IMAGE_BUFFER_BYTES: usize = 64 * 1024;
/// Canonical standalone Panel executable inside a release directory.
pub const PANEL_IMAGE_PATH: &str = "bin/echo-agent-panel";
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_DEADLINE: Duration = Duration::from_secs(3600);
const FIXED_PATH: &str = "/usr/bin:/bin";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    User,
    System,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemctlVerb {
    Start,
    Stop,
    /// `show --property=ActiveState --value`, which exits zero for every state.
    ActiveState,
    /// `show --property=MainPID --value`; `0` means no main process.
    MainPid,
}
/// Observed systemd state. Unknown strings are rejected, never read as stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveState {
    Active,
    Reloading,
    Inactive,
    Failed,
    Activating,
    Deactivating,
}
impl ActiveState {
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value.trim() {
            "active" => Self::Active,
            "reloading" => Self::Reloading,
            "inactive" => Self::Inactive,
            "failed" => Self::Failed,
            "activating" => Self::Activating,
            "deactivating" => Self::Deactivating,
            other => bail!("unexpected systemd ActiveState {other:?}"),
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlines {
    pub systemctl: Duration,
    pub stop_confirm: Duration,
    pub start_ready: Duration,
    pub control: Duration,
}
impl Deadlines {
    pub const fn new(
        systemctl: Duration,
        stop_confirm: Duration,
        start_ready: Duration,
        control: Duration,
    ) -> Self {
        Self {
            systemctl,
            stop_confirm,
            start_ready,
            control,
        }
    }
    pub fn validate(&self) -> Result<()> {
        for (label, value) in [
            ("systemctl", self.systemctl),
            ("stop_confirm", self.stop_confirm),
            ("start_ready", self.start_ready),
            ("control", self.control),
        ] {
            ensure!(!value.is_zero(), "{label} deadline must be non-zero");
            ensure!(value <= MAX_DEADLINE, "{label} deadline is too large");
        }
        Ok(())
    }
}
impl Default for Deadlines {
    fn default() -> Self {
        Self::new(
            Duration::from_secs(30),
            Duration::from_secs(120),
            Duration::from_secs(120),
            Duration::from_secs(60),
        )
    }
}
/// Immutable, validated description of one systemd instance.
///
/// Paths are not created here. The control adapter is rechecked before every
/// operation, so removing or replacing it disables the backend.
#[derive(Debug, Clone)]
pub struct SystemdSpec {
    instance: String,
    scope: Scope,
    core_unit: String,
    panel_unit: Option<String>,
    unit_directory: PathBuf,
    state_directory: PathBuf,
    current_selector: PathBuf,
    store: PathBuf,
    control_adapter: PathBuf,
    deadlines: Deadlines,
}

#[allow(clippy::too_many_arguments)]
impl SystemdSpec {
    pub fn new(
        instance: impl Into<String>,
        scope: Scope,
        core_unit: impl Into<String>,
        panel_unit: Option<String>,
        unit_directory: PathBuf,
        state_directory: PathBuf,
        current_selector: PathBuf,
        store: PathBuf,
        control_adapter: PathBuf,
        deadlines: Deadlines,
    ) -> Result<Self> {
        let instance = instance.into();
        let core_unit = core_unit.into();
        safe_id(&instance)?;
        validate_unit_name(&core_unit, "core_unit")?;
        if let Some(panel) = &panel_unit {
            validate_unit_name(panel, "panel_unit")?;
            ensure!(panel != &core_unit, "panel_unit must differ from core_unit");
        }
        for (label, path) in [
            ("unit_directory", &unit_directory),
            ("state_directory", &state_directory),
            ("current_selector", &current_selector),
            ("store", &store),
            ("control_adapter", &control_adapter),
        ] {
            absolute_normal(path, label)?;
        }
        ensure!(
            unit_directory != state_directory,
            "unit and state directories must differ"
        );
        ensure!(
            state_directory != store,
            "state directory and store must differ"
        );
        ensure!(
            current_selector.starts_with(&state_directory) && current_selector != state_directory,
            "current_selector must be a descendant of state_directory"
        );
        ensure!(
            !control_adapter.starts_with(&store),
            "control_adapter must not live inside the candidate store"
        );
        deadlines.validate()?;
        Ok(Self {
            instance,
            scope,
            core_unit,
            panel_unit,
            unit_directory,
            state_directory,
            current_selector,
            store,
            control_adapter,
            deadlines,
        })
    }
    pub fn instance(&self) -> &str {
        &self.instance
    }
    pub fn scope(&self) -> Scope {
        self.scope
    }
    pub fn core_unit(&self) -> &str {
        &self.core_unit
    }
    pub fn panel_unit(&self) -> Option<&str> {
        self.panel_unit.as_deref()
    }
    pub fn unit_directory(&self) -> &Path {
        &self.unit_directory
    }
    pub fn state_directory(&self) -> &Path {
        &self.state_directory
    }
    pub fn current_selector(&self) -> &Path {
        &self.current_selector
    }
    pub fn store(&self) -> &Path {
        &self.store
    }
    pub fn control_adapter(&self) -> &Path {
        &self.control_adapter
    }
    pub fn deadlines(&self) -> Deadlines {
        self.deadlines
    }

    /// Recheck the trusted adapter. Missing, symlinked, non-regular,
    /// group/other-writable, non-executable or foreign-owned files are rejected.
    pub fn verify_control_adapter(&self) -> Result<()> {
        let meta = fs::symlink_metadata(&self.control_adapter)
            .with_context(|| format!("control_adapter {}", self.control_adapter.display()))?;
        ensure!(
            meta.file_type().is_file(),
            "control_adapter must be a regular file"
        );
        ensure!(
            meta.mode() & 0o022 == 0,
            "control_adapter must not be group/other writable"
        );
        ensure!(
            meta.mode() & 0o111 != 0,
            "control_adapter must be executable"
        );
        // SAFETY: geteuid has no preconditions.
        let euid = unsafe { libc::geteuid() };
        ensure!(
            meta.uid() == euid || meta.uid() == 0,
            "control_adapter must be owned by the current user or root"
        );
        Ok(())
    }
}
/// A release whose manifest digest and id were verified. Private fields mean
/// only [`verify_release_directory`] can produce this proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRelease {
    release_id: String,
    manifest_sha256: String,
    directory: PathBuf,
}
impl VerifiedRelease {
    pub fn release_id(&self) -> &str {
        &self.release_id
    }
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorSwitch {
    pub previous: Option<PathBuf>,
    pub current: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeObservation {
    pub current_release: Option<String>,
    pub selector_release: Option<String>,
    pub maintenance: bool,
    pub ready: bool,
    pub active_work: u32,
}
impl RuntimeObservation {
    pub fn selector_matches(&self) -> bool {
        self.current_release == self.selector_release
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    Drained,
    Busy { remaining: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlOperation {
    PrepareUpgrade,
    Ready,
    MaintenanceEnter,
    MaintenanceLeave,
    Drain,
    Observe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    pub protocol: String,
    pub operation: ControlOperation,
    pub instance: String,
    pub release: Option<String>,
    pub manifest_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlReply {
    pub protocol: String,
    pub operation: ControlOperation,
    pub instance: String,
    pub success: bool,
    #[serde(default)]
    pub current_release: Option<String>,
    #[serde(default)]
    pub maintenance: Option<bool>,
    #[serde(default)]
    pub ready: Option<bool>,
    #[serde(default)]
    pub active_work: Option<u32>,
    #[serde(default)]
    pub error: Option<String>,
}

impl ControlReply {
    pub fn ack(operation: ControlOperation, instance: impl Into<String>) -> Self {
        Self {
            protocol: CONTROL_PROTOCOL.into(),
            operation,
            instance: instance.into(),
            success: true,
            current_release: None,
            maintenance: None,
            ready: None,
            active_work: None,
            error: None,
        }
    }
    pub fn observed(
        instance: impl Into<String>,
        current_release: Option<String>,
        maintenance: bool,
        ready: bool,
        active_work: u32,
    ) -> Self {
        Self {
            current_release,
            maintenance: Some(maintenance),
            ready: Some(ready),
            active_work: Some(active_work),
            ..Self::ack(ControlOperation::Observe, instance)
        }
    }
}

/// Injection point for systemctl and adapter observations. Production uses
/// [`ExecTransport`]; tests use an in-memory fake.
pub trait Transport {
    fn start_unit(&mut self, spec: &SystemdSpec, unit: &str, deadline: Duration) -> Result<()>;
    fn stop_unit(&mut self, spec: &SystemdSpec, unit: &str, deadline: Duration) -> Result<()>;
    fn active_state(
        &mut self,
        spec: &SystemdSpec,
        unit: &str,
        deadline: Duration,
    ) -> Result<ActiveState>;
    fn control(
        &mut self,
        spec: &SystemdSpec,
        request: &ControlRequest,
        deadline: Duration,
    ) -> Result<ControlReply>;

    /// Prove that the process currently serving `unit` executes exactly the
    /// image whose SHA-256 is `expected_sha256`. A transport that cannot make
    /// this observation must return an error: standalone Panel readiness is
    /// never inferred from `ActiveState=active` alone. The default rejects so
    /// existing injectable transports stay compilable without silently
    /// weakening the readiness gate.
    fn verify_process_image(
        &mut self,
        _spec: &SystemdSpec,
        _unit: &str,
        _expected_sha256: &str,
        _deadline: Duration,
    ) -> Result<()> {
        bail!("transport does not support process image verification")
    }
}

pub struct SystemdBackend<T> {
    spec: SystemdSpec,
    transport: T,
}

impl<T: Transport> SystemdBackend<T> {
    pub fn new(spec: SystemdSpec, transport: T) -> Result<Self> {
        spec.verify_control_adapter()?;
        Ok(Self { spec, transport })
    }
    pub fn spec(&self) -> &SystemdSpec {
        &self.spec
    }
    pub fn transport(&self) -> &T {
        &self.transport
    }
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }
    pub fn current_release(&self) -> Result<Option<String>> {
        read_current_release(&self.spec)
    }

    /// Read-only runtime truth. `current_release` is what the adapter reports;
    /// `selector_release` is the pointer. They may differ transiently (crash or
    /// failed start), so the mismatch is reported rather than hidden.
    pub fn observe(&mut self) -> Result<RuntimeObservation> {
        let selector_release = read_current_release(&self.spec)?;
        let reply = self.control(ControlOperation::Observe, None, None)?;
        Ok(RuntimeObservation {
            current_release: reply.current_release.clone(),
            selector_release,
            maintenance: reply.maintenance.context("missing maintenance state")?,
            ready: reply.ready.context("missing readiness state")?,
            active_work: reply.active_work.context("missing active work count")?,
        })
    }
    /// Read-only proof for an explicit first install. Missing control sockets
    /// are not evidence that configured systemd units have stopped.
    pub fn verify_units_inactive(&mut self) -> Result<Vec<String>> {
        self.spec.verify_control_adapter()?;
        let mut checked = Vec::new();
        for unit in self.stop_order() {
            let state =
                self.transport
                    .active_state(&self.spec, &unit, self.spec.deadlines.systemctl)?;
            ensure!(
                state == ActiveState::Inactive,
                "first install requires inactive unit {unit}, observed {state:?}"
            );
            checked.push(unit);
        }
        Ok(checked)
    }

    pub fn prepare_upgrade(&mut self, release: &str, sha: &str) -> Result<VerifiedRelease> {
        let verified = verify_release_directory(&self.spec, release, sha)?;
        self.control(ControlOperation::PrepareUpgrade, Some(release), Some(sha))?;
        Ok(verified)
    }
    pub fn enter_maintenance(&mut self) -> Result<()> {
        self.control(ControlOperation::MaintenanceEnter, None, None)?;
        Ok(())
    }
    pub fn leave_maintenance(&mut self) -> Result<()> {
        self.control(ControlOperation::MaintenanceLeave, None, None)?;
        Ok(())
    }

    /// Checked drain: active work is reported, never killed.
    pub fn drain(&mut self) -> Result<DrainOutcome> {
        let reply = self.control(ControlOperation::Drain, None, None)?;
        let remaining = reply.active_work.context("missing active work count")?;
        Ok(if remaining == 0 {
            DrainOutcome::Drained
        } else {
            DrainOutcome::Busy { remaining }
        })
    }

    /// Stop Panel before Core; success requires observed `inactive` for each.
    /// The `systemctl stop --no-block` client timing out is never success.
    pub fn stop_units(&mut self) -> Result<()> {
        self.spec.verify_control_adapter()?;
        for unit in self.stop_order() {
            self.transport
                .stop_unit(&self.spec, &unit, self.spec.deadlines.systemctl)
                .with_context(|| format!("systemctl stop {unit}"))?;
            self.wait_for_state(
                &unit,
                ActiveState::Inactive,
                self.spec.deadlines.stop_confirm,
            )?;
        }
        Ok(())
    }

    /// Start Core before Panel; success requires observed `active` for each.
    pub fn start_units(&mut self) -> Result<()> {
        self.spec.verify_control_adapter()?;
        for unit in self.start_order() {
            self.transport
                .start_unit(&self.spec, &unit, self.spec.deadlines.systemctl)
                .with_context(|| format!("systemctl start {unit}"))?;
            self.wait_for_state(&unit, ActiveState::Active, self.spec.deadlines.start_ready)?;
        }
        Ok(())
    }

    /// Both units must stay active and the read-only adapter Ready probe must
    /// succeed within one total deadline. Type=simple can become active before
    /// its control socket exists; never retry mutations to solve that race.
    /// For a standalone instance, after the Core adapter confirms Ready the
    /// Panel process image must hash to the selected release's Panel artifact
    /// within the *same* deadline. Unsupported image transports reject.
    pub fn verify_ready(&mut self, release: &str) -> Result<()> {
        self.spec.verify_control_adapter()?;
        safe_id(release)?;
        let panel_unit = self.spec.panel_unit().map(str::to_owned);
        // The expected Panel image is read from the *selected* release's
        // strict manifest before any readiness probe. `ActiveState=active` is
        // not evidence that the running bytes are the release bytes.
        let panel_sha256 = match panel_unit {
            Some(_) => Some(panel_image_sha_for_selected_release(&self.spec, release)?),
            None => None,
        };
        let deadline = Instant::now()
            .checked_add(self.spec.deadlines.start_ready)
            .context("readiness deadline is too large")?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "application readiness deadline elapsed"
            );
            for unit in self.start_order() {
                let budget = self
                    .spec
                    .deadlines
                    .systemctl
                    .min(deadline.saturating_duration_since(Instant::now()));
                ensure!(
                    !budget.is_zero(),
                    "readiness deadline elapsed querying units"
                );
                let state = self.transport.active_state(&self.spec, &unit, budget)?;
                ensure!(
                    state == ActiveState::Active,
                    "unit {unit} is not active during readiness: {state:?}"
                );
            }
            let request = ControlRequest {
                protocol: CONTROL_PROTOCOL.into(),
                operation: ControlOperation::Ready,
                instance: self.spec.instance.clone(),
                release: Some(release.into()),
                manifest_sha256: None,
            };
            let budget = self
                .spec
                .deadlines
                .control
                .min(deadline.saturating_duration_since(Instant::now()));
            ensure!(!budget.is_zero(), "application readiness deadline elapsed");
            let attempt = self
                .transport
                .control(&self.spec, &request, budget)
                .and_then(|reply| validate_control_reply(&request, &reply));
            match attempt {
                Ok(()) => {
                    if let (Some(unit), Some(expected_sha256)) =
                        (panel_unit.as_deref(), panel_sha256.as_deref())
                    {
                        // Core's control adapter has confirmed readiness first.
                        // The Panel image proof shares the one readiness
                        // deadline instead of extending it.
                        let budget = deadline.saturating_duration_since(Instant::now());
                        ensure!(
                            !budget.is_zero(),
                            "application readiness deadline elapsed before panel image verification"
                        );
                        self.transport
                            .verify_process_image(&self.spec, unit, expected_sha256, budget)
                            .context("panel process image does not match the selected release")?;
                    }
                    return Ok(());
                }
                Err(error) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(error).context("application readiness deadline elapsed");
                    }
                    thread::sleep(POLL_INTERVAL.min(remaining));
                }
            }
        }
    }

    /// Verify and atomically point the selector at one staged release.
    pub fn switch_release(&mut self, release: &str, sha: &str) -> Result<SelectorSwitch> {
        self.spec.verify_control_adapter()?;
        let verified = verify_release_directory(&self.spec, release, sha)?;
        switch_current_selector(&self.spec, &verified)
    }

    /// Safe activate step: maintenance and drained must be observed, then the
    /// verified release is switched, started and confirmed ready. Maintenance
    /// is not released.
    pub fn activate_release(&mut self, release: &str, sha: &str) -> Result<VerifiedRelease> {
        let verified = verify_release_directory(&self.spec, release, sha)?;
        self.spec.verify_control_adapter()?;
        let observation = self.observe()?;
        ensure!(
            observation.selector_matches(),
            "activation requires selector and runtime agreement"
        );
        ensure!(
            observation.maintenance,
            "activation requires maintenance mode"
        );
        ensure!(
            observation.active_work == 0,
            "activation requires a drained instance"
        );
        switch_current_selector(&self.spec, &verified)?;
        self.start_units()?;
        self.verify_ready(&verified.release_id)?;
        Ok(verified)
    }

    /// Roll back: verify the old release SHA first, require maintenance and
    /// drain, then switch/start/verify. Maintenance release stays with caller.
    pub fn rollback_to(&mut self, previous: &str, previous_sha: &str) -> Result<VerifiedRelease> {
        let verified = verify_release_directory(&self.spec, previous, previous_sha)?;
        self.spec.verify_control_adapter()?;
        let observation = self.observe()?;
        ensure!(
            observation.maintenance,
            "rollback requires maintenance mode"
        );
        ensure!(
            observation.active_work == 0,
            "rollback requires a drained instance"
        );
        switch_current_selector(&self.spec, &verified)?;
        self.start_units()?;
        self.verify_ready(&verified.release_id)?;
        Ok(verified)
    }
    fn control(
        &mut self,
        operation: ControlOperation,
        release: Option<&str>,
        sha: Option<&str>,
    ) -> Result<ControlReply> {
        self.spec.verify_control_adapter()?;
        if let Some(release) = release {
            safe_id(release)?;
        }
        if let Some(sha) = sha {
            validate_digest(sha)?;
        }
        let request = ControlRequest {
            protocol: CONTROL_PROTOCOL.into(),
            operation,
            instance: self.spec.instance.clone(),
            release: release.map(str::to_owned),
            manifest_sha256: sha.map(str::to_owned),
        };
        let reply = {
            let Self { spec, transport } = self;
            transport.control(spec, &request, spec.deadlines.control)?
        };
        validate_control_reply(&request, &reply)?;
        Ok(reply)
    }
    fn stop_order(&self) -> Vec<String> {
        let mut units: Vec<String> = self.spec.panel_unit.iter().cloned().collect();
        units.push(self.spec.core_unit.clone());
        units
    }
    fn start_order(&self) -> Vec<String> {
        let mut units = vec![self.spec.core_unit.clone()];
        units.extend(self.spec.panel_unit.iter().cloned());
        units
    }
    fn wait_for_state(&mut self, unit: &str, expected: ActiveState, total: Duration) -> Result<()> {
        let deadline = Instant::now()
            .checked_add(total)
            .context("state deadline is too large")?;
        loop {
            let state = self
                .transport
                .active_state(&self.spec, unit, self.spec.deadlines.systemctl)
                .with_context(|| format!("query ActiveState of {unit}"))?;
            if state == expected {
                return Ok(());
            }
            ensure!(
                state != ActiveState::Failed,
                "unit {unit} entered failed state instead of {expected:?}"
            );
            let now = Instant::now();
            ensure!(
                now < deadline,
                "unit {unit} is {state:?}, did not reach {expected:?} within {total:?}"
            );
            thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
        }
    }
}

fn validate_control_reply(request: &ControlRequest, reply: &ControlReply) -> Result<()> {
    ensure!(
        reply.protocol == CONTROL_PROTOCOL,
        "control adapter protocol mismatch"
    );
    ensure!(
        reply.operation == request.operation,
        "control adapter operation mismatch"
    );
    ensure!(
        reply.instance == request.instance,
        "control adapter instance mismatch"
    );
    if !reply.success {
        let detail = reply.error.as_deref().unwrap_or("no error detail");
        bail!(
            "control adapter rejected {:?}: {}",
            request.operation,
            bounded_text(detail, 512)
        );
    }
    if let Some(release) = reply.current_release.as_deref() {
        safe_id(release)?;
    }
    match request.operation {
        ControlOperation::Observe => ensure!(
            reply.maintenance.is_some() && reply.ready.is_some() && reply.active_work.is_some(),
            "control adapter observation is incomplete"
        ),
        ControlOperation::Drain => {
            ensure!(reply.active_work.is_some(), "missing active work count")
        }
        ControlOperation::Ready => {
            ensure!(
                reply.ready == Some(true),
                "control adapter did not confirm readiness"
            );
            if let Some(reported) = reply.current_release.as_deref() {
                let expected = request
                    .release
                    .as_deref()
                    .context("ready without release")?;
                ensure!(
                    reported == expected,
                    "control adapter reports release {reported:?}, expected {expected:?}"
                );
            }
        }
        ControlOperation::MaintenanceEnter => ensure!(
            reply.maintenance == Some(true),
            "control adapter did not confirm maintenance mode"
        ),
        ControlOperation::MaintenanceLeave => ensure!(
            reply.maintenance == Some(false),
            "control adapter did not confirm maintenance release"
        ),
        ControlOperation::PrepareUpgrade => {}
    }
    Ok(())
}

/// Verify `<store>/<release>/release.json` against explicit digest and id.
pub fn verify_release_directory(
    spec: &SystemdSpec,
    release: &str,
    expected_sha: &str,
) -> Result<VerifiedRelease> {
    safe_id(release)?;
    validate_digest(expected_sha)?;
    let directory = spec.store.join(release);
    let meta =
        fs::symlink_metadata(&directory).with_context(|| format!("staged release {release}"))?;
    ensure!(meta.is_dir(), "staged release must be a real directory");
    let manifest = read_nofollow(&directory.join("release.json"), MAX_MANIFEST_BYTES)
        .context("read release.json")?;
    let actual = hex_sha256(&manifest);
    ensure!(
        actual == expected_sha,
        "release manifest digest mismatch for {release}"
    );
    // Parse the same strict `crate::model::Release` shape used by staging and
    // transactions. A digest alone only proves we read the expected bytes, not
    // that those bytes describe a valid release.
    let parsed: Release =
        serde_json::from_slice(&manifest).context("release manifest is not a strict Release")?;
    parsed
        .validate()
        .context("release manifest failed model validation")?;
    ensure!(
        parsed.release_id == release,
        "release manifest id does not match directory {release}"
    );
    // Reject a Panel artifact that is not unique/canonical even before any
    // process is started. A release may legitimately have no Panel artifact
    // (core-only/embedded topologies). When present, hash the actual staged
    // bytes so a manifest digest alone can never stand in for the image.
    if let Some(panel) = panel_artifact(&parsed)? {
        verify_panel_artifact_bytes(&directory, panel)?;
    }
    Ok(VerifiedRelease {
        release_id: release.into(),
        manifest_sha256: actual,
        directory,
    })
}

/// Read the expected Panel image digest from the release currently named by
/// the selector. The manifest must parse as a strict [`Release`], and the
/// release must contain exactly one executable `Kind::Panel` artifact at
/// [`PANEL_IMAGE_PATH`]. A release without a Panel artifact is not a valid
/// standalone release and is rejected here.
fn panel_image_sha_for_selected_release(spec: &SystemdSpec, release: &str) -> Result<String> {
    let selected =
        read_current_release(spec)?.context("standalone readiness requires a selected release")?;
    ensure!(
        selected == release,
        "selected release {selected:?} does not match readiness release {release:?}"
    );
    let directory = spec.store.join(release);
    let meta =
        fs::symlink_metadata(&directory).with_context(|| format!("selected release {release}"))?;
    ensure!(meta.is_dir(), "selected release must be a real directory");
    let manifest = read_nofollow(&directory.join("release.json"), MAX_MANIFEST_BYTES)
        .context("read selected release.json")?;
    let parsed: Release = serde_json::from_slice(&manifest)
        .context("selected release manifest is not a strict Release")?;
    parsed
        .validate()
        .context("selected release manifest failed model validation")?;
    ensure!(
        parsed.release_id == release,
        "selected release manifest id does not match directory {release}"
    );
    let panel = panel_artifact(&parsed)?.context("standalone release has no panel artifact")?;
    verify_panel_artifact_bytes(&directory, panel)?;
    Ok(panel.sha256.clone())
}

/// Hash the staged Panel image through a no-follow descriptor and compare it
/// with the artifact's declared size/digest. Symlinked or special files are
/// rejected; this is the staged half of the running-image proof.
fn verify_panel_artifact_bytes(directory: &Path, panel: &Artifact) -> Result<()> {
    let path = directory.join(&panel.path);
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .with_context(|| format!("open panel artifact {}", path.display()))?;
    let meta = file.metadata().context("stat panel artifact")?;
    ensure!(meta.is_file(), "panel artifact must be a regular file");
    ensure!(
        meta.len() == panel.size,
        "panel artifact size mismatch for {:?}",
        panel.path
    );
    ensure!(
        meta.mode() & 0o111 != 0,
        "panel artifact must be executable"
    );
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = vec![0u8; PROCESS_IMAGE_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer).context("read panel artifact")?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("panel artifact size overflow")?;
        ensure!(total <= panel.size, "panel artifact grew while hashing");
        hasher.update(&buffer[..read]);
    }
    ensure!(
        total == panel.size,
        "panel artifact size mismatch for {:?}",
        panel.path
    );
    ensure!(
        format!("{:x}", hasher.finalize()) == panel.sha256,
        "panel artifact digest mismatch for {:?}",
        panel.path
    );
    Ok(())
}

/// Return the release's single Panel artifact, if any. Multiple Panel
/// artifacts, a non-canonical path or a non-executable Panel are rejected.
fn panel_artifact(release: &Release) -> Result<Option<&Artifact>> {
    let mut panels = release
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind == Kind::Panel);
    let Some(panel) = panels.next() else {
        return Ok(None);
    };
    ensure!(
        panels.next().is_none(),
        "standalone release must contain exactly one panel artifact"
    );
    ensure!(
        panel.path == PANEL_IMAGE_PATH,
        "panel artifact path must be {PANEL_IMAGE_PATH:?}, found {:?}",
        panel.path
    );
    ensure!(panel.executable, "panel artifact must be executable");
    validate_digest(&panel.sha256)?;
    Ok(Some(panel))
}

pub fn read_current_release(spec: &SystemdSpec) -> Result<Option<String>> {
    let Some(target) = selector_target(spec)? else {
        return Ok(None);
    };
    let release = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("selector target has no release name")?;
    Ok(Some(release.into()))
}

/// Atomically point the selector at the verified release. Existing must be a
/// symlink; its target must be exactly one release directory below the store.
/// The release is re-verified immediately before replacement.
pub fn switch_current_selector(
    spec: &SystemdSpec,
    verified: &VerifiedRelease,
) -> Result<SelectorSwitch> {
    let verified =
        verify_release_directory(spec, verified.release_id(), verified.manifest_sha256())?;
    let parent = spec
        .current_selector
        .parent()
        .context("current_selector has no parent")?;
    let previous = selector_target(spec)?;
    let temp = parent.join(format!(
        ".echo-selector-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    symlink(&verified.directory, &temp).context("create temporary selector symlink")?;
    let result = (|| -> Result<()> {
        if previous.is_some() {
            let meta =
                fs::symlink_metadata(&spec.current_selector).context("stat current_selector")?;
            ensure!(
                meta.file_type().is_symlink(),
                "existing current_selector is not a symlink"
            );
            fs::rename(&temp, &spec.current_selector).context("replace current_selector")?;
        } else {
            rename_noreplace(&temp, &spec.current_selector)?;
        }
        fs::File::open(parent)?
            .sync_all()
            .context("fsync current_selector directory")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    Ok(SelectorSwitch {
        previous,
        current: verified.directory,
    })
}

fn selector_target(spec: &SystemdSpec) -> Result<Option<PathBuf>> {
    match fs::symlink_metadata(&spec.current_selector) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("stat current_selector"),
        Ok(meta) => {
            ensure!(
                meta.file_type().is_symlink(),
                "existing current_selector is not a symlink"
            );
            let target = fs::read_link(&spec.current_selector).context("read current_selector")?;
            validate_selector_target(spec, &target)?;
            Ok(Some(target))
        }
    }
}

fn validate_selector_target(spec: &SystemdSpec, target: &Path) -> Result<()> {
    absolute_normal(target, "current_selector target")?;
    ensure!(
        target.starts_with(&spec.store) && target != spec.store,
        "current_selector target escapes the candidate store"
    );
    let relative = target
        .strip_prefix(&spec.store)
        .context("current_selector target is outside the store")?;
    let mut components = relative.components();
    let Some(Component::Normal(name)) = components.next() else {
        bail!("current_selector target must name one release directory");
    };
    ensure!(
        components.next().is_none(),
        "current_selector target must be exactly one release directory below the store"
    );
    safe_id(name.to_str().context("selector target is not UTF-8")?)
}

fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    let from = CString::new(from.as_os_str().as_bytes()).context("selector temp path")?;
    let to = CString::new(to.as_os_str().as_bytes()).context("selector path")?;
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        bail!("current_selector appeared concurrently; refusing to replace it");
    }
    Err(error).context("install current_selector symlink")
}

/// Build the fixed systemctl argv. No shell, no PATH lookup, and the unit must
/// belong to this immutable spec.
pub fn systemctl_args(
    spec: &SystemdSpec,
    verb: SystemctlVerb,
    unit: &str,
) -> Result<Vec<OsString>> {
    validate_unit_name(unit, "unit")?;
    ensure!(
        unit == spec.core_unit || spec.panel_unit.as_deref() == Some(unit),
        "unit {unit:?} is not part of instance {:?}",
        spec.instance
    );
    let mut args = Vec::with_capacity(8);
    if spec.scope == Scope::User {
        args.push("--user".into());
    }
    for option in ["--no-pager", "--no-ask-password", "--plain"] {
        args.push(option.into());
    }
    match verb {
        SystemctlVerb::Start => args.extend(["start".into(), unit.into()]),
        // Queue the stop; the caller must observe ActiveState=inactive.
        SystemctlVerb::Stop => args.extend(["stop".into(), "--no-block".into(), unit.into()]),
        SystemctlVerb::ActiveState => args.extend([
            "show".into(),
            "--property=ActiveState".into(),
            "--value".into(),
            unit.into(),
        ]),
        SystemctlVerb::MainPid => args.extend([
            "show".into(),
            "--property=MainPID".into(),
            "--value".into(),
            unit.into(),
        ]),
    }
    Ok(args)
}

/// Real transport; never constructed by this module's tests.
pub struct ExecTransport {
    program: PathBuf,
}

impl Default for ExecTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecTransport {
    pub fn new() -> Self {
        Self {
            program: PathBuf::from(SYSTEMCTL_PROGRAM),
        }
    }
    pub fn program(&self) -> &Path {
        &self.program
    }
    fn systemctl(
        &self,
        spec: &SystemdSpec,
        verb: SystemctlVerb,
        unit: &str,
        deadline: Duration,
    ) -> Result<CommandOutput> {
        let mut command = Command::new(&self.program);
        command
            .current_dir("/")
            .env_clear()
            .env("LC_ALL", "C")
            .env("PATH", FIXED_PATH);
        if spec.scope == Scope::User {
            for key in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
        }
        command.args(systemctl_args(spec, verb, unit)?);
        run_bounded(command, deadline, MAX_COMMAND_OUTPUT_BYTES, None)
    }

    fn main_pid(&self, spec: &SystemdSpec, unit: &str, deadline: Duration) -> Result<u32> {
        let output = self.systemctl(spec, SystemctlVerb::MainPid, unit, deadline)?;
        ensure_success(&output, "systemctl show MainPID")?;
        parse_main_pid(&output.stdout).with_context(|| format!("read MainPID for {unit}"))
    }
}

impl Transport for ExecTransport {
    fn start_unit(&mut self, spec: &SystemdSpec, unit: &str, deadline: Duration) -> Result<()> {
        ensure_success(
            &self.systemctl(spec, SystemctlVerb::Start, unit, deadline)?,
            "systemctl start",
        )
    }
    fn stop_unit(&mut self, spec: &SystemdSpec, unit: &str, deadline: Duration) -> Result<()> {
        ensure_success(
            &self.systemctl(spec, SystemctlVerb::Stop, unit, deadline)?,
            "systemctl stop",
        )
    }
    fn active_state(
        &mut self,
        spec: &SystemdSpec,
        unit: &str,
        deadline: Duration,
    ) -> Result<ActiveState> {
        let output = self.systemctl(spec, SystemctlVerb::ActiveState, unit, deadline)?;
        ensure_success(&output, "systemctl show ActiveState")?;
        ActiveState::parse(
            std::str::from_utf8(&output.stdout).context("ActiveState output is not UTF-8")?,
        )
    }
    fn control(
        &mut self,
        spec: &SystemdSpec,
        request: &ControlRequest,
        deadline: Duration,
    ) -> Result<ControlReply> {
        spec.verify_control_adapter()?;
        let payload = serde_json::to_vec(request).context("serialize control request")?;
        ensure!(
            payload.len() <= MAX_ADAPTER_REQUEST_BYTES,
            "control request exceeds 64 KiB"
        );
        let mut command = Command::new(&spec.control_adapter);
        command
            .current_dir("/")
            .env_clear()
            .env("LC_ALL", "C")
            .env("PATH", FIXED_PATH)
            .env("ECHO_DEPLOY_CONTROL_PROTOCOL", CONTROL_PROTOCOL);
        let output = run_bounded(
            command,
            deadline,
            MAX_ADAPTER_RESPONSE_BYTES,
            Some(&payload),
        )?;
        ensure_success(&output, "control adapter")?;
        serde_json::from_slice(&output.stdout).context("control adapter response is not valid JSON")
    }

    /// Shell-free MainPID observation, bounded by one total deadline. The
    /// executable is hashed through `/proc/<pid>/exe` (never a caller-supplied
    /// path) and the PID, process start time and executable inode must all stay
    /// stable across the hash. A MainPID re-query after hashing rejects a
    /// restart that reused a PID; reading through the pinned descriptor rejects
    /// a re-exec in the middle of the hash.
    fn verify_process_image(
        &mut self,
        spec: &SystemdSpec,
        unit: &str,
        expected_sha256: &str,
        deadline: Duration,
    ) -> Result<()> {
        validate_digest(expected_sha256)?;
        ensure!(
            !deadline.is_zero(),
            "process image verification deadline must be non-zero"
        );
        let deadline_at = Instant::now()
            .checked_add(deadline)
            .context("process image verification deadline is too large")?;
        let first_budget = deadline_at.saturating_duration_since(Instant::now());
        ensure!(
            !first_budget.is_zero(),
            "process image verification deadline elapsed"
        );
        let pid = self.main_pid(spec, unit, first_budget)?;
        verify_process_owner(pid, unit)?;
        let start_before = proc_start_time(pid)?;
        let exe_path = PathBuf::from(format!("/proc/{pid}/exe"));
        // O_CLOEXEC only: a descriptor pins the inode even if the process
        // re-execs or the path is replaced while hashing.
        let mut image = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(&exe_path)
            .with_context(|| format!("open process image for {unit} (MainPID {pid})"))?;
        let before = image.metadata().context("stat opened process image")?;
        ensure!(before.is_file(), "process image must be a regular file");
        verify_image_owner(&before)?;
        let identity = ImageIdentity::from_metadata(&before);
        let actual = hash_process_image(&mut image, &before, deadline_at)?;
        let after = image.metadata().context("restat opened process image")?;
        ensure!(
            identity.matches(&after),
            "pinned process image changed while hashing for {unit}"
        );
        let current = fs::metadata(&exe_path)
            .with_context(|| format!("restat process image for {unit} (MainPID {pid})"))?;
        ensure!(
            identity.matches(&current),
            "process image changed while hashing for {unit}"
        );
        let start_after = proc_start_time(pid)?;
        ensure!(
            start_before == start_after,
            "process start time changed while hashing {unit}"
        );
        let recheck_budget = deadline_at.saturating_duration_since(Instant::now());
        ensure!(
            !recheck_budget.is_zero(),
            "process image verification deadline elapsed"
        );
        let current_pid = self.main_pid(spec, unit, recheck_budget)?;
        ensure!(
            current_pid == pid,
            "systemd MainPID changed while verifying {unit}: {pid} -> {current_pid}"
        );
        ensure!(
            actual == expected_sha256,
            "process image digest mismatch for {unit}"
        );
        Ok(())
    }
}

fn parse_main_pid(output: &[u8]) -> Result<u32> {
    let text = std::str::from_utf8(output).context("MainPID output is not UTF-8")?;
    let text = text.trim();
    ensure!(!text.is_empty(), "systemctl show MainPID returned no value");
    let pid = text
        .parse::<u32>()
        .with_context(|| format!("invalid MainPID {text:?}"))?;
    ensure!(pid > 0, "unit has no main process (MainPID=0)");
    ensure!(pid <= i32::MAX as u32, "MainPID is out of range");
    Ok(pid)
}

fn verify_process_owner(pid: u32, unit: &str) -> Result<()> {
    let path = PathBuf::from(format!("/proc/{pid}"));
    let meta =
        fs::metadata(&path).with_context(|| format!("stat {} for {unit}", path.display()))?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    ensure!(
        meta.uid() == euid || meta.uid() == 0,
        "process for {unit} is owned by an untrusted UID"
    );
    Ok(())
}

fn verify_image_owner(meta: &fs::Metadata) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    ensure!(
        meta.uid() == euid || meta.uid() == 0,
        "process image is owned by an untrusted UID"
    );
    ensure!(
        meta.mode() & 0o022 == 0,
        "process image must not be group/other writable"
    );
    ensure!(meta.mode() & 0o111 != 0, "process image must be executable");
    Ok(())
}

fn proc_start_time(pid: u32) -> Result<u64> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let bytes = read_nofollow(&path, MAX_PROC_STAT_BYTES)
        .with_context(|| format!("read {}", path.display()))?;
    let text = std::str::from_utf8(&bytes).context("/proc stat is not UTF-8")?;
    let rest = text
        .rsplit_once(')')
        .map(|(_, rest)| rest)
        .context("malformed /proc stat: missing comm terminator")?;
    let starttime = rest
        .split_whitespace()
        .nth(19)
        .context("malformed /proc stat: missing starttime")?;
    starttime
        .parse::<u64>()
        .context("invalid /proc stat starttime")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl ImageIdentity {
    fn from_metadata(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        }
    }
    fn matches(&self, meta: &fs::Metadata) -> bool {
        *self == Self::from_metadata(meta)
    }
}

fn hash_process_image(
    file: &mut fs::File,
    meta: &fs::Metadata,
    deadline: Instant,
) -> Result<String> {
    ensure!(
        meta.len() <= MAX_PROCESS_IMAGE_BYTES,
        "process image exceeds 1 GiB"
    );
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; PROCESS_IMAGE_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        ensure!(
            Instant::now() < deadline,
            "process image hashing deadline elapsed"
        );
        let read = file.read(&mut buffer).context("read process image")?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("process image size overflow")?;
        ensure!(
            total <= MAX_PROCESS_IMAGE_BYTES,
            "process image exceeds 1 GiB"
        );
        hasher.update(&buffer[..read]);
    }
    ensure!(
        total == meta.len(),
        "process image size changed while hashing"
    );
    Ok(format!("{:x}", hasher.finalize()))
}

fn ensure_success(output: &CommandOutput, what: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let detail = bounded_text(String::from_utf8_lossy(&output.stderr).trim(), 512);
    if detail.is_empty() {
        bail!("{what} failed ({})", describe_status(output.status));
    }
    bail!(
        "{what} failed ({}): {detail}",
        describe_status(output.status)
    )
}

fn describe_status(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        _ => "unknown status".into(),
    }
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run one direct command with a hard deadline and bounded output. On timeout
/// only the child's own process group is killed; no unit is signalled here.
fn run_bounded(
    mut command: Command,
    timeout: Duration,
    output_limit: usize,
    input: Option<&[u8]>,
) -> Result<CommandOutput> {
    ensure!(!timeout.is_zero(), "command deadline must be non-zero");
    ensure!(output_limit > 0, "command output limit must be non-zero");
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("command deadline is too large")?;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().context("spawn bounded command")?;
    let pid = child.id();
    let stdout_rx = spawn_reader(
        child.stdout.take().context("command stdout pipe")?,
        output_limit,
    );
    let stderr_rx = spawn_reader(
        child.stderr.take().context("command stderr pipe")?,
        output_limit,
    );
    if let Some(input) = input {
        let mut stdin = child.stdin.take().context("command stdin pipe")?;
        let input = input.to_vec();
        let _writer = thread::spawn(move || {
            let _ = stdin.write_all(&input);
            let _ = stdin.flush();
        });
    }
    let status = loop {
        if let Some(status) = child.try_wait().context("wait for bounded command")? {
            break status;
        }
        let now = Instant::now();
        if now >= deadline {
            kill_group(pid);
            let _ = child.wait();
            bail!("command timed out after {timeout:?}");
        }
        thread::sleep(Duration::from_millis(5).min(deadline.saturating_duration_since(now)));
    };
    Ok(CommandOutput {
        status,
        stdout: receive_output(stdout_rx, deadline, pid, "stdout")?,
        stderr: receive_output(stderr_rx, deadline, pid, "stderr")?,
    })
}

fn receive_output(
    receiver: Receiver<Result<Vec<u8>>>,
    deadline: Instant,
    pid: u32,
    stream: &str,
) -> Result<Vec<u8>> {
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result.with_context(|| format!("read bounded command {stream}")),
        Err(RecvTimeoutError::Timeout) => {
            kill_group(pid);
            bail!("bounded command {stream} did not close before the deadline")
        }
        Err(RecvTimeoutError::Disconnected) => bail!("bounded command {stream} reader stopped"),
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    mut stream: R,
    limit: usize,
) -> Receiver<Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    let _reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut overflow = false;
        let mut buffer = [0_u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) if bytes.len() + count <= limit => {
                    bytes.extend_from_slice(&buffer[..count])
                }
                Ok(_) => overflow = true,
                Err(error) => {
                    let _ = sender.send(Err(anyhow::Error::new(error)));
                    return;
                }
            }
        }
        let _ = sender.send(if overflow {
            Err(anyhow::anyhow!(
                "bounded command output exceeds {limit} bytes"
            ))
        } else {
            Ok(bytes)
        });
    });
    receiver
}

fn kill_group(pid: u32) {
    if pid > 0 {
        // SAFETY: best-effort kill of the process group created by run_bounded.
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
    }
}

fn read_nofollow(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let meta = file.metadata().context("stat file")?;
    ensure!(meta.is_file(), "expected a regular file");
    ensure!(meta.len() <= limit, "file exceeds size limit");
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut bytes).context("read file")?;
    ensure!(bytes.len() as u64 <= limit, "file grew beyond size limit");
    Ok(bytes)
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn absolute_normal(path: &Path, label: &str) -> Result<()> {
    ensure!(path.is_absolute(), "{label} must be absolute");
    ensure!(
        path.parent().is_some(),
        "{label} must not be the filesystem root"
    );
    let text = path.to_str().context("path must be UTF-8")?;
    let mut parts = text.split('/');
    ensure!(parts.next() == Some(""), "{label} must be absolute");
    for part in parts {
        ensure!(
            !part.is_empty() && part != "." && part != "..",
            "{label} must be a normal absolute path"
        );
    }
    Ok(())
}

fn safe_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 100,
        "invalid identifier length"
    );
    ensure!(
        value.as_bytes()[0].is_ascii_alphanumeric(),
        "identifier must start with alphanumeric"
    );
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
        "unsafe identifier: {value:?}"
    );
    Ok(())
}

fn validate_digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "expected lowercase SHA256"
    );
    Ok(())
}

fn validate_unit_name(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 255,
        "{label} has invalid length"
    );
    ensure!(value.ends_with(".service"), "{label} must end in .service");
    ensure!(
        !value.starts_with('.') && !value.starts_with('-'),
        "{label} must not start with '.' or '-'"
    );
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.@:".contains(&byte)),
        "{label} contains unsupported characters"
    );
    Ok(())
}
