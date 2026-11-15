//! Legacy-unit takeover *preparation* only (never activation).
//!
//! All inputs are explicit: no `$HOME` discovery, no defaults, no production
//! call. The current host's legacy Core root is exactly
//! `/home/evence/.local/share/echo-agent-core/source`; callers pass it in
//! [`LegacyUnit::working_directory`]. `ExecStart` is captured verbatim and
//! compared only for exact equality: binary hashing proves the bytes at the
//! declared path, not that ExecStart launches it. Drop-ins, other environment,
//! and same-euid/root ancestor races are outside this evidence. Reports always
//! say `ready=false` / `activated=false`; new units are never started here.
//! Callers serialize capture/takeover/recover; this module takes no lock.
#![cfg_attr(test, allow(dead_code))]
use crate::{
    model,
    safe_fs::{self, Dir},
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Read,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
pub const MIGRATION_SCHEMA_VERSION: u32 = 1;
pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;
const JOURNAL_SCHEMA_VERSION: u32 = 1;
const SYSTEMCTL_PROGRAM: &str = "/usr/bin/systemctl";
const FIXED_PATH: &str = "/usr/bin:/bin";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_CONFIRM: Duration = Duration::from_secs(120);
const START_CONFIRM: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(25);
const MAX_UNIT_BYTES: u64 = 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_EVIDENCE_BYTES: u64 = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_TEXT: usize = 8192;
const MAX_UNITS: usize = 32;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    User,
    System,
}
/// One explicit declaration. `None` means "not declared", never "discover it".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyUnit {
    pub unit: String,
    pub fragment: PathBuf,
    pub binary: Option<PathBuf>,
    pub working_directory: Option<PathBuf>,
}
/// Complete strict input document. No field is defaulted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationSpec {
    pub schema_version: u32,
    pub instance_id: String,
    pub scope: Scope,
    pub state_dir: PathBuf,
    /// Stopped serially in declaration order. Supply reverse dependency order;
    /// the tool does not infer dependencies or sort unit names.
    pub legacy_units: Vec<LegacyUnit>,
    pub new_units: Vec<String>,
    pub unit_directory: PathBuf,
    /// Trusted tool used by every new unit activation guard.
    pub guard_executable: PathBuf,
}
impl MigrationSpec {
    /// Pure structural validation; no filesystem or service access.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == MIGRATION_SCHEMA_VERSION,
            "unsupported migration schema"
        );
        model::safe_id(&self.instance_id)?;
        ensure!(
            self.instance_id.len() <= 64,
            "instance_id too long for migration file names"
        );
        model::absolute_path(&self.state_dir)?;
        model::absolute_path(&self.unit_directory)?;
        model::absolute_path(&self.guard_executable)?;
        ensure!(
            !self.legacy_units.is_empty() && self.legacy_units.len() <= MAX_UNITS,
            "legacy_units must contain 1..=32 units"
        );
        ensure!(
            !self.new_units.is_empty() && self.new_units.len() <= MAX_UNITS,
            "new_units must contain 1..=32 units"
        );
        ensure!(
            self.state_dir != self.unit_directory,
            "state_dir and unit_directory must differ"
        );
        let mut seen = BTreeSet::new();
        for legacy in &self.legacy_units {
            validate_unit_name(&legacy.unit, "legacy unit")?;
            ensure!(
                seen.insert(legacy.unit.clone()),
                "duplicate unit {:?}",
                legacy.unit
            );
            ensure!(
                !critical_host_unit(&legacy.unit),
                "legacy unit is protected host infrastructure: {}",
                legacy.unit
            );
            model::absolute_path(&legacy.fragment)?;
            if let Some(binary) = &legacy.binary {
                model::absolute_path(binary)?;
            }
            if let Some(directory) = &legacy.working_directory {
                model::absolute_path(directory)?;
            }
        }
        for unit in &self.new_units {
            validate_unit_name(unit, "new unit")?;
            let prefix = format!("echo-agent-{}-", self.instance_id);
            ensure!(
                ["core.service", "panel.service", "update.service"]
                    .iter()
                    .any(|suffix| unit == &format!("{prefix}{suffix}")),
                "new unit must be canonical for this instance: {unit}"
            );
            ensure!(seen.insert(unit.clone()), "duplicate unit {unit:?}");
        }
        Ok(())
    }
    fn check_layout(&self) -> Result<()> {
        self.validate()?;
        ensure!(
            self.scope != Scope::System || euid() == 0,
            "system-scope migration requires root"
        );
        open_dir(&self.state_dir, "state_dir", true)?;
        open_dir(&self.unit_directory, "unit_directory", false)?;
        Ok(())
    }
}
/// Exact unit guard contract. No shell interpretation or implicit path discovery.
pub fn activation_guard_line(
    executable: &Path,
    state_dir: &Path,
    instance_id: &str,
    unit: &str,
) -> Result<String> {
    model::absolute_path(executable)?;
    model::absolute_path(state_dir)?;
    model::safe_id(instance_id)?;
    ensure!(instance_id.len() <= 64, "instance_id too long");
    ensure!(
        ["core", "panel", "update"]
            .iter()
            .any(|role| unit == format!("echo-agent-{instance_id}-{role}.service")),
        "noncanonical activation unit"
    );
    // Keep these escapes identical to systemd ExecStart argv escaping.
    fn quote(value: &str, executable: bool) -> Result<String> {
        ensure!(
            !value.chars().any(char::is_control),
            "control character in guard argument"
        );
        ensure!(
            !executable || !value.chars().any(|c| matches!(c, '\'' | '"' | '\\')),
            "unsupported executable path"
        );
        let mut result = String::from("\"");
        for c in value.chars() {
            match c {
                '%' => result.push_str("%%"),
                '$' if !executable => result.push_str("$$"),
                '\\' => result.push_str("\\\\"),
                '"' => result.push_str("\\\""),
                _ => result.push(c),
            }
        }
        result.push('"');
        Ok(result)
    }
    Ok(format!(
        "ExecStartPre={} migration-mark-activation --state-dir {} --instance-id {} --unit {}",
        quote(executable.to_str().context("non UTF-8 executable")?, true)?,
        quote(state_dir.to_str().context("non UTF-8 state_dir")?, false)?,
        quote(instance_id, false)?,
        quote(unit, false)?
    ))
}

fn activation_marker_name(instance_id: &str) -> String {
    format!("migration-activated-{instance_id}.json")
}

