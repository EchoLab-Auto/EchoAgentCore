//! Closed-loop `echo-deploy-control/1` adapter for one Core instance.
//!
//! Bridges Core's Unix control socket (`prepare|cancel|status|ready`), the Panel
//! deployment gate, the durable selector and the verified release manifest. It is
//! the administrator-owned program invoked by the systemd backend (one JSON
//! request on stdin, one JSON reply on stdout).
//!
//! Fail-closed: `PrepareUpgrade` requires `acknowledge_external_writers_stopped`
//! because Core locks cover only its own writers; `Ready`/`MaintenanceLeave` hash
//! `/proc/<pid>/exe` (pid from `SO_PEERCRED`) against the active manifest, never
//! `release.declared` alone; an absent Core socket makes `Observe` report
//! `ready=false` and every mutating/ready operation fails.
//!
//! First install is the one exception to "Core must answer", and it is never
//! inferred from a missing socket: the administrator must configure
//! [`FirstInstallConfig`], and the systemd backend must write a fresh, private
//! [`FirstInstallEvidence`] document that proves the durable selector is absent
//! and every canonical unit is `inactive`. The adapter independently re-reads
//! the real selector, so socket absence alone can never fabricate a drained or
//! prepared installation.

use crate::{
    control_client::{self, current_euid, CoreOperation},
    model::{self, Kind},
    state_lock::StateLock,
    systemd::{ControlOperation, ControlReply, ControlRequest, CONTROL_PROTOCOL},
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const STATE_FILE: &str = "runtime-adapter-state.json";
/// First-install evidence is a strict, versioned wire contract between the
/// systemd transaction backend and this adapter.
pub const FIRST_INSTALL_EVIDENCE_SCHEMA_VERSION: u32 = 1;
/// The only accepted evidence producer.
pub const FIRST_INSTALL_EVIDENCE_SOURCE: &str = "systemd";
/// Default maximum age of a backend observation.
pub const DEFAULT_FIRST_INSTALL_MAX_AGE_SECONDS: u64 = 300;
const MAX_FIRST_INSTALL_AGE_SECONDS: u64 = 600;
/// Future timestamps beyond this are rejected (limited clock skew only).
const MAX_CLOCK_SKEW_MS: u64 = 30_000;
/// Core is absent on a first install; this synthetic generation is stored only
/// so the durable prepared record keeps its non-zero invariant. It is never
/// compared with a live Core generation.
const SYNTHETIC_FIRST_INSTALL_GENERATION: u64 = 1;
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_JSON_BYTES: u64 = 64 * 1024;
const MAX_SOCKET_PATH_BYTES: usize = 107;
const MAX_ID_BYTES: usize = 128;
const MAX_PROTOCOL_BYTES: usize = 64;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PanelGate {
    schema_version: u32,
    instance_id: String,
    maintenance: bool,
    release_id: String,
    management_protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedState {
    transaction_id: String,
    expected_generation: u64,
    release: String,
    manifest_sha256: String,
    #[serde(default)]
    previous: Option<String>,
    /// Present exactly when this prepare is a first install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    first_install: Option<FirstInstallPrepared>,
}

/// Durable binding to the backend evidence used by a first-install prepare.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FirstInstallPrepared {
    evidence_sha256: String,
    observed_at_unix_ms: u64,
    units: Vec<FirstInstallUnit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterState {
    schema_version: u32,
    instance_id: String,
    maintenance: bool,
    /// Set after any successful maintenance release with a selected release.
    /// A later missing selector must not be mistaken for a brand-new install.
    #[serde(default)]
    installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prepared: Option<PreparedState>,
}

/// Strict administrator config: no inferred path, no security default.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub instance_id: String,
    pub core_socket: PathBuf,
    pub panel_gate_file: Option<PathBuf>,
    /// Private directory (`mode & 0o077 == 0`) owned by the effective uid.
    pub instance_state: PathBuf,
    pub current_selector: PathBuf,
    pub release_store: PathBuf,
    #[serde(default)]
    pub required_modules: Vec<String>,
    /// Must be true before `PrepareUpgrade`: user subprocess writers stopped.
    #[serde(default)]
    pub acknowledge_external_writers_stopped: bool,
    /// Opt-in first-install evidence contract. Without it a missing Core socket
    /// remains a hard failure; it is never read as an empty installation.
    #[serde(default)]
    pub first_install: Option<FirstInstallConfig>,
}

/// Administrator-declared first-install evidence contract.
///
/// The evidence document is owned by the systemd transaction backend, not by
/// this adapter: the backend observes `ActiveState` for the canonical units and
/// writes [`FirstInstallEvidence`] before asking the adapter to prepare a first
/// install. The adapter validates the document and independently re-reads the
/// real selector; it never substitutes socket absence for that proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirstInstallConfig {
    /// Absolute path of the backend-owned evidence document. Its parent must be
    /// a private directory owned by the effective uid.
    pub evidence: PathBuf,
    /// Canonical `echo-agent-<instance>-core.service`.
    pub core_unit: String,
    /// Canonical `echo-agent-<instance>-panel.service` for standalone
    /// topologies; `None` only when the deployment has no panel unit.
    #[serde(default)]
    pub panel_unit: Option<String>,
    /// Maximum accepted age of `observed_at_unix_ms`.
    #[serde(default = "default_first_install_max_age_seconds")]
    pub max_evidence_age_seconds: u64,
}

fn default_first_install_max_age_seconds() -> u64 {
    DEFAULT_FIRST_INSTALL_MAX_AGE_SECONDS
}

/// One systemd unit observation provided by the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirstInstallUnit {
    pub unit: String,
    pub active_state: String,
}