/// Always persist before ExecStart, even when no migration evidence exists yet.
/// Concurrent unit starts share an O_EXCL latch. Never delete this marker to retry.
pub fn mark_activation(state_dir: &Path, instance_id: &str, unit: &str) -> Result<()> {
    activation_guard_line(Path::new("/echo-deploy"), state_dir, instance_id, unit)?;
    let dir = open_dir(state_dir, "state_dir", true)?;
    let leaf = activation_marker_name(instance_id);
    match dir.create(&leaf) {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(&serde_json::to_vec(&serde_json::json!({
                "schema_version": 1, "instance_id": instance_id, "first_unit": unit,
                "activation_may_have_started": true
            }))?)?;
            file.sync_all()?;
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
        {
            // Empty/partial marker after a crash still means activation is unsafe.
            let file = dir.open_file(&leaf, false)?;
            let meta = file.metadata()?;
            ensure!(
                meta.uid() == euid() && meta.mode() & 0o077 == 0,
                "activation marker must be private and owned by caller"
            );
            file.sync_all()?;
        }
        Err(error) => return Err(error.context("persist activation marker")),
    }
    dir.sync()
}

fn require_absent(path: &Path, reason: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => bail!("{reason}"),
        Err(error) => Err(error).context(reason.to_owned()),
    }
}

fn verify_activation_guard(spec: &MigrationSpec, unit: &str, path: &Path) -> Result<()> {
    let parent = safe_fs::Dir::absolute(path.parent().context("new unit parent")?)?;
    let file = parent
        .open_file(
            path.file_name()
                .and_then(|s| s.to_str())
                .context("new unit filename")?,
            false,
        )
        .context("new unit file for activation guard")?;
    let mut bytes = Vec::new();
    file.take(MAX_UNIT_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_UNIT_BYTES,
        "guarded unit too large"
    );
    let text = std::str::from_utf8(&bytes).context("non UTF-8 unit")?;
    let expected = activation_guard_line(
        &spec.guard_executable,
        &spec.state_dir,
        &spec.instance_id,
        unit,
    )?;
    let mut service = false;
    let mut guards = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            service = line == "[Service]";
        }
        if service && line.starts_with("ExecStartPre") {
            guards.push(line);
        }
    }
    ensure!(
        guards == vec![expected.as_str()],
        "new unit {unit} must have the exact durable activation guard for this state_dir/instance"
    );
    Ok(())
}

/// Makes the ExecStart limitation visible in durable evidence, not just prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecStartAssurance {
    OpaqueEqualityOnly,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OldUnitEvidence {
    pub unit: String,
    pub fragment_path: PathBuf,
    pub fragment_sha256: String,
    pub fragment_mode: u32,
    pub fragment_uid: u32,
    pub main_pid: u32,
    pub active_state: String,
    pub unit_file_state: String,
    pub working_directory: String,
    pub exec_start: String,
    pub exec_start_assurance: ExecStartAssurance,
    pub binary_path: Option<PathBuf>,
    pub binary_sha256: Option<String>,
    pub binary_mode: Option<u32>,
    pub binary_uid: Option<u32>,
}
impl OldUnitEvidence {
    pub fn was_active(&self) -> bool {
        matches!(
            self.active_state.as_str(),
            "active" | "reloading" | "activating"
        )
    }
    pub fn was_enabled(&self) -> bool {
        matches!(self.unit_file_state.as_str(), "enabled" | "enabled-runtime")
    }
    pub fn was_disabled(&self) -> bool {
        matches!(
            self.unit_file_state.as_str(),
            "disabled" | "disabled-runtime"
        )
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewUnitEvidence {
    pub unit: String,
    pub path: PathBuf,
    pub sha256: String,
    pub mode: u32,
    pub uid: u32,
    pub unit_file_state: String,
    pub active_state: String,
}
/// Immutable-by-construction capture: private fields, borrowed by takeover and
/// recover, and published as a new `0600` JSON with `O_EXCL`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    schema_version: u32,
    spec: MigrationSpec,
    captured_unix: u64,
    old_units: Vec<OldUnitEvidence>,
    new_units: Vec<NewUnitEvidence>,
}
impl Evidence {
    pub fn spec(&self) -> &MigrationSpec {
        &self.spec
    }
    pub fn old_units(&self) -> &[OldUnitEvidence] {
        &self.old_units
    }
    pub fn new_units(&self) -> &[NewUnitEvidence] {
        &self.new_units
    }
    pub fn old(&self, unit: &str) -> Option<&OldUnitEvidence> {
        self.old_units.iter().find(|entry| entry.unit == unit)
    }
    pub fn digest(&self) -> Result<String> {
        Ok(sha256(
            &serde_json::to_vec(self).context("serialize evidence")?,
        ))
    }
    pub fn load(spec: &MigrationSpec) -> Result<Self> {
        spec.validate()?;
        let dir = open_dir(&spec.state_dir, "state_dir", true)?;
        let leaf = format!("migration-evidence-{}.json", spec.instance_id);
        let evidence: Self =
            serde_json::from_slice(&read_private_json(&dir, &leaf, "migration evidence")?)
                .context("parse migration evidence")?;
        evidence.validate(spec)?;
        Ok(evidence)
    }
    fn validate(&self, spec: &MigrationSpec) -> Result<()> {
        ensure!(
            self.schema_version == EVIDENCE_SCHEMA_VERSION,
            "unsupported evidence schema"
        );
        ensure!(
            self.spec == *spec,
            "evidence is bound to a different migration spec"
        );
        spec.validate()?;
        ensure!(
            self.old_units.len() == spec.legacy_units.len()
                && self.new_units.len() == spec.new_units.len(),
            "evidence unit count changed"
        );
        for (entry, declared) in self.old_units.iter().zip(&spec.legacy_units) {
            ensure!(
                entry.unit == declared.unit && entry.fragment_path == declared.fragment,
                "legacy evidence does not match declaration"
            );
            model::digest(&entry.fragment_sha256)?;
            ensure!(
                entry.fragment_mode <= 0o7777 && entry.fragment_uid == euid(),
                "invalid fragment metadata"
            );
            ensure!(
                active_state_ok(&entry.active_state) && unit_file_state_ok(&entry.unit_file_state),
                "invalid captured state"
            );
            ensure!(
                entry.exec_start.len() <= MAX_TEXT
                    && entry.exec_start_assurance == ExecStartAssurance::OpaqueEqualityOnly,
                "invalid ExecStart evidence"
            );
            let expected = declared
                .working_directory
                .as_ref()
                .and_then(|path| path.to_str())
                .unwrap_or_default();
            ensure!(
                entry.working_directory == expected,
                "WorkingDirectory evidence does not match declaration"
            );
            match (
                &declared.binary,
                &entry.binary_path,
                &entry.binary_sha256,
                entry.binary_mode,
                entry.binary_uid,
            ) {
                (None, None, None, None, None) => {}
                (Some(path), Some(captured), Some(hash), Some(mode), Some(uid)) => {
                    ensure!(
                        path == captured && mode <= 0o7777 && uid == euid(),
                        "binary evidence does not match declaration"
                    );
                    model::digest(hash)?;
                }
                _ => bail!("binary evidence is incomplete"),
            }
        }
        for (entry, unit) in self.new_units.iter().zip(&spec.new_units) {
            ensure!(
                entry.unit == *unit && entry.path == spec.unit_directory.join(unit),
                "new unit evidence does not match declaration"
            );
            model::digest(&entry.sha256)?;
            ensure!(
                entry.mode <= 0o7777 && entry.uid == euid(),
                "invalid new unit metadata"
            );
            ensure!(
                matches!(
                    entry.unit_file_state.as_str(),
                    "disabled" | "disabled-runtime"
                ),
                "invalid captured new UnitFileState"
            );
            ensure!(
                entry.active_state == "inactive",
                "new unit was not inactive at capture"
            );
        }
        Ok(())
    }
    /// Read-only preview derived solely from durable evidence.
    pub fn preview(&self) -> Preview {
        Preview {
            instance_id: self.spec.instance_id.clone(),
            evidence_path: evidence_path(&self.spec),
            stop_units: self
                .old_units
                .iter()
                .map(|entry| entry.unit.clone())
                .collect(),
            disable_units: self
                .old_units
                .iter()
                .filter(|entry| entry.was_enabled())
                .map(|entry| entry.unit.clone())
                .collect(),
            enable_units: self
                .new_units
                .iter()
                .map(|entry| entry.unit.clone())
                .collect(),
            start_on_recovery: self
                .old_units
                .iter()
                .filter(|entry| entry.was_active())
                .map(|entry| entry.unit.clone())
                .collect(),
            takeover_ready: true,
            activation_ready: false,
            notes: notes(),
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub instance_id: String,
    pub evidence_path: PathBuf,
    pub stop_units: Vec<String>,
    pub disable_units: Vec<String>,
    pub enable_units: Vec<String>,
    pub start_on_recovery: Vec<String>,
    pub takeover_ready: bool,
    pub activation_ready: bool,
    pub notes: Vec<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalPhase {
    Prepared,
    Stopping,
    OldStopped,
    Reloading,
    DisablingOld,
    OldDisabled,
    EnablingNew,
    NewEnabled,
    PreparedForActivation,
    RollbackStarted,
    NewStopped,
    OldRestored,
    RolledBack,
    Failed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: u32,
    instance_id: String,
    evidence_sha256: String,
    phase: JournalPhase,
    updated_unix: u64,
}
impl Journal {
    fn load(spec: &MigrationSpec) -> Result<Option<Self>> {
        let path = journal_path(spec);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("stat {}", path.display())),
            Ok(_) => {
                let dir = open_dir(&spec.state_dir, "state_dir", true)?;
                let leaf = format!("migration-journal-{}.json", spec.instance_id);
                let journal: Self =
                    serde_json::from_slice(&read_private_json(&dir, &leaf, "migration journal")?)
                        .context("parse migration journal")?;
                ensure!(
                    journal.schema_version == JOURNAL_SCHEMA_VERSION
                        && journal.instance_id == spec.instance_id,
                    "journal identity mismatch"
                );
                model::digest(&journal.evidence_sha256)?;
                Ok(Some(journal))
            }
        }
    }
    fn save(spec: &MigrationSpec, evidence_sha256: &str, phase: JournalPhase) -> Result<()> {
        let journal = Self {
            schema_version: JOURNAL_SCHEMA_VERSION,
            instance_id: spec.instance_id.clone(),
            evidence_sha256: evidence_sha256.to_owned(),
            phase,
            updated_unix: unix_now(),
        };
        let bytes = serde_json::to_vec_pretty(&journal).context("serialize migration journal")?;
        let dir = open_dir(&spec.state_dir, "state_dir", true)?;
        let temporary = format!(
            "migration-journal-tmp-{}-{}-{}",
            std::process::id(),
            unix_nanos(),
            phase as u8
        );
        dir.write_new(&temporary, &bytes, 0o600)?;
        if let Err(error) = fs::rename(spec.state_dir.join(&temporary), journal_path(spec)) {
            let _ = fs::remove_file(spec.state_dir.join(&temporary));
            return Err(error).context("publish migration journal");
        }
        dir.sync()
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct UnitOutcome {
    pub unit: String,
    pub active_state: String,
    pub unit_file_state: String,
}
/// `ready` and `activated` are always false; activation is systemd-apply's job.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub instance_id: String,
    pub phase: JournalPhase,
    pub ready: bool,
    pub activated: bool,
    pub activation_required: bool,
    pub takeover_complete: bool,
    pub recovered: bool,
    pub old_runtime_restored: bool,
    pub data_modified: bool,
    pub data_rolled_back: bool,
    pub evidence_sha256: String,
    pub old_units: Vec<UnitOutcome>,
    pub new_units: Vec<UnitOutcome>,
    pub next_action: String,
    pub notes: Vec<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitProperty {
    FragmentPath,
    MainPID,
    ActiveState,
    UnitFileState,
    WorkingDirectory,
    ExecStart,
    ExecMainStartTimestampMonotonic,
}
impl UnitProperty {
    fn as_str(self) -> &'static str {
        match self {
            Self::FragmentPath => "FragmentPath",
            Self::MainPID => "MainPID",
            Self::ActiveState => "ActiveState",
            Self::UnitFileState => "UnitFileState",
            Self::WorkingDirectory => "WorkingDirectory",
            Self::ExecStart => "ExecStart",
            Self::ExecMainStartTimestampMonotonic => "ExecMainStartTimestampMonotonic",
        }
    }
}
/// Injection boundary. Tests use a fake; production uses [`ExecTransport`].
pub trait MigrationTransport {
    /// Must prove the stop set cannot terminate the caller or its ancestors.
    /// Unsupported transports fail closed; mocks must opt in explicitly.
    fn verify_caller_safety(&mut self, _spec: &MigrationSpec) -> Result<()> {
        bail!("migration transport does not support caller safety verification")
    }
    fn show(
        &mut self,
        spec: &MigrationSpec,
        unit: &str,
        property: UnitProperty,
        timeout: Duration,
    ) -> Result<String>;
    fn stop_unit(&mut self, spec: &MigrationSpec, unit: &str, timeout: Duration) -> Result<()>;
    fn start_unit(&mut self, spec: &MigrationSpec, unit: &str, timeout: Duration) -> Result<()>;
    fn enable_unit(&mut self, spec: &MigrationSpec, unit: &str, timeout: Duration) -> Result<()>;
    fn disable_unit(&mut self, spec: &MigrationSpec, unit: &str, timeout: Duration) -> Result<()>;
    fn daemon_reload(&mut self, spec: &MigrationSpec, timeout: Duration) -> Result<()>;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SystemctlVerb {
    Show(UnitProperty),
    Stop,
    Start,
    Enable,
    Disable,
    DaemonReload,
}
pub(crate) fn systemctl_argv(
    spec: &MigrationSpec,
    verb: SystemctlVerb,
    unit: Option<&str>,
) -> Result<Vec<OsString>> {
    let mut args: Vec<OsString> = Vec::new();
    if spec.scope == Scope::User {
        args.push("--user".into());
    }
    for option in ["--no-pager", "--no-ask-password", "--plain"] {
        args.push(option.into());
    }
    let check = |unit: &str| -> Result<()> {
        validate_unit_name(unit, "unit")?;
        ensure!(
            spec.legacy_units.iter().any(|entry| entry.unit == unit)
                || spec.new_units.iter().any(|entry| entry == unit),
            "unit {unit:?} is not part of the migration spec"
        );
        Ok(())
    };
    if let SystemctlVerb::DaemonReload = verb {
        ensure!(unit.is_none(), "daemon-reload takes no unit");
        args.push("daemon-reload".into());
        return Ok(args);
    }
    let unit = unit.context("systemctl operation requires a unit")?;
    check(unit)?;
    match verb {
        SystemctlVerb::Show(property) => {
            args.push("show".into());
            args.push(format!("--property={}", property.as_str()).into());
            args.extend(["--value".into(), unit.into()]);
        }
        SystemctlVerb::Stop => args.extend(["stop".into(), "--no-block".into(), unit.into()]),
        SystemctlVerb::Start => args.extend(["start".into(), unit.into()]),
        SystemctlVerb::Enable => args.extend(["enable".into(), unit.into()]),
        SystemctlVerb::Disable => args.extend(["disable".into(), unit.into()]),
        SystemctlVerb::DaemonReload => unreachable!(),
    }
    Ok(args)
}
/// Real transport: fixed `/usr/bin/systemctl`, argv only, no shell/PATH lookup,
/// cleared environment, bounded output and a hard total timeout that kills the
/// child's process group. Only user-scope D-Bus variables are forwarded.
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
    fn run(
        &self,
        spec: &MigrationSpec,
        verb: SystemctlVerb,
        unit: Option<&str>,
        timeout: Duration,
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
        command.args(systemctl_argv(spec, verb, unit)?);
        run_bounded(command, timeout, MAX_OUTPUT_BYTES)
    }
}
impl MigrationTransport for ExecTransport {
    fn verify_caller_safety(&mut self, spec: &MigrationSpec) -> Result<()> {
        let cgroup = fs::read_to_string("/proc/self/cgroup")
            .context("read caller cgroup before migration")?;
        let ancestors = caller_ancestors()?;
        for unit in spec
            .legacy_units
            .iter()
            .map(|entry| entry.unit.as_str())
            .chain(spec.new_units.iter().map(String::as_str))
        {
            ensure!(
                !cgroup.lines().any(|line| line
                    .splitn(3, ':')
                    .nth(2)
                    .is_some_and(|path| path.split('/').any(|part| part == unit))),
                "refusing migration: unit {unit} contains the calling process"
            );
            let pid: u32 = self
                .show(spec, unit, UnitProperty::MainPID, COMMAND_TIMEOUT)?
                .trim()
                .parse()
                .context("invalid migration MainPID")?;
            ensure!(
                pid == 0 || !ancestors.contains(&pid),
                "refusing migration: unit {unit} MainPID {pid} is the caller or its ancestor"
            );
        }
        Ok(())
    }
    fn show(
        &mut self,
        spec: &MigrationSpec,
        unit: &str,
        property: UnitProperty,
        timeout: Duration,
    ) -> Result<String> {
        let output = self.run(spec, SystemctlVerb::Show(property), Some(unit), timeout)?;
        ensure_success(&output, "systemctl show")?;
        String::from_utf8(output.stdout).context("systemctl output is not UTF-8")
    }
    fn stop_unit(&mut self, spec: &MigrationSpec, unit: &str, timeout: Duration) -> Result<()> {
        ensure_success(
            &self.run(spec, SystemctlVerb::Stop, Some(unit), timeout)?,
            "systemctl stop",
        )
    }
    fn start_unit(&mut self, spec: &MigrationSpec, unit: &str, timeout: Duration) -> Result<()> {
        ensure_success(
            &self.run(spec, SystemctlVerb::Start, Some(unit), timeout)?,
            "systemctl start",
        )
    }
    fn enable_unit(&mut self, spec: &MigrationSpec, unit: &str, timeout: Duration) -> Result<()> {
        ensure_success(
            &self.run(spec, SystemctlVerb::Enable, Some(unit), timeout)?,
            "systemctl enable",
        )
    }
    fn disable_unit(&mut self, spec: &MigrationSpec, unit: &str, timeout: Duration) -> Result<()> {
        ensure_success(
            &self.run(spec, SystemctlVerb::Disable, Some(unit), timeout)?,
            "systemctl disable",
        )
    }
    fn daemon_reload(&mut self, spec: &MigrationSpec, timeout: Duration) -> Result<()> {
        ensure_success(
            &self.run(spec, SystemctlVerb::DaemonReload, None, timeout)?,
            "systemctl daemon-reload",
        )
    }
}
/// Capture is read-only with respect to systemd. It queries the six declared
/// legacy properties, snapshots declared files, validates installed new unit
/// files, and creates a new `0600` evidence JSON with `O_EXCL`+fsync.
pub fn capture(spec: &MigrationSpec, transport: &mut dyn MigrationTransport) -> Result<Evidence> {
    spec.check_layout()?;
    require_absent(
        &spec
            .state_dir
            .join(activation_marker_name(&spec.instance_id)),
        "durable activation marker exists; refusing legacy migration",
    )?;
    for unit in &spec.new_units {
        verify_activation_guard(spec, unit, &spec.unit_directory.join(unit))?;
    }
    transport.verify_caller_safety(spec)?;
    let mut old_units = Vec::with_capacity(spec.legacy_units.len());
    for declared in &spec.legacy_units {
        old_units.push(observe_old(spec, declared, transport)?);
    }
    let mut new_units = Vec::with_capacity(spec.new_units.len());
    for unit in &spec.new_units {
        new_units.push(observe_new(spec, unit, transport)?);
    }
    let evidence = Evidence {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        spec: spec.clone(),
        captured_unix: unix_now(),
        old_units,
        new_units,
    };
    evidence.validate(spec)?;
    let bytes = serde_json::to_vec_pretty(&evidence).context("serialize migration evidence")?;
    ensure!(
        bytes.len() as u64 <= MAX_EVIDENCE_BYTES,
        "migration evidence exceeds 1MiB"
    );
    let dir = open_dir(&spec.state_dir, "state_dir", true)?;
    let leaf = format!("migration-evidence-{}.json", spec.instance_id);
    dir.write_new(&leaf, &bytes, 0o600)
        .context("write migration evidence (O_EXCL, refusing overwrite)")?;
    dir.sync()?;
    Ok(evidence)
}
/// Stop legacy units, reload, disable originally-enabled legacy units and enable
/// the declared new units. New units are never started; the returned report is
/// only evidence for the later real activation.
pub fn takeover(
    spec: &MigrationSpec,
    evidence: &Evidence,
    transport: &mut dyn MigrationTransport,
    confirm: bool,
) -> Result<Report> {
    spec.check_layout()?;
    require_absent(
        &spec
            .state_dir
            .join(activation_marker_name(&spec.instance_id)),
        "durable activation marker exists; refusing legacy migration",
    )?;
    for unit in &spec.new_units {
        verify_activation_guard(spec, unit, &spec.unit_directory.join(unit))?;
    }
    transport.verify_caller_safety(spec)?;
    evidence.validate(spec)?;
    ensure!(confirm, "takeover requires explicit confirm=true");
    ensure!(
        Journal::load(spec)?.is_none(),
        "durable migration journal already exists; run recover first"
    );
    let evidence_sha256 = evidence.digest()?;
    verify_drift(spec, evidence, transport)?;
    Journal::save(spec, &evidence_sha256, JournalPhase::Prepared)?;
    let outcome = (|| -> Result<Report> {
        Journal::save(spec, &evidence_sha256, JournalPhase::Stopping)?;
        for entry in evidence.old_units() {
            transport.stop_unit(spec, &entry.unit, COMMAND_TIMEOUT)?;
            wait_for(spec, transport, &entry.unit, "inactive", STOP_CONFIRM)?;
        }
        Journal::save(spec, &evidence_sha256, JournalPhase::OldStopped)?;
        Journal::save(spec, &evidence_sha256, JournalPhase::Reloading)?;
        transport.daemon_reload(spec, COMMAND_TIMEOUT)?;
        Journal::save(spec, &evidence_sha256, JournalPhase::DisablingOld)?;
        for entry in evidence
            .old_units()
            .iter()
            .filter(|entry| entry.was_enabled())
        {
            transport.disable_unit(spec, &entry.unit, COMMAND_TIMEOUT)?;
        }
        Journal::save(spec, &evidence_sha256, JournalPhase::OldDisabled)?;
        Journal::save(spec, &evidence_sha256, JournalPhase::EnablingNew)?;
        for entry in evidence.new_units() {
            transport.enable_unit(spec, &entry.unit, COMMAND_TIMEOUT)?;
        }
        Journal::save(spec, &evidence_sha256, JournalPhase::NewEnabled)?;
        let (old_units, new_units) = inspect_outcomes(spec, evidence, transport)?;
        for entry in evidence.old_units() {
            ensure!(
                show_value(spec, transport, &entry.unit, UnitProperty::ActiveState)? == "inactive",
                "legacy unit {} is not inactive",
                entry.unit
            );
            if entry.was_enabled() {
                ensure!(
                    !unit_file_state_is_enabled(&show_value(
                        spec,
                        transport,
                        &entry.unit,
                        UnitProperty::UnitFileState
                    )?),
                    "legacy unit {} is still enabled",
                    entry.unit
                );
            }
        }
        for entry in evidence.new_units() {
            ensure!(
                show_value(spec, transport, &entry.unit, UnitProperty::ActiveState)? == "inactive",
                "new unit {} was started",
                entry.unit
            );
            ensure!(
                unit_file_state_is_enabled(&show_value(
                    spec,
                    transport,
                    &entry.unit,
                    UnitProperty::UnitFileState
                )?),
                "new unit {} is not enabled",
                entry.unit
            );
        }
        Journal::save(spec, &evidence_sha256, JournalPhase::PreparedForActivation)?;
        Ok(report(spec, &evidence_sha256, JournalPhase::PreparedForActivation, true, false, false, old_units, new_units, "main agent: run systemd-apply to activate the new units; this module did not start them"))
    })();
    if outcome.is_err() {
        let _ = Journal::save(spec, &evidence_sha256, JournalPhase::Failed);
    }
    outcome.context("migration takeover may be partial; inspect the durable journal and run migration-recover --confirm --acknowledge-reversible before attempting another takeover; no automatic old-service restart was performed")
}
/// Pre-activation-only bootstrap rollback. Stops new units first, restores the
/// captured legacy enable state, then starts only legacy units that were active
/// at capture. If any new unit is active this refuses: post-activation
/// service/data rollback is out of scope and never guessed. Unit config files
/// are never rewritten.
pub fn recover(
    spec: &MigrationSpec,
    evidence: &Evidence,
    transport: &mut dyn MigrationTransport,
    ack_reversible: bool,
) -> Result<Report> {
    spec.check_layout()?;
    require_absent(
        &spec
            .state_dir
            .join(activation_marker_name(&spec.instance_id)),
        "durable activation marker exists; refusing legacy migration",
    )?;
    for unit in &spec.new_units {
        verify_activation_guard(spec, unit, &spec.unit_directory.join(unit))?;
    }
    transport.verify_caller_safety(spec)?;
    evidence.validate(spec)?;
    let journal =
        Journal::load(spec)?.context("no durable migration journal; nothing was taken over")?;
    let evidence_sha256 = evidence.digest()?;
    ensure!(
        journal.evidence_sha256 == evidence_sha256,
        "journal is bound to different evidence"
    );
    ensure!(
        journal.phase != JournalPhase::RolledBack,
        "migration already rolled back; recovery is terminal and cannot restart legacy units"
    );
    verify_old_files(spec, evidence, transport)?;
    for entry in evidence.new_units() {
        let (sha, mode, uid) = snapshot_file(&entry.path, "new unit file", false, MAX_UNIT_BYTES)?;
        ensure!(
            sha == entry.sha256 && mode == entry.mode && uid == entry.uid,
            "new unit drift before recovery for {}",
            entry.unit
        );
        ensure!(
            Path::new(&show_value(
                spec,
                transport,
                &entry.unit,
                UnitProperty::FragmentPath
            )?) == entry.path,
            "new FragmentPath drift before recovery for {}",
            entry.unit
        );
        ensure!(
            show_value(spec, transport, &entry.unit, UnitProperty::ActiveState)? == "inactive",
            "new unit {} is active; refusing post-activation rollback",
            entry.unit
        );
    }
    for entry in evidence.new_units() {
        ensure_never_started(spec, &entry.unit, transport)?;
    }
    require_absent(
        &spec
            .state_dir
            .join(format!("transaction-{}.json", spec.instance_id)),
        "deployment transaction exists; refusing post-activation migration recovery",
    )?;
    let needs_runtime = evidence.old_units().iter().any(OldUnitEvidence::was_active);
    ensure!(
        !needs_runtime || ack_reversible,
        "recover requires ack_reversible=true to restore originally active legacy units"
    );
    let outcome = (|| -> Result<Report> {
        Journal::save(spec, &evidence_sha256, JournalPhase::RollbackStarted)?;
        for entry in evidence.new_units() {
            transport.stop_unit(spec, &entry.unit, COMMAND_TIMEOUT)?;
            wait_for(spec, transport, &entry.unit, "inactive", STOP_CONFIRM)?;
        }
        Journal::save(spec, &evidence_sha256, JournalPhase::NewStopped)?;
        for entry in evidence.old_units() {
            if entry.was_enabled() {
                transport.enable_unit(spec, &entry.unit, COMMAND_TIMEOUT)?;
            } else if entry.was_disabled() {
                transport.disable_unit(spec, &entry.unit, COMMAND_TIMEOUT)?;
            }
        }
        for entry in evidence.new_units() {
            transport.disable_unit(spec, &entry.unit, COMMAND_TIMEOUT)?;
        }
        Journal::save(spec, &evidence_sha256, JournalPhase::OldRestored)?;
        for entry in evidence
            .old_units()
            .iter()
            .filter(|entry| entry.was_active())
        {
            transport.start_unit(spec, &entry.unit, COMMAND_TIMEOUT)?;
            wait_for(spec, transport, &entry.unit, "active", START_CONFIRM)?;
        }
        let (old_units, new_units) = inspect_outcomes(spec, evidence, transport)?;
        for entry in evidence.old_units() {
            if entry.was_active() {
                ensure!(
                    old_units
                        .iter()
                        .find(|out| out.unit == entry.unit)
                        .is_some_and(|out| out.active_state == "active"),
                    "legacy unit {} was not restored",
                    entry.unit
                );
            }
        }
        for entry in evidence.new_units() {
            ensure!(
                !unit_file_state_is_enabled(&show_value(
                    spec,
                    transport,
                    &entry.unit,
                    UnitProperty::UnitFileState
                )?),
                "new unit {} is still enabled",
                entry.unit
            );
        }
        // Publish the terminal phase only after every fallible outcome check.
        Journal::save(spec, &evidence_sha256, JournalPhase::RolledBack)?;
        Ok(report(spec, &evidence_sha256, JournalPhase::RolledBack, false, true, true, old_units, new_units, "legacy units restored; evidence/journal are immutable, use a fresh state_dir for a new attempt"))
    })();
    if outcome.is_err() {
        let _ = Journal::save(spec, &evidence_sha256, JournalPhase::Failed);
    }
    outcome
}
fn observe_old(
    spec: &MigrationSpec,
    declared: &LegacyUnit,
    transport: &mut dyn MigrationTransport,
) -> Result<OldUnitEvidence> {
    let fragment_path = show_value(spec, transport, &declared.unit, UnitProperty::FragmentPath)?;
    ensure!(
        Path::new(&fragment_path) == declared.fragment,
        "FragmentPath {fragment_path:?} does not match declared {}",
        declared.fragment.display()
    );
    let (fragment_sha256, fragment_mode, fragment_uid) =
        snapshot_file(&declared.fragment, "legacy fragment", false, MAX_UNIT_BYTES)?;
    let active_state = show_value(spec, transport, &declared.unit, UnitProperty::ActiveState)?;
    ensure!(
        active_state_ok(&active_state),
        "unexpected ActiveState {active_state:?}"
    );
    let main_pid = show_value(spec, transport, &declared.unit, UnitProperty::MainPID)?
        .parse::<u32>()
        .with_context(|| format!("parse MainPID for {}", declared.unit))?;
    let unit_file_state = show_value(spec, transport, &declared.unit, UnitProperty::UnitFileState)?;
    ensure!(
        unit_file_state_ok(&unit_file_state),
        "unexpected UnitFileState {unit_file_state:?}"
    );
    let working_directory = show_value(
        spec,
        transport,
        &declared.unit,
        UnitProperty::WorkingDirectory,
    )?;
    let expected = declared
        .working_directory
        .as_ref()
        .and_then(|path| path.to_str())
        .unwrap_or_default();
    ensure!(
        working_directory == expected,
        "WorkingDirectory {working_directory:?} does not match declaration {expected:?}"
    );
    let exec_start = show_value(spec, transport, &declared.unit, UnitProperty::ExecStart)?;
    ensure!(
        !exec_start.is_empty() && exec_start.len() <= MAX_TEXT,
        "unexpected ExecStart for {}",
        declared.unit
    );
    let (binary_path, binary_sha256, binary_mode, binary_uid) = match &declared.binary {
        None => (None, None, None, None),
        Some(path) => {
            let (sha, mode, uid) = snapshot_file(path, "legacy binary", true, MAX_BINARY_BYTES)?;
            (Some(path.clone()), Some(sha), Some(mode), Some(uid))
        }
    };
    Ok(OldUnitEvidence {
        unit: declared.unit.clone(),
        fragment_path: declared.fragment.clone(),
        fragment_sha256,
        fragment_mode,
        fragment_uid,
        main_pid,
        active_state,
        unit_file_state,
        working_directory,
        exec_start,
        exec_start_assurance: ExecStartAssurance::OpaqueEqualityOnly,
        binary_path,
        binary_sha256,
        binary_mode,
        binary_uid,
    })
}
fn observe_new(
    spec: &MigrationSpec,
    unit: &str,
    transport: &mut dyn MigrationTransport,
) -> Result<NewUnitEvidence> {
    let path = spec.unit_directory.join(unit);
    ensure!(
        path.parent() == Some(spec.unit_directory.as_path()),
        "new unit path escapes unit_directory"
    );
    ensure!(
        Path::new(&show_value(
            spec,
            transport,
            unit,
            UnitProperty::FragmentPath
        )?) == path,
        "new FragmentPath does not match captured unit file for {unit}"
    );
    let (sha256, mode, uid) = snapshot_file(&path, "new unit file", false, MAX_UNIT_BYTES)?;
    let unit_file_state = show_value(spec, transport, unit, UnitProperty::UnitFileState)?;
    ensure!(
        matches!(unit_file_state.as_str(), "disabled" | "disabled-runtime"),
        "new unit {unit} is not installed-disabled ({unit_file_state:?})"
    );
    let active_state = show_value(spec, transport, unit, UnitProperty::ActiveState)?;
    ensure!(
        active_state == "inactive",
        "new unit {unit} is already {active_state}; takeover only prepares activation"
    );
    ensure_never_started(spec, unit, transport)?;
    Ok(NewUnitEvidence {
        unit: unit.into(),
        path,
        sha256,
        mode,
        uid,
        unit_file_state,
        active_state,
    })
}
fn verify_old_files(
    spec: &MigrationSpec,
    evidence: &Evidence,
    transport: &mut dyn MigrationTransport,
) -> Result<()> {
    for entry in evidence.old_units() {
        ensure!(
            Path::new(&show_value(
                spec,
                transport,
                &entry.unit,
                UnitProperty::FragmentPath
            )?) == entry.fragment_path,
            "FragmentPath drift for {}",
            entry.unit
        );
        let (sha, mode, uid) = snapshot_file(
            &entry.fragment_path,
            "legacy fragment",
            false,
            MAX_UNIT_BYTES,
        )?;
        ensure!(
            sha == entry.fragment_sha256
                && mode == entry.fragment_mode
                && uid == entry.fragment_uid,
            "legacy fragment drift for {}",
            entry.unit
        );
        ensure!(
            show_value(spec, transport, &entry.unit, UnitProperty::WorkingDirectory)?
                == entry.working_directory,
            "WorkingDirectory drift for {}",
            entry.unit
        );
        ensure!(
            exec_start_definition(&show_value(
                spec,
                transport,
                &entry.unit,
                UnitProperty::ExecStart
            )?) == exec_start_definition(&entry.exec_start),
            "ExecStart drift for {}",
            entry.unit
        );
        if let (Some(path), Some(expected), Some(expected_mode), Some(expected_uid)) = (
            &entry.binary_path,
            &entry.binary_sha256,
            entry.binary_mode,
            entry.binary_uid,
        ) {
            let (sha, mode, uid) = snapshot_file(path, "legacy binary", true, MAX_BINARY_BYTES)?;
            ensure!(
                sha == *expected && mode == expected_mode && uid == expected_uid,
                "legacy binary drift for {}",
                entry.unit
            );
        }
    }
    Ok(())
}
fn verify_drift(
    spec: &MigrationSpec,
    evidence: &Evidence,
    transport: &mut dyn MigrationTransport,
) -> Result<()> {
    verify_old_files(spec, evidence, transport)?;
    for entry in evidence.old_units() {
        ensure!(
            show_value(spec, transport, &entry.unit, UnitProperty::ActiveState)?
                == entry.active_state,
            "ActiveState drift for {}",
            entry.unit
        );
        ensure!(
            show_value(spec, transport, &entry.unit, UnitProperty::MainPID)?
                .parse::<u32>()
                .ok()
                == Some(entry.main_pid),
            "MainPID drift for {}",
            entry.unit
        );
        ensure!(
            show_value(spec, transport, &entry.unit, UnitProperty::UnitFileState)?
                == entry.unit_file_state,
            "UnitFileState drift for {}",
            entry.unit
        );
    }
    for entry in evidence.new_units() {
        ensure!(
            Path::new(&show_value(
                spec,
                transport,
                &entry.unit,
                UnitProperty::FragmentPath
            )?) == entry.path,
            "new FragmentPath drift for {}",
            entry.unit
        );
        let (sha, mode, uid) = snapshot_file(&entry.path, "new unit file", false, MAX_UNIT_BYTES)?;
        ensure!(
            sha == entry.sha256 && mode == entry.mode && uid == entry.uid,
            "new unit file drift for {}",
            entry.unit
        );
        ensure!(
            show_value(spec, transport, &entry.unit, UnitProperty::UnitFileState)?
                == entry.unit_file_state,
            "new UnitFileState drift for {}",
            entry.unit
        );
        ensure!(
            show_value(spec, transport, &entry.unit, UnitProperty::ActiveState)? == "inactive",
            "new unit {} became active",
            entry.unit
        );
    }
    Ok(())
}
fn inspect_outcomes(
    spec: &MigrationSpec,
    evidence: &Evidence,
    transport: &mut dyn MigrationTransport,
) -> Result<(Vec<UnitOutcome>, Vec<UnitOutcome>)> {
    let mut old_units = Vec::with_capacity(evidence.old_units().len());
    for entry in evidence.old_units() {
        old_units.push(UnitOutcome {
            unit: entry.unit.clone(),
            active_state: show_value(spec, transport, &entry.unit, UnitProperty::ActiveState)?,
            unit_file_state: show_value(spec, transport, &entry.unit, UnitProperty::UnitFileState)?,
        });
    }
    let mut new_units = Vec::with_capacity(evidence.new_units().len());
    for entry in evidence.new_units() {
        new_units.push(UnitOutcome {
            unit: entry.unit.clone(),
            active_state: show_value(spec, transport, &entry.unit, UnitProperty::ActiveState)?,
            unit_file_state: show_value(spec, transport, &entry.unit, UnitProperty::UnitFileState)?,
        });
    }
    Ok((old_units, new_units))
}
fn wait_for(
    spec: &MigrationSpec,
    transport: &mut dyn MigrationTransport,
    unit: &str,
    wanted: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("deadline too large")?;
    loop {
        let state = show_value(spec, transport, unit, UnitProperty::ActiveState)?;
        ensure!(
            active_state_ok(&state),
            "unexpected ActiveState {state:?} for {unit}"
        );
        if state == wanted {
            return Ok(());
        }
        ensure!(
            state != "failed",
            "unit {unit} entered failed state while waiting for {wanted}"
        );
        if Instant::now() >= deadline {
            bail!("timed out waiting for {unit} to become {wanted} (last state {state})");
        }
        thread::sleep(POLL);
    }
}
fn show_value(
    spec: &MigrationSpec,
    transport: &mut dyn MigrationTransport,
    unit: &str,
    property: UnitProperty,
) -> Result<String> {
    let value = transport.show(spec, unit, property, COMMAND_TIMEOUT)?;
    let value = value.trim_end_matches(['\r', '\n']);
    ensure!(
        value.len() <= MAX_TEXT,
        "systemctl property value too large"
    );
    Ok(value.to_owned())
}
#[allow(clippy::too_many_arguments)]
fn report(
    spec: &MigrationSpec,
    evidence_sha256: &str,
    phase: JournalPhase,
    takeover_complete: bool,
    recovered: bool,
    old_runtime_restored: bool,
    old_units: Vec<UnitOutcome>,
    new_units: Vec<UnitOutcome>,
    next_action: &str,
) -> Report {
    Report {
        schema_version: MIGRATION_SCHEMA_VERSION,
        instance_id: spec.instance_id.clone(),
        phase,
        ready: false,
        activated: false,
        activation_required: true,
        takeover_complete,
        recovered,
        old_runtime_restored,
        data_modified: false,
        data_rolled_back: false,
        evidence_sha256: evidence_sha256.to_owned(),
        old_units,
        new_units,
        next_action: next_action.into(),
        notes: notes(),
    }
}
fn notes() -> Vec<String> {
    [
        "ready=false and activated=false: this is takeover preparation, not activation",
        "ExecStart is opaque equality-only; declared binary hashing does not prove it is launched",
        "new units are never started by takeover; a main-agent systemd-apply step is required",
        "recover refuses once any new unit is active; data is never rolled back blindly",
    ]
    .map(str::to_owned)
    .to_vec()
}
fn evidence_path(spec: &MigrationSpec) -> PathBuf {
    spec.state_dir
        .join(format!("migration-evidence-{}.json", spec.instance_id))
}
pub(crate) fn journal_path(spec: &MigrationSpec) -> PathBuf {
    spec.state_dir
        .join(format!("migration-journal-{}.json", spec.instance_id))
}
fn snapshot_file(
    path: &Path,
    label: &str,
    executable: bool,
    limit: u64,
) -> Result<(String, u32, u32)> {
    model::absolute_path(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open {label} {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "{label} must be a regular file");
    ensure!(
        metadata.uid() == euid(),
        "{label} must be owned by the current euid"
    );
    ensure!(
        metadata.mode() & 0o022 == 0,
        "{label} must not be group/world writable"
    );
    if executable {
        ensure!(metadata.mode() & 0o111 != 0, "{label} must be executable");
    }
    ensure!(metadata.len() <= limit, "{label} exceeds size limit");
    let (sha, read) = hash_reader(file, limit)?;
    ensure!(read == metadata.len(), "{label} changed while hashing");
    Ok((sha, metadata.mode() & 0o7777, metadata.uid()))
}
fn hash_reader(mut reader: impl Read, limit: u64) -> Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.checked_add(read as u64).context("size overflow")?;
        ensure!(total <= limit, "input exceeds size limit");
        hasher.update(&buffer[..read]);
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}
fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn read_private_json(dir: &Dir, leaf: &str, label: &str) -> Result<Vec<u8>> {
    let file = dir.open_file(leaf, false)?;
    let metadata = file.metadata()?;
    ensure!(metadata.uid() == euid(), "{label} must be caller-owned");
    ensure!(
        metadata.mode() & 0o077 == 0,
        "{label} must be private (0600)"
    );
    ensure!(metadata.len() <= MAX_EVIDENCE_BYTES, "{label} exceeds 1MiB");
    let bytes = safe_fs::read_json_bytes(file)?;
    ensure!(
        bytes.len() as u64 <= MAX_EVIDENCE_BYTES,
        "{label} exceeds 1MiB"
    );
    Ok(bytes)
}
fn open_dir(path: &Path, label: &str, private: bool) -> Result<Dir> {
    let dir = Dir::absolute(path).with_context(|| format!("open {label}"))?;
    let metadata = dir.0.metadata()?;
    ensure!(
        metadata.uid() == euid(),
        "{label} must be owned by the current euid"
    );
    if private {
        ensure!(
            metadata.mode() & 0o077 == 0,
            "{label} must be private (0700)"
        );
    } else {
        ensure!(
            metadata.mode() & 0o022 == 0,
            "{label} must not be group/world writable"
        );
    }
    Ok(dir)
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
fn active_state_ok(value: &str) -> bool {
    matches!(
        value,
        "active" | "reloading" | "inactive" | "failed" | "activating" | "deactivating"
    )
}
fn unit_file_state_ok(value: &str) -> bool {
    matches!(
        value,
        "enabled"
            | "enabled-runtime"
            | "disabled"
            | "disabled-runtime"
            | "static"
            | "indirect"
            | "generated"
            | "alias"
            | "masked"
            | "masked-runtime"
            | "linked"
            | "linked-runtime"
    )
}
fn unit_file_state_is_enabled(value: &str) -> bool {
    matches!(value.trim(), "enabled" | "enabled-runtime")
}
fn euid() -> u32 {
    unsafe { libc::geteuid() }
}
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
fn unix_nanos() -> u128 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    base + COUNTER.fetch_add(1, Ordering::Relaxed) as u128
}
fn ensure_success(output: &CommandOutput, what: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let detail: String = String::from_utf8_lossy(&output.stderr)
        .trim()
        .chars()
        .take(512)
        .collect();
    bail!(
        "{what} failed ({}){}",
        describe_status(output.status),
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
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
fn run_bounded(mut command: Command, timeout: Duration, limit: usize) -> Result<CommandOutput> {
    ensure!(!timeout.is_zero() && limit > 0, "invalid command bounds");
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("command deadline too large")?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().context("spawn bounded systemctl")?;
    let pid = child.id();
    let stdout = spawn_reader(child.stdout.take().context("systemctl stdout")?, limit);
    let stderr = spawn_reader(child.stderr.take().context("systemctl stderr")?, limit);
    let status = loop {
        if let Some(status) = child.try_wait().context("wait for systemctl")? {
            break status;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            }
            let _ = child.wait();
            let _ = stdout.join();
            let _ = stderr.join();
            bail!("systemctl timed out after {timeout:?}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout
        .join()
        .map_err(|_| anyhow!("stdout reader panicked"))??;
    let stderr = stderr
        .join()
        .map_err(|_| anyhow!("stderr reader panicked"))??;
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}
fn spawn_reader(
    mut pipe: impl Read + Send + 'static,
    limit: usize,
) -> thread::JoinHandle<Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            let read = pipe.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            ensure!(output.len() + read <= limit, "command output exceeds limit");
            output.extend_from_slice(&buffer[..read]);
        }
        Ok(output)
    })
}

// Only systemd's trailing execution-result fields may change after stop.
// The raw value remains in evidence; unknown forms retain exact equality.
fn exec_start_definition(value: &str) -> &str {
    value
        .split_once(" ; start_time=")
        .map(|(definition, _)| definition)
        .unwrap_or(value)
}
fn ensure_never_started(
    spec: &MigrationSpec,
    unit: &str,
    transport: &mut dyn MigrationTransport,
) -> Result<()> {
    let timestamp = show_value(
        spec,
        transport,
        unit,
        UnitProperty::ExecMainStartTimestampMonotonic,
    )?;
    ensure!(
        timestamp
            .parse::<u64>()
            .context("invalid unit start timestamp")?
            == 0,
        "new unit {unit} has previously started; refusing pre-activation recovery/capture"
    );
    Ok(())
}

fn critical_host_unit(unit: &str) -> bool {
    matches!(
        unit,
        "systemd-logind.service"
            | "dbus.service"
            | "dbus-broker.service"
            | "systemd-journald.service"
            | "systemd-user-sessions.service"
    ) || unit.starts_with("user@")
        || unit.starts_with("systemd-user")
}
fn caller_ancestors() -> Result<BTreeSet<u32>> {
    let mut result = BTreeSet::new();
    let mut pid = std::process::id();
    while pid != 0 {
        ensure!(result.insert(pid), "cycle in caller process ancestry");
        let status = fs::read_to_string(format!("/proc/{pid}/status"))
            .context("read caller ancestry before service takeover")?;
        pid = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .context("caller ancestor has no PPid")?
            .trim()
            .parse()
            .context("invalid ancestor PPid")?;
    }
    Ok(result)
}