/// Explicit first-install proof written by the systemd transaction backend.
///
/// This is a wire contract, not a self-authenticating document: the file has to
/// live in a private directory, and the adapter treats the backend as the same
/// administrator-owned execution boundary as itself. It binds the observation
/// to one instance, one target release and one target manifest. The adapter
/// still rechecks the selector, unit names, states, timestamp and digests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirstInstallEvidence {
    pub schema_version: u32,
    /// Must equal [`FIRST_INSTALL_EVIDENCE_SOURCE`].
    pub source: String,
    pub instance_id: String,
    pub target_release: String,
    pub target_manifest_sha256: String,
    /// Backend assertion; the adapter also re-reads the real selector.
    pub selector_absent: bool,
    pub units: Vec<FirstInstallUnit>,
    /// Wall-clock observation time in Unix milliseconds.
    pub observed_at_unix_ms: u64,
}

impl FirstInstallEvidence {
    /// Check the parts that do not depend on local adapter configuration.
    pub fn validate_shape(&self) -> Result<()> {
        need(
            self.schema_version == FIRST_INSTALL_EVIDENCE_SCHEMA_VERSION,
            "unsupported first-install evidence schema",
        )?;
        need(
            self.source == FIRST_INSTALL_EVIDENCE_SOURCE,
            "first-install evidence source is not systemd",
        )?;
        model::safe_id(&self.instance_id).context("bad first-install evidence instance")?;
        model::safe_id(&self.target_release)?;
        model::safe_id(&self.target_release)?;
        model::digest(&self.target_manifest_sha256)?;
        need(
            !self.units.is_empty() && self.units.len() <= 8,
            "bad first-install evidence unit list",
        )?;
        let mut seen = std::collections::BTreeSet::new();
        for unit in &self.units {
            need(
                !unit.unit.is_empty() && unit.unit.len() <= 256,
                "bad first-install unit name",
            )?;
            need(seen.insert(&unit.unit), "duplicate first-install unit")?;
        }
        Ok(())
    }
}

struct ActiveRelease {
    release_id: String,
    manifest_sha256: String,
    core_sha256: String,
}
struct CoreStatus {
    declared: Option<String>,
    phase: String,
    prepared: bool,
    maintenance_held: bool,
    generation: Option<u64>,
}

fn need(ok: bool, message: &str) -> Result<()> {
    ensure!(ok, "{message}");
    Ok(())
}
fn file_mode(meta: &fs::Metadata) -> u32 {
    meta.permissions().mode()
}

/// Handle one request. Every `Err` is a real failure: the CLI must exit
/// non-zero rather than print a fabricated success acknowledgement.
pub fn handle(config: &RuntimeConfig, request: ControlRequest) -> Result<ControlReply> {
    config.validate()?;
    need(
        request.protocol == CONTROL_PROTOCOL,
        "unsupported control protocol",
    )?;
    need(
        request.instance == config.instance_id,
        "request instance mismatch",
    )?;
    let _lock = StateLock::acquire(&config.instance_state).context("lock adapter state")?;
    match request.operation {
        ControlOperation::Observe => observe(config),
        ControlOperation::PrepareUpgrade => prepare(config, &request),
        ControlOperation::Drain => drain(config),
        ControlOperation::MaintenanceEnter => maintenance_enter(config),
        ControlOperation::MaintenanceLeave => maintenance_leave(config),
        ControlOperation::Ready => ready(config, &request),
    }
}

impl RuntimeConfig {
    /// Validate declared paths and existing directory permissions without
    /// acquiring the adapter lock, connecting a socket, or writing state.
    pub fn validate(&self) -> Result<()> {
        model::safe_id(&self.instance_id)?;
        for path in [
            &self.core_socket,
            &self.instance_state,
            &self.current_selector,
            &self.release_store,
        ] {
            model::absolute_path(path)?;
        }
        if let Some(gate) = &self.panel_gate_file {
            model::absolute_path(gate)?;
        }
        let raw = self.core_socket.as_os_str().as_encoded_bytes();
        need(
            !raw.is_empty() && raw.len() <= MAX_SOCKET_PATH_BYTES,
            "bad core_socket length",
        )?;
        let parent = self
            .core_socket
            .parent()
            .context("core_socket has no parent")?;
        match fs::metadata(parent) {
            Ok(meta) => {
                need(meta.is_dir(), "core_socket parent is not a directory")?;
                need(
                    meta.uid() == current_euid(),
                    "core_socket parent owner mismatch",
                )?;
                need(
                    file_mode(&meta) & 0o077 == 0,
                    "core_socket parent not private",
                )?;
            }
            // A removed runtime directory is a stopped service, not a config error.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("stat core_socket parent"),
        }
        check_dir(&self.instance_state, "instance_state", true)?;
        check_dir(&self.release_store, "release_store", false)?;
        for module in &self.required_modules {
            model::safe_id(module)?;
        }
        if let Some(first) = &self.first_install {
            model::absolute_path(&first.evidence)?;
            need(
                first.evidence != state_path(self),
                "first_install.evidence must not be the adapter state file",
            )?;
            need(
                !first.evidence.starts_with(&self.release_store),
                "first_install.evidence must not live in release_store",
            )?;
            let evidence_parent = first
                .evidence
                .parent()
                .context("first_install.evidence has no parent")?;
            check_dir(evidence_parent, "first_install.evidence parent", true)?;
            let expected_core = format!("echo-agent-{}-core.service", self.instance_id);
            need(
                first.core_unit == expected_core,
                "first_install.core_unit does not match the deployment instance",
            )?;
            if let Some(panel) = &first.panel_unit {
                let expected_panel = format!("echo-agent-{}-panel.service", self.instance_id);
                need(
                    panel == &expected_panel,
                    "first_install.panel_unit does not match the deployment instance",
                )?;
                need(
                    panel != &first.core_unit,
                    "first_install core and panel units must differ",
                )?;
            }
            need(
                (1..=MAX_FIRST_INSTALL_AGE_SECONDS).contains(&first.max_evidence_age_seconds),
                "first_install.max_evidence_age_seconds outside 1..=600",
            )?;
        }
        Ok(())
    }
}

fn check_dir(path: &Path, label: &str, private: bool) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {label}"))?;
    need(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "not a real directory",
    )?;
    need(meta.uid() == current_euid(), "directory owner mismatch")?;
    let forbidden = if private { 0o077 } else { 0o022 };
    need(file_mode(&meta) & forbidden == 0, "unsafe directory mode")?;
    Ok(())
}

fn read_regular(path: &Path, label: &str, limit: u64) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {label}"))?;
    need(meta.file_type().is_file(), "not a regular file")?;
    need(meta.uid() == current_euid(), "file owner mismatch")?;
    need(meta.len() <= limit, "file too large")?;
    need(
        file_mode(&meta) & 0o022 == 0,
        "file is writable by group/other",
    )?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Read a backend-owned evidence/state file with owner-only privacy and a
/// single link. Used for first-install evidence, which authorizes a mutation
/// without a live Core process.
fn read_private_regular(path: &Path, label: &str, limit: u64) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {label}"))?;
    need(meta.file_type().is_file(), "not a regular file")?;
    need(meta.uid() == current_euid(), "file owner mismatch")?;
    need(meta.nlink() == 1, "file must have exactly one link")?;
    need(meta.len() <= limit, "file too large")?;
    need(file_mode(&meta) & 0o077 == 0, "file is not private")?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let opened = file.metadata()?;
    need(
        opened.dev() == meta.dev()
            && opened.ino() == meta.ino()
            && opened.nlink() == 1
            && opened.uid() == current_euid()
            && file_mode(&opened) & 0o077 == 0
            && opened.len() == meta.len(),
        "file changed while opening",
    )?;
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut bytes)?;
    need(
        bytes.len() as u64 == meta.len(),
        "file changed while reading",
    )?;
    Ok(bytes)
}

fn core_socket_present(config: &RuntimeConfig) -> Result<bool> {
    match fs::symlink_metadata(&config.core_socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("stat Core socket"),
        Ok(meta) => {
            need(
                meta.file_type().is_socket(),
                "core_socket exists but is not a socket",
            )?;
            Ok(true)
        }
    }
}

fn now_unix_ms() -> Result<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    u64::try_from(elapsed.as_millis()).context("system time exceeds u64 milliseconds")
}

fn expected_first_install_units(config: &RuntimeConfig) -> Result<Vec<String>> {
    let first = config
        .first_install
        .as_ref()
        .context("first-install evidence is not configured")?;
    let mut units = vec![first.core_unit.clone()];
    if let Some(panel) = &first.panel_unit {
        units.push(panel.clone());
    }
    Ok(units)
}

/// Compare unit observations independent of declaration order.
fn same_unit_observations(a: &[FirstInstallUnit], b: &[FirstInstallUnit]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    a.sort_by(|left, right| left.unit.cmp(&right.unit));
    b.sort_by(|left, right| left.unit.cmp(&right.unit));
    a == b
}

/// Validate a backend evidence document against the adapter config and the
/// request being prepared. Returns the parsed document plus the digest of the
/// exact bytes, so the prepared record is bound to immutable evidence.
fn validate_first_install_evidence(
    config: &RuntimeConfig,
    release: &str,
    manifest: &str,
) -> Result<(FirstInstallEvidence, String)> {
    let first = config.first_install.as_ref().context(
        "first-install evidence is not configured; \
         refusing to infer an empty installation from a missing Core socket",
    )?;
    model::safe_id(release)?;
    model::digest(manifest)?;
    let bytes = read_private_regular(&first.evidence, "first-install evidence", MAX_JSON_BYTES)?;
    let evidence: FirstInstallEvidence =
        serde_json::from_slice(&bytes).context("parse first-install evidence as strict JSON")?;
    evidence.validate_shape()?;
    need(
        evidence.instance_id == config.instance_id,
        "first-install evidence instance mismatch",
    )?;
    need(
        evidence.target_release == release,
        "first-install evidence release mismatch",
    )?;
    need(
        evidence.target_manifest_sha256 == manifest,
        "first-install evidence manifest mismatch",
    )?;
    need(
        evidence.selector_absent,
        "first-install evidence does not assert selector_absent",
    )?;
    let expected = expected_first_install_units(config)?;
    need(
        evidence.units.len() == expected.len(),
        "first-install evidence unit set does not match the configured units",
    )?;
    for unit in &expected {
        let mut matches = evidence.units.iter().filter(|seen| &seen.unit == unit);
        let Some(seen) = matches.next() else {
            bail!("first-install evidence is missing unit {unit}");
        };
        need(
            matches.next().is_none(),
            "first-install evidence repeats a configured unit",
        )?;
        need(
            seen.active_state == "inactive",
            "first-install evidence unit is not inactive",
        )?;
    }
    let now = now_unix_ms()?;
    need(
        evidence.observed_at_unix_ms <= now.saturating_add(MAX_CLOCK_SKEW_MS),
        "first-install evidence timestamp is in the future",
    )?;
    let age = now.saturating_sub(evidence.observed_at_unix_ms);
    need(
        age <= first.max_evidence_age_seconds.saturating_mul(1000),
        "first-install evidence is stale",
    )?;
    let digest = hex(&bytes);
    Ok((evidence, digest))
}

/// Publish the backend's first-install observation for the adapter to consume.
///
/// This is the writer side of the wire contract, provided here so the systemd
/// transaction backend cannot accidentally emit a document the adapter would
/// reject. It is atomic (temp + rename), mode `0600`, fsyncs the file and the
/// directory, replaces a symlink instead of following it, and rejects any
/// document that asserts `selector_absent=false` or records a non-`inactive`
/// unit.
pub fn write_first_install_evidence(path: &Path, evidence: &FirstInstallEvidence) -> Result<()> {
    evidence.validate_shape()?;
    need(
        evidence.selector_absent,
        "first-install evidence may only be written with selector_absent=true",
    )?;
    for unit in &evidence.units {
        need(
            unit.active_state == "inactive",
            "first-install evidence may only record inactive units",
        )?;
    }
    model::absolute_path(path)?;
    let parent = path
        .parent()
        .context("first-install evidence has no parent")?;
    check_dir(parent, "first-install evidence parent", true)?;
    write_json_atomic(path, evidence, 0o600)
}

/// Clear maintenance in the Panel gate for a failed first install, when the
/// installer provisioned a gate. A brand-new standalone instance may not have a
/// gate yet; there is nothing to release in that case, and inventing a release
/// identity here would be worse than leaving the file absent.
fn clear_first_install_gate(config: &RuntimeConfig) -> Result<()> {
    let Some(path) = &config.panel_gate_file else {
        return Ok(());
    };
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("stat panel gate"),
        Ok(_) => {
            let mut gate = read_gate(config)?;
            gate.maintenance = false;
            validate_gate(config, &gate)?;
            write_json_atomic(path, &gate, file_mode(&fs::metadata(path)?) & 0o666)
        }
    }
}

fn first_install_gate_is_clear(config: &RuntimeConfig) -> Result<bool> {
    let Some(path) = &config.panel_gate_file else {
        return Ok(true);
    };
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error).context("stat panel gate"),
        Ok(_) => Ok(!read_gate(config)?.maintenance),
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T, mode: u32) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    need(bytes.len() as u64 <= MAX_JSON_BYTES, "JSON too large")?;
    let parent = path.parent().context("path has no parent")?;
    let meta = fs::metadata(parent)?;
    need(
        meta.is_dir() && meta.uid() == current_euid(),
        "unsafe parent",
    )?;
    need(
        file_mode(&meta) & 0o022 == 0,
        "parent is group/other writable",
    )?;
    let leaf = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid name")?;
    let temp = parent.join(format!(
        ".{leaf}.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn state_path(config: &RuntimeConfig) -> PathBuf {
    config.instance_state.join(STATE_FILE)
}

fn load_state(config: &RuntimeConfig) -> Result<AdapterState> {
    let empty = AdapterState {
        schema_version: 1,
        instance_id: config.instance_id.clone(),
        maintenance: false,
        installed: false,
        prepared: None,
    };
    let path = state_path(config);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(empty),
        Err(error) => return Err(error).context("stat adapter state"),
        Ok(_) => {}
    }
    let state: AdapterState =
        serde_json::from_slice(&read_regular(&path, "adapter state", MAX_JSON_BYTES)?)?;
    need(
        state.schema_version == 1,
        "unsupported adapter state schema",
    )?;
    need(
        state.instance_id == config.instance_id,
        "adapter state instance mismatch",
    )?;
    if let Some(prepared) = &state.prepared {
        model::safe_id(&prepared.release)?;
        model::digest(&prepared.manifest_sha256)?;
        need(!prepared.transaction_id.is_empty(), "empty transaction id")?;
        need(prepared.expected_generation > 0, "zero prepared generation")?;
        match (&prepared.previous, &prepared.first_install) {
            (Some(previous), None) => {
                model::safe_id(previous)?;
            }
            (None, Some(first)) => {
                model::digest(&first.evidence_sha256)?;
                need(
                    first.observed_at_unix_ms > 0,
                    "zero first-install evidence timestamp",
                )?;
                need(
                    !first.units.is_empty() && first.units.len() <= 8,
                    "bad first-install prepared unit list",
                )?;
                let mut seen = std::collections::BTreeSet::new();
                for unit in &first.units {
                    need(!unit.unit.is_empty(), "empty first-install unit")?;
                    need(
                        unit.active_state == "inactive",
                        "first-install prepared unit is not inactive",
                    )?;
                    need(
                        seen.insert(&unit.unit),
                        "duplicate first-install prepared unit",
                    )?;
                }
            }
            (None, None) => {
                bail!("prepared state has neither a previous release nor first-install evidence")
            }
            (Some(_), Some(_)) => {
                bail!("prepared state has both a previous release and first-install evidence")
            }
        }
    }
    Ok(state)
}

fn write_state(config: &RuntimeConfig, state: &AdapterState) -> Result<()> {
    write_json_atomic(&state_path(config), state, 0o600).context("write adapter state")
}

fn read_gate(config: &RuntimeConfig) -> Result<PanelGate> {
    let path = config
        .panel_gate_file
        .as_ref()
        .context("panel gate not configured")?;
    let gate: PanelGate =
        serde_json::from_slice(&read_regular(path, "panel gate", MAX_JSON_BYTES)?)?;
    validate_gate(config, &gate)?;
    Ok(gate)
}

fn validate_gate(config: &RuntimeConfig, gate: &PanelGate) -> Result<()> {
    need(gate.schema_version == 1, "unsupported panel gate schema")?;
    need(
        gate.instance_id == config.instance_id,
        "panel gate instance mismatch",
    )?;
    need(
        !gate.release_id.is_empty() && gate.release_id.len() <= MAX_ID_BYTES,
        "bad gate release_id",
    )?;
    need(
        !gate.management_protocol.is_empty()
            && gate.management_protocol.len() <= MAX_PROTOCOL_BYTES,
        "bad gate management_protocol",
    )?;
    Ok(())
}

fn maintenance_truth(config: &RuntimeConfig, state: &AdapterState) -> Result<bool> {
    match &config.panel_gate_file {
        Some(_) => Ok(read_gate(config)?.maintenance),
        None => Ok(state.maintenance),
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

fn verify_release(
    config: &RuntimeConfig,
    release: &str,
    expected_manifest: Option<&str>,
    hash_artifact: bool,
) -> Result<ActiveRelease> {
    model::safe_id(release)?;
    if let Some(expected) = expected_manifest {
        model::digest(expected)?;
    }
    let dir = config.release_store.join(release);
    let meta = fs::symlink_metadata(&dir)?;
    need(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "release is not a real directory",
    )?;
    let bytes = read_regular(
        &dir.join("release.json"),
        "release manifest",
        MAX_MANIFEST_BYTES,
    )?;
    let manifest_sha256 = hex(&bytes);
    if let Some(expected) = expected_manifest {
        need(
            manifest_sha256 == expected,
            "release manifest digest mismatch",
        )?;
    }
    let document: model::Release = serde_json::from_slice(&bytes)?;
    document.validate()?;
    need(
        document.release_id == release,
        "release id does not match directory",
    )?;
    let mut cores = document
        .artifacts
        .iter()
        .filter(|a| a.kind == Kind::Core && a.executable);
    let core = cores.next().context("no executable Core artifact")?;
    need(cores.next().is_none(), "multiple executable Core artifacts")?;
    let binary = dir.join(&core.path);
    let meta = fs::symlink_metadata(&binary)?;
    need(
        meta.is_file() && !meta.file_type().is_symlink(),
        "Core artifact is not a file",
    )?;
    need(meta.len() == core.size, "Core artifact size mismatch")?;
    need(
        file_mode(&meta) & 0o111 != 0,
        "Core artifact is not executable",
    )?;
    if hash_artifact {
        let actual = sha_file(&binary)?;
        need(actual == core.sha256, "Core artifact digest mismatch")?;
    }
    Ok(ActiveRelease {
        release_id: release.into(),
        manifest_sha256,
        core_sha256: core.sha256.clone(),
    })
}

fn read_selector(config: &RuntimeConfig) -> Result<Option<ActiveRelease>> {
    let meta = match fs::symlink_metadata(&config.current_selector) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("stat current_selector"),
        Ok(meta) => meta,
    };
    need(
        meta.file_type().is_symlink(),
        "current_selector is not a symlink",
    )?;
    let target = fs::read_link(&config.current_selector)?;
    model::absolute_path(&target)?;
    let relative = target
        .strip_prefix(&config.release_store)
        .context("selector escapes store")?;
    let mut components = relative.components();
    let Some(Component::Normal(name)) = components.next() else {
        bail!("selector must name one release directory")
    };
    need(
        components.next().is_none(),
        "selector must name one release directory",
    )?;
    let release = name.to_str().context("release name is not UTF-8")?;
    model::safe_id(release)?;
    need(
        target == config.release_store.join(release),
        "selector target is not canonical",
    )?;
    Ok(Some(verify_release(config, release, None, false)?))
}

fn query(
    config: &RuntimeConfig,
    operation: CoreOperation,
    tx: &str,
    expected: Option<&str>,
) -> Result<Value> {
    let response =
        control_client::query_core(&config.core_socket, operation, tx, expected, QUERY_TIMEOUT)?;
    if matches!(operation, CoreOperation::Status | CoreOperation::Ready) {
        control_client::verify_required_modules(&response, &config.required_modules)?;
    }
    Ok(response)
}

fn parse_status(response: &Value, tx: &str) -> Result<CoreStatus> {
    check_ack(response, "status", tx)?;
    let identity = control_client::core_identity(response)?;
    let object = response.as_object().context("status is not an object")?;
    let phase = object
        .get("phase")
        .and_then(Value::as_str)
        .context("missing phase")?;
    let prepared = object
        .get("prepared")
        .and_then(Value::as_bool)
        .context("missing prepared")?;
    let held = object
        .get("maintenance_held")
        .and_then(Value::as_bool)
        .context("missing held")?;
    let generation = match object.get("generation") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().context("generation is not u64")?),
    };
    Ok(CoreStatus {
        declared: identity.declared,
        phase: phase.into(),
        prepared,
        maintenance_held: held,
        generation,
    })
}

fn check_ack(response: &Value, operation: &str, tx: &str) -> Result<()> {
    let object = response
        .as_object()
        .context("Core response is not an object")?;
    if object.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!("Core rejected {operation}: {}", core_error(response));
    }
    need(
        object.get("operation").and_then(Value::as_str) == Some(operation),
        "operation mismatch",
    )?;
    need(
        object.get("transaction_id").and_then(Value::as_str) == Some(tx),
        "transaction_id mismatch",
    )?;
    Ok(())
}

fn core_error(response: &Value) -> String {
    let error = response
        .as_object()
        .and_then(|o| o.get("error"))
        .and_then(Value::as_object);
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("no detail");
    format!("{code}: {message}")
}

fn peer_pid(config: &RuntimeConfig) -> Result<u32> {
    let meta = fs::symlink_metadata(&config.core_socket).context("stat Core socket")?;
    need(meta.file_type().is_socket(), "core_socket is not a socket")?;
    let euid = current_euid();
    need(meta.uid() == euid, "Core socket owner mismatch")?;
    let stream = UnixStream::connect(&config.core_socket).context("connect Core socket")?;
    // SAFETY: zeroed ucred is valid for getsockopt to fill in.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: stream owns the fd; pointers are valid for the duration.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    need(result == 0, "getsockopt(SO_PEERCRED) failed")?;
    need(
        length as usize >= std::mem::size_of::<libc::ucred>(),
        "short SO_PEERCRED",
    )?;
    need(credentials.uid == euid, "Core peer uid mismatch")?;
    need(credentials.pid > 0, "Core peer has no pid")?;
    Ok(credentials.pid as u32)
}

fn process_start(pid: u32) -> Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, rest) = stat.rsplit_once(')').context("malformed /proc stat")?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    fields
        .get(19)
        .context("missing starttime")?
        .parse::<u64>()
        .context("bad starttime")
}

fn sha_reader(file: &mut File) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn sha_file(path: &Path) -> Result<String> {
    sha_reader(&mut File::open(path)?)
}

fn running_exe_sha(pid: u32) -> Result<String> {
    let before = process_start(pid)?;
    let mut file = File::open(format!("/proc/{pid}/exe")).context("open /proc/<pid>/exe")?;
    let sha = sha_reader(&mut file)?;
    need(
        process_start(pid)? == before,
        "process changed while hashing",
    )?;
    Ok(sha)
}

fn verify_running_pid(config: &RuntimeConfig, active: &ActiveRelease) -> Result<u32> {
    let pid = peer_pid(config)?;
    let sha = running_exe_sha(pid)?;
    if sha != active.core_sha256 {
        bail!(
            "running executable SHA {sha} does not match release {} manifest {}",
            active.release_id,
            active.core_sha256
        );
    }
    need(
        peer_pid(config)? == pid,
        "Core process changed while verifying",
    )?;
    Ok(pid)
}

fn verify_process_still(config: &RuntimeConfig, pid: u32) -> Result<()> {
    need(
        peer_pid(config)? == pid,
        "Core process changed during query",
    )
}

fn observe(config: &RuntimeConfig) -> Result<ControlReply> {
    let selector = read_selector(config)?;
    let state = load_state(config)?;
    let maintenance = maintenance_truth(config, &state)?;
    let running = match fs::symlink_metadata(&config.core_socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("stat Core socket"),
        Ok(_) => true,
    };
    let (current_release, ready) = if running {
        let tx = format!("observe-{}", config.instance_id);
        let status = parse_status(&query(config, CoreOperation::Status, &tx, None)?, &tx)?;
        match (&selector, &status.declared) {
            (Some(active), Some(declared)) if active.release_id != *declared => {
                bail!(
                    "Core declares {declared:?}, selector points at {:?}",
                    active.release_id
                )
            }
            (Some(active), None) => {
                bail!(
                    "Core declares no release, selector points at {:?}",
                    active.release_id
                )
            }
            _ => {}
        }
        if let Some(active) = &selector {
            verify_running_pid(config, active)?;
        }
        let ready = selector.is_some()
            && status.declared.is_some()
            && status.phase == "idle"
            && !status.prepared
            && !status.maintenance_held;
        (status.declared, ready)
    } else {
        (selector.as_ref().map(|a| a.release_id.clone()), false)
    };
    Ok(ControlReply::observed(
        config.instance_id.clone(),
        current_release,
        maintenance,
        ready,
        0,
    ))
}

fn prepare(config: &RuntimeConfig, request: &ControlRequest) -> Result<ControlReply> {
    need(
        config.acknowledge_external_writers_stopped,
        "refusing PrepareUpgrade without acknowledge_external_writers_stopped=true",
    )?;
    let release = request
        .release
        .as_deref()
        .context("PrepareUpgrade requires release")?;
    let manifest = request
        .manifest_sha256
        .as_deref()
        .context("PrepareUpgrade requires manifest_sha256")?;
    verify_release(config, release, Some(manifest), true)?;
    let selector = read_selector(config)?;
    let previous = selector.as_ref().map(|a| a.release_id.clone());
    need(
        previous.as_deref() != Some(release),
        "release is already selected",
    )?;
    let state = load_state(config)?;
    need(
        !maintenance_truth(config, &state)?,
        "maintenance must be released",
    )?;
    if config.panel_gate_file.is_some() {
        read_gate(config)?;
    }
    let tx = match &state.prepared {
        Some(prepared) => {
            need(
                prepared.release == release && prepared.manifest_sha256 == manifest,
                "adapter already holds a different prepared transaction",
            )?;
            need(
                prepared.previous == previous,
                "prepared state previous release does not match the observed selector",
            )?;
            prepared.transaction_id.clone()
        }
        None => format!("prepare-{}-{manifest}", config.instance_id),
    };

    // A brand-new instance has neither a selector nor a Core socket. That state
    // is only actionable with explicit backend evidence; socket absence alone is
    // never treated as "nothing is running".
    if previous.is_none() && !core_socket_present(config)? {
        return prepare_first_install(config, state, tx, release, manifest);
    }

    let response = query(config, CoreOperation::Prepare, &tx, previous.as_deref())?;
    let mapping = ControlRequest {
        protocol: CONTROL_PROTOCOL.into(),
        operation: ControlOperation::PrepareUpgrade,
        instance: config.instance_id.clone(),
        release: previous.clone(),
        manifest_sha256: None,
    };
    let reply = control_client::map_to_systemd_reply(&mapping, response.clone())?;
    let generation = response
        .get("generation")
        .and_then(Value::as_u64)
        .context("prepare ACK missing generation")?;
    need(generation > 0, "prepare generation must be non-zero")?;
    let mut next = state;
    // A selector existed for this prepare: this instance is not a candidate for
    // the first-install path even if the selector disappears later.
    next.installed = true;
    next.prepared = Some(PreparedState {
        transaction_id: tx,
        expected_generation: generation,
        release: release.into(),
        manifest_sha256: manifest.into(),
        previous,
        first_install: None,
    });
    write_state(config, &next)?;
    Ok(reply)
}

/// Prepare a release on an instance that has never had a selector or a running
/// Core. The only accepted proof is a fresh backend evidence document: exact
/// canonical units inactive plus selector absent. The adapter re-reads the real
/// selector and re-checks socket absence before and after validating evidence.
fn prepare_first_install(
    config: &RuntimeConfig,
    state: AdapterState,
    tx: String,
    release: &str,
    manifest: &str,
) -> Result<ControlReply> {
    need(
        !state.installed,
        "adapter state records a previously installed release; \
         refusing to treat a missing selector as a first install",
    )?;
    let (evidence, evidence_sha256) = validate_first_install_evidence(config, release, manifest)?;
    need(
        read_selector(config)?.is_none(),
        "current_selector appeared during first-install prepare",
    )?;
    need(
        !core_socket_present(config)?,
        "Core socket appeared during first-install prepare",
    )?;
    let mut next = state;
    next.prepared = Some(PreparedState {
        transaction_id: tx,
        expected_generation: SYNTHETIC_FIRST_INSTALL_GENERATION,
        release: release.into(),
        manifest_sha256: manifest.into(),
        previous: None,
        first_install: Some(FirstInstallPrepared {
            evidence_sha256,
            observed_at_unix_ms: evidence.observed_at_unix_ms,
            units: evidence.units,
        }),
    });
    write_state(config, &next)?;
    Ok(ControlReply::ack(
        ControlOperation::PrepareUpgrade,
        config.instance_id.clone(),
    ))
}

fn drain(config: &RuntimeConfig) -> Result<ControlReply> {
    let state = load_state(config)?;
    let prepared = state
        .prepared
        .as_ref()
        .context("Drain requires prepared adapter state")?;
    if let Some(first) = &prepared.first_install {
        // No Core exists to answer Status, so this branch must not fabricate a
        // drained ACK from a missing socket. It requires the explicit backend
        // observation again, plus independent selector/socket checks.
        need(
            state.maintenance,
            "first-install drain requires maintenance mode",
        )?;
        need(
            config.acknowledge_external_writers_stopped,
            "refusing first-install Drain without acknowledge_external_writers_stopped=true",
        )?;
        need(
            read_selector(config)?.is_none(),
            "first-install drain requires an absent current_selector",
        )?;
        need(
            !core_socket_present(config)?,
            "first-install drain requires the Core socket to be absent",
        )?;
        let (evidence, evidence_sha256) =
            validate_first_install_evidence(config, &prepared.release, &prepared.manifest_sha256)?;
        need(
            same_unit_observations(&evidence.units, &first.units),
            "first-install evidence units changed since prepare",
        )?;
        if evidence_sha256 != first.evidence_sha256 {
            need(
                evidence.observed_at_unix_ms >= first.observed_at_unix_ms,
                "first-install evidence regressed since prepare",
            )?;
        }
        let mut reply = ControlReply::ack(ControlOperation::Drain, config.instance_id.clone());
        reply.active_work = Some(0);
        reply.current_release = None;
        return Ok(reply);
    }
    let tx = format!("drain-{}", config.instance_id);
    let status = parse_status(&query(config, CoreOperation::Status, &tx, None)?, &tx)?;
    need(
        status.declared == prepared.previous,
        "Core release != prepared pre-upgrade release",
    )?;
    need(status.prepared, "Core is not prepared")?;
    need(status.maintenance_held, "Core does not hold maintenance")?;
    need(
        status.generation == Some(prepared.expected_generation),
        "Core generation != expected generation",
    )?;
    let mut reply = ControlReply::ack(ControlOperation::Drain, config.instance_id.clone());
    reply.active_work = Some(0);
    reply.current_release = status.declared;
    Ok(reply)
}

fn maintenance_enter(config: &RuntimeConfig) -> Result<ControlReply> {
    let mut state = load_state(config)?;
    if let Some(path) = &config.panel_gate_file {
        let mut gate = read_gate(config)?;
        gate.maintenance = true;
        validate_gate(config, &gate)?;
        write_json_atomic(path, &gate, file_mode(&fs::metadata(path)?) & 0o666)?;
    }
    state.maintenance = true;
    write_state(config, &state)?;
    let mut reply = ControlReply::ack(
        ControlOperation::MaintenanceEnter,
        config.instance_id.clone(),
    );
    reply.maintenance = Some(true);
    Ok(reply)
}

fn maintenance_leave(config: &RuntimeConfig) -> Result<ControlReply> {
    let mut state = load_state(config)?;
    let selector = match read_selector(config)? {
        Some(selector) => selector,
        // A failed first install has no release selected and no Core to verify
        // as ready. Releasing maintenance is still bounded by the same
        // backend evidence: units must be observed inactive and the selector
        // must really be absent.
        None => return release_first_install_maintenance(config, state),
    };
    let mut gate = match &config.panel_gate_file {
        Some(_) => Some(read_gate(config)?),
        None => None,
    };
    let tx = format!("leave-{}", config.instance_id);
    let mut status = parse_status(&query(config, CoreOperation::Status, &tx, None)?, &tx)?;
    if status.maintenance_held {
        let prepared = state
            .prepared
            .as_ref()
            .context("Core holds a generation but adapter has no prepared state")?;
        need(
            status.generation == Some(prepared.expected_generation),
            "Core generation != adapter expected generation",
        )?;
        need(
            status.declared == prepared.previous,
            "Core release != prepared previous release",
        )?;
        let cancel_tx = prepared.transaction_id.clone();
        let cancellation = query(
            config,
            CoreOperation::cancel(prepared.expected_generation),
            &cancel_tx,
            None,
        )?;
        check_ack(&cancellation, "cancel", &cancel_tx)?;
        let object = cancellation
            .as_object()
            .context("cancel response is not an object")?;
        need(
            object.get("prepared").and_then(Value::as_bool) == Some(false),
            "cancel did not clear prepared",
        )?;
        need(
            object.get("maintenance_held").and_then(Value::as_bool) == Some(false),
            "cancel did not release maintenance",
        )?;
        status = parse_status(&query(config, CoreOperation::Status, &tx, None)?, &tx)?;
    }
    need(
        !status.maintenance_held && !status.prepared,
        "Core still maintains",
    )?;
    need(status.phase == "idle", "Core phase is not idle")?;
    need(
        status.declared.as_deref() == Some(selector.release_id.as_str()),
        "Core release != selector release",
    )?;
    let pid = verify_running_pid(config, &selector)?;
    let ready_tx = format!("ready-{}", config.instance_id);
    let ready_response = query(
        config,
        CoreOperation::Ready,
        &ready_tx,
        Some(&selector.release_id),
    )?;
    verify_process_still(config, pid)?;
    let ready_request = ControlRequest {
        protocol: CONTROL_PROTOCOL.into(),
        operation: ControlOperation::Ready,
        instance: config.instance_id.clone(),
        release: Some(selector.release_id.clone()),
        manifest_sha256: None,
    };
    control_client::map_to_systemd_reply_with_process_verification(
        &ready_request,
        ready_response,
        true,
    )
    .context("MaintenanceLeave requires a verified ready Core")?;
    state.maintenance = false;
    state.installed = true;
    state.prepared = None;
    write_state(config, &state)?;
    if let (Some(path), Some(mut gate)) = (&config.panel_gate_file, gate.take()) {
        gate.maintenance = false;
        gate.release_id = selector.release_id.clone();
        validate_gate(config, &gate)?;
        let mode = file_mode(&fs::metadata(path)?) & 0o666;
        write_json_atomic(path, &gate, mode)?;
    }
    let mut reply = ControlReply::ack(
        ControlOperation::MaintenanceLeave,
        config.instance_id.clone(),
    );
    reply.maintenance = Some(false);
    reply.current_release = Some(selector.release_id);
    Ok(reply)
}

/// Release maintenance after a failed first install. The caller has already
/// established that the selector is absent. This never reports a current
/// release: the transaction is rolling back to "nothing installed".
fn release_first_install_maintenance(
    config: &RuntimeConfig,
    mut state: AdapterState,
) -> Result<ControlReply> {
    let Some(prepared) = state.prepared.as_ref() else {
        // Idempotent rollback retry: a previous first-install maintenance
        // release may already have cleared the adapter state but failed while
        // updating the Panel gate. There is no release to report and no Core
        // to verify; only a fully quiescent, already-released state is accepted.
        need(
            !state.maintenance,
            "MaintenanceLeave requires an active selector or a prepared first install",
        )?;
        need(
            !core_socket_present(config)?,
            "first-install maintenance release requires a stopped Core",
        )?;
        need(
            read_selector(config)?.is_none(),
            "current_selector appeared during first-install maintenance release",
        )?;
        need(
            first_install_gate_is_clear(config)?,
            "panel gate still reports maintenance without prepared state",
        )?;
        let mut reply = ControlReply::ack(
            ControlOperation::MaintenanceLeave,
            config.instance_id.clone(),
        );
        reply.maintenance = Some(false);
        return Ok(reply);
    };
    let Some(first) = prepared.first_install.as_ref() else {
        bail!("MaintenanceLeave requires an active selector for an existing installation");
    };
    let release = prepared.release.clone();
    let manifest = prepared.manifest_sha256.clone();
    let prepared_evidence_sha256 = first.evidence_sha256.clone();
    let prepared_observed_at = first.observed_at_unix_ms;
    let prepared_units = first.units.clone();

    need(
        !core_socket_present(config)?,
        "first-install maintenance release requires a stopped Core",
    )?;
    let (evidence, evidence_sha256) = validate_first_install_evidence(config, &release, &manifest)?;
    need(
        same_unit_observations(&evidence.units, &prepared_units),
        "first-install evidence units changed since prepare",
    )?;
    if evidence_sha256 != prepared_evidence_sha256 {
        need(
            evidence.observed_at_unix_ms >= prepared_observed_at,
            "first-install evidence regressed since prepare",
        )?;
    }
    need(
        read_selector(config)?.is_none(),
        "current_selector appeared during first-install maintenance release",
    )?;
    need(
        !core_socket_present(config)?,
        "Core socket appeared during first-install maintenance release",
    )?;

    // Clear the Panel gate first: if it cannot be written, the durable adapter
    // state still names the prepared transaction and a retry can try again.
    clear_first_install_gate(config)?;
    state.maintenance = false;
    state.prepared = None;
    write_state(config, &state)?;
    let mut reply = ControlReply::ack(
        ControlOperation::MaintenanceLeave,
        config.instance_id.clone(),
    );
    reply.maintenance = Some(false);
    Ok(reply)
}

fn ready(config: &RuntimeConfig, request: &ControlRequest) -> Result<ControlReply> {
    let release = request
        .release
        .as_deref()
        .context("Ready requires release")?;
    let selector = read_selector(config)?.context("Ready requires active selector")?;
    need(
        selector.release_id == release,
        "selector release != Ready release",
    )?;
    if let Some(expected) = request.manifest_sha256.as_deref() {
        need(
            selector.manifest_sha256 == expected,
            "Ready manifest mismatch",
        )?;
    }
    let pid = verify_running_pid(config, &selector)?;
    let tx = format!("ready-{}", config.instance_id);
    let response = query(config, CoreOperation::Ready, &tx, Some(release))?;
    verify_process_still(config, pid)?;
    control_client::map_to_systemd_reply_with_process_verification(request, response, true)
}
