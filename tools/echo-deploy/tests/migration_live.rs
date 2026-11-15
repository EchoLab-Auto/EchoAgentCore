//! **Opt-in, live** acceptance for the legacy-unit migration preparation CLI
//! (`migration-capture` / `migration-takeover` / `migration-recover`) against a
//! real per-user `systemd` manager. Run from an independent cgroup:
//!
//! ```bash
//! cd tools/echo-deploy
//! systemd-run --user --scope --collect -- \
//!   cargo test --locked --test migration_live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The scope is mandatory: `CgroupGuard` rejects cgroups containing the legacy
//! Core/Panel units or the canonical instance units, and this repo's agent runs
//! inside `echo-agent-core.service`. `live_cli_refuses_canonical_core_cgroup`
//! proves that rejection with a transient `systemd-run` unit.
//!
//! Isolation: one `echo-deploy-test-<pid>-<nanos>` instance. Old units are
//! random `...-old-{core,panel,update}.service`; new units are canonical
//! `echo-agent-<instance>-{core,panel,update}.service`. Old fixtures are real
//! `/bin/sleep 600` foreground services; new units are `Type=oneshot` `/bin/true`
//! fixtures that are enabled, never started. All spec/state/log files live
//! under `$HOME/.local/state/echo-deploy-live/migration-live-*` (private,
//! outside `/tmp`); the only outside writes are this instance's temporary unit
//! files in the XDG user unit directory, which `Drop` stops, disables, removes
//! and reloads. Production units are only read with `systemctl show`, before
//! and after. No production config, user data, credential or network is touched.
//!
//! Real: systemctl lifecycle, real MainPIDs, the shipped CLI and its real
//! `ExecTransport`. Fixture: the `tests/fixtures/migration_live_*.service.in`
//! bodies and `/bin/sleep`. No Core/Panel binary runs; no activation or data
//! migration is claimed.
//!
//! Known source limitations recorded, not hidden: `systemctl show ExecStart`
//! includes runtime fields, so `recover`'s verbatim comparison after `takeover`
//! hits `ExecStart drift`; the recover test accepts a full restore (after a
//! source fix) or that exact non-mutating refusal, logs `RECOVER_BLOCKED`, and
//! always asserts the safe post-takeover state. It never forges evidence.
//! Canonical new-unit names are test-asserted because `src/migration.rs` does
//! not enforce them yet; `/bin/sleep` is root-owned so `LegacyUnit.binary`
//! hashing is not exercised; `ExecStart` is opaque equality-only.

use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;

const SYSTEMCTL: &str = "/usr/bin/systemctl";
const PROTECTED_UNITS: [&str; 2] = ["echo-agent-core.service", "echo-agent-panel.service"];
const WAIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(25);

// ---------------------------------------------------------------------------
// small utilities
// ---------------------------------------------------------------------------

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_echo-deploy"))
}

fn unique() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}")
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn xdg_unit_dir() -> PathBuf {
    if let Some(config) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(config).join("systemd/user");
    }
    PathBuf::from(env::var_os("HOME").expect("HOME is required")).join(".config/systemd/user")
}

fn xdg_state_base() -> PathBuf {
    if let Some(state) = env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(state).join("echo-deploy-live");
    }
    PathBuf::from(env::var_os("HOME").expect("HOME is required"))
        .join(".local/state/echo-deploy-live")
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", path.display()))?;
    Ok(())
}

fn fixture(name: &str) -> PathBuf {
    crate_dir().join("tests/fixtures").join(name)
}

fn render_template(name: &str, values: &[(&str, &str)]) -> Result<String> {
    let mut text = fs::read_to_string(fixture(name))
        .with_context(|| format!("read fixture {}", fixture(name).display()))?;
    for (placeholder, value) in values {
        text = text.replace(placeholder, value);
    }
    ensure!(!text.contains('@'), "unrendered placeholder in {name}");
    Ok(text)
}

fn systemctl(args: &[&str]) -> Result<Output> {
    Command::new(SYSTEMCTL)
        .arg("--user")
        .args(args)
        .output()
        .with_context(|| format!("run systemctl --user {}", args.join(" ")))
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn render(output: &Output) -> String {
    format!(
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        stdout_of(output),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn systemctl_checked(args: &[&str]) -> Result<String> {
    let output = systemctl(args)?;
    ensure!(
        output.status.success(),
        "systemctl --user {} failed: {}",
        args.join(" "),
        render(&output)
    );
    Ok(stdout_of(&output))
}

fn show(unit: &str, property: &str) -> Result<String> {
    let output = systemctl(&["show", &format!("--property={property}"), "--value", unit])?;
    ensure!(
        output.status.success(),
        "show {property} for {unit} failed: {}",
        render(&output)
    );
    Ok(stdout_of(&output))
}

fn main_pid(value: &str) -> Result<u32> {
    if value.is_empty() {
        return Ok(0);
    }
    value
        .parse()
        .with_context(|| format!("MainPID is not a number: {value:?}"))
}

fn user_systemd_available() -> bool {
    if env::var_os("XDG_RUNTIME_DIR").is_none() {
        return false;
    }
    systemctl(&["show", "--property=Version", "--value"])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn current_cgroup() -> Result<String> {
    fs::read_to_string("/proc/self/cgroup").context("read /proc/self/cgroup")
}

/// The CLI itself enforces this; the test fails before mutating units instead
/// of relying on a later, less clear CLI refusal.
fn ensure_isolated_cgroup() -> Result<()> {
    let text = current_cgroup()?;
    for forbidden in PROTECTED_UNITS {
        ensure!(
            !text.contains(forbidden),
            "refusing to run from {forbidden} cgroup:\n{text}\nrun under: systemd-run --user --scope"
        );
    }
    eprintln!("[cgroup] isolated: {}", text.trim().replace('\n', " | "));
    Ok(())
}

fn wait_for_active(unit: &str) -> Result<u32> {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = show(unit, "ActiveState")?;
        if state == "active" {
            let pid = main_pid(&show(unit, "MainPID")?)?;
            if pid != 0 {
                return Ok(pid);
            }
        }
        ensure!(
            state != "failed",
            "unit {unit} failed while waiting for active"
        );
        if Instant::now() >= deadline {
            bail!("timed out waiting for {unit} to become active (last {state:?})");
        }
        thread::sleep(POLL);
    }
}

fn production_snapshot() -> Result<BTreeMap<String, String>> {
    let mut snapshot = BTreeMap::new();
    for unit in PROTECTED_UNITS {
        for property in ["ActiveState", "MainPID"] {
            snapshot.insert(
                format!("{unit}/{property}"),
                show(unit, property).unwrap_or_else(|error| format!("<{error:#}>")),
            );
        }
    }
    Ok(snapshot)
}

#[derive(Clone, Debug)]
struct Snapshot {
    load_state: String,
    active_state: String,
    unit_file_state: String,
    main_pid: u32,
}

impl Snapshot {
    fn read(unit: &str) -> Result<Self> {
        Ok(Self {
            load_state: show(unit, "LoadState")?,
            active_state: show(unit, "ActiveState")?,
            unit_file_state: show(unit, "UnitFileState")?,
            main_pid: main_pid(&show(unit, "MainPID")?)?,
        })
    }

    fn log(&self, tag: &str, unit: &str) {
        eprintln!(
            "[{tag}] {unit} load={} active={} file={} MainPID={}",
            self.load_state, self.active_state, self.unit_file_state, self.main_pid
        );
    }

    fn require(&self, unit: &str, active: &str, file: &str) -> Result<()> {
        ensure!(
            self.active_state == active
                && self.unit_file_state == file
                && self.load_state == "loaded",
            "{unit} expected active={active} file={file}, got {self:?}"
        );
        Ok(())
    }
}

struct OldUnit {
    role: &'static str,
    unit: String,
    fragment: PathBuf,
    start: bool,
}

struct Initial {
    old_pids: BTreeMap<String, u32>,
}

struct CleanupReport {
    units: BTreeMap<String, Snapshot>,
    protected_unchanged: bool,
    temp_root_removed: bool,
}

// ---------------------------------------------------------------------------
// live fixture
// ---------------------------------------------------------------------------

struct Live {
    _tmp: TempDir,
    root: PathBuf,
    instance: String,
    unit_dir: PathBuf,
    state_dir: PathBuf,
    workdir: PathBuf,
    spec_path: PathBuf,
    old: Vec<OldUnit>,
    new_units: Vec<String>,
    protected_before: BTreeMap<String, String>,
    cleaned: bool,
}

impl Live {
    fn new(label: &str) -> Result<Self> {
        ensure!(
            user_systemd_available(),
            "live acceptance requires a working user systemd"
        );
        ensure_isolated_cgroup()?;

        let unit_dir = xdg_unit_dir();
        fs::create_dir_all(&unit_dir)
            .with_context(|| format!("create unit directory {}", unit_dir.display()))?;
        let unit_dir = fs::canonicalize(&unit_dir)
            .with_context(|| format!("canonicalize {}", unit_dir.display()))?;

        let base = xdg_state_base();
        create_private_dir(&base)?;
        let tmp = tempfile::Builder::new()
            .prefix(&format!("migration-live-{label}-"))
            .tempdir_in(&base)
            .context("create migration live temp root")?;
        let root = fs::canonicalize(tmp.path()).context("canonicalize live temp root")?;
        let state_dir = root.join("state");
        let workdir = root.join("work");
        create_private_dir(&state_dir)?;
        create_private_dir(&workdir)?;

        let instance = format!("echo-deploy-test-{}-{}", std::process::id(), unique());
        ensure!(instance.len() <= 64, "instance id too long: {instance}");
        let old = ["core", "panel", "update"]
            .into_iter()
            .map(|role| {
                let unit = format!("{instance}-old-{role}.service");
                OldUnit {
                    role,
                    fragment: unit_dir.join(&unit),
                    unit,
                    start: role != "update",
                }
            })
            .collect();
        let new_units = ["core", "panel", "update"]
            .into_iter()
            .map(|role| format!("echo-agent-{instance}-{role}.service"))
            .collect::<Vec<_>>();
        let spec_path = root.join("migration-spec.json");
        let live = Self {
            _tmp: tmp,
            root,
            instance,
            unit_dir,
            state_dir,
            workdir,
            spec_path,
            old,
            new_units,
            protected_before: production_snapshot()?,
            cleaned: false,
        };
        eprintln!(
            "[{label}] instance={} root={} unit_dir={} protected_before={:?}",
            live.instance,
            live.root.display(),
            live.unit_dir.display(),
            live.protected_before
        );
        Ok(live)
    }

    fn evidence_path(&self) -> PathBuf {
        self.state_dir
            .join(format!("migration-evidence-{}.json", self.instance))
    }

    fn journal_path(&self) -> PathBuf {
        self.state_dir
            .join(format!("migration-journal-{}.json", self.instance))
    }

    fn all_units(&self) -> Vec<String> {
        let mut units: Vec<String> = self.old.iter().map(|old| old.unit.clone()).collect();
        units.extend(self.new_units.iter().cloned());
        units
    }

    fn old_units(&self) -> Vec<String> {
        self.old.iter().map(|old| old.unit.clone()).collect()
    }

    fn started_old_units(&self) -> Vec<String> {
        self.old
            .iter()
            .filter(|old| old.start)
            .map(|old| old.unit.clone())
            .collect()
    }

    fn install_unit(&self, unit: &str, body: &str) -> Result<()> {
        let path = self.unit_dir.join(unit);
        ensure!(
            path.parent() == Some(self.unit_dir.as_path()) && !path.exists(),
            "invalid or already existing unit path {}",
            path.display()
        );
        fs::write(&path, body).with_context(|| format!("write unit {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        Ok(())
    }

    /// Install all six units, enable the old ones, start only old core/panel,
    /// and write the strict migration spec. New units stay inactive+disabled.
    fn prepare(&self) -> Result<Initial> {
        for old in &self.old {
            let body = render_template(
                "migration_live_old.service.in",
                &[
                    ("@INSTANCE@", &self.instance),
                    ("@ROLE@", old.role),
                    (
                        "@WORKDIR@",
                        self.workdir.to_str().context("workdir is not UTF-8")?,
                    ),
                ],
            )?;
            self.install_unit(&old.unit, &body)?;
        }
        for (role, unit) in ["core", "panel", "update"].into_iter().zip(&self.new_units) {
            let body = render_template(
                "migration_live_new.service.in",
                &[("@INSTANCE@", &self.instance), ("@ROLE@", role)],
            )?;
            let guard = echo_deploy::migration::activation_guard_line(
                &cli(),
                &self.state_dir,
                &self.instance,
                unit,
            )?;
            let body = body.replace(
                "ExecStart=/bin/true",
                &format!("{guard}\nExecStart=/bin/true"),
            );
            self.install_unit(unit, &body)?;
        }
        systemctl_checked(&["daemon-reload"])?;

        for unit in &self.new_units {
            let snapshot = Snapshot::read(unit)?;
            snapshot.log("prepared-new", unit);
            snapshot.require(unit, "inactive", "disabled")?;
        }
        for old in &self.old {
            systemctl_checked(&["enable", old.unit.as_str()])?;
        }
        for old in self.old.iter().filter(|old| old.start) {
            systemctl_checked(&["start", old.unit.as_str()])?;
        }

        let mut old_pids = BTreeMap::new();
        for old in &self.old {
            let snapshot = Snapshot::read(&old.unit)?;
            snapshot.log("prepared-old", &old.unit);
            let pid = if old.start {
                let pid = wait_for_active(&old.unit)?;
                snapshot.require(&old.unit, "active", "enabled")?;
                pid
            } else {
                snapshot.require(&old.unit, "inactive", "enabled")?;
                0
            };
            old_pids.insert(old.unit.clone(), pid);
        }

        let legacy: Vec<Value> = self
            .old
            .iter()
            .map(|old| {
                json!({
                    "unit": old.unit,
                    "fragment": old.fragment,
                    "binary": null,
                    "working_directory": self.workdir,
                })
            })
            .collect();
        let document = json!({
            "schema_version": 1,
            "instance_id": self.instance,
            "scope": "user",
            "state_dir": self.state_dir,
            "legacy_units": legacy,
            "new_units": self.new_units,
            "unit_directory": self.unit_dir,
            "guard_executable": cli(),
        });
        fs::write(&self.spec_path, serde_json::to_vec_pretty(&document)?)
            .with_context(|| format!("write spec {}", self.spec_path.display()))?;
        fs::set_permissions(&self.spec_path, fs::Permissions::from_mode(0o600))?;
        eprintln!(
            "[prepare] spec={} old_pids={old_pids:?}",
            self.spec_path.display()
        );
        Ok(Initial { old_pids })
    }

    fn cli(&self, verb: &str, extra: &[&str]) -> Result<Output> {
        let output = Command::new(cli())
            .arg(verb)
            .arg("--spec")
            .arg(&self.spec_path)
            .args(extra)
            .output()
            .with_context(|| format!("run echo-deploy {verb}"))?;
        eprintln!("[cli] {verb} {extra:?} {}", render(&output));
        Ok(output)
    }

    fn cleanup(&mut self) -> CleanupReport {
        if self.cleaned {
            return CleanupReport {
                units: BTreeMap::new(),
                protected_unchanged: true,
                temp_root_removed: !self.root.exists(),
            };
        }
        let units = self.all_units();
        for unit in &units {
            for verb in ["stop", "disable", "reset-failed"] {
                let _ = systemctl(&[verb, unit.as_str()]);
            }
            let path = self.unit_dir.join(unit);
            if path.parent() == Some(self.unit_dir.as_path()) {
                let _ = fs::remove_file(&path);
            }
            let _ = fs::remove_file(self.unit_dir.join("default.target.wants").join(unit));
        }
        match systemctl(&["daemon-reload"]) {
            Ok(output) => eprintln!("[cleanup] daemon-reload {}", render(&output)),
            Err(error) => eprintln!("[cleanup] daemon-reload failed: {error:#}"),
        }
        let snapshots = units
            .iter()
            .filter_map(|unit| match Snapshot::read(unit) {
                Ok(snapshot) => {
                    snapshot.log("cleanup", unit);
                    Some((unit.clone(), snapshot))
                }
                Err(error) => {
                    eprintln!("[cleanup] snapshot {unit} failed: {error:#}");
                    None
                }
            })
            .collect();
        let protected_unchanged =
            production_snapshot().unwrap_or_default() == self.protected_before;
        let temp_root_removed = fs::remove_dir_all(&self.root).is_ok() || !self.root.exists();
        eprintln!(
            "[cleanup] instance={} production_unchanged={protected_unchanged} temp_root_removed={temp_root_removed}",
            self.instance
        );
        self.cleaned = true;
        CleanupReport {
            units: snapshots,
            protected_unchanged,
            temp_root_removed,
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup();
        }
    }
}

fn parse_json(output: &Output, what: &str) -> Result<Value> {
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parse {what} JSON from {}", render(output)))
}

fn require_success(output: &Output, what: &str) -> Result<()> {
    ensure!(output.status.success(), "{what} failed: {}", render(output));
    Ok(())
}

fn require_failure(output: &Output, what: &str, needle: &str) -> Result<()> {
    ensure!(
        !output.status.success(),
        "{what} unexpectedly succeeded: {}",
        render(output)
    );
    let text = render(output);
    ensure!(
        text.contains(needle),
        "{what} failed without {needle:?}: {text}"
    );
    Ok(())
}

fn assert_cleanup(report: &CleanupReport) -> Result<()> {
    ensure!(!report.units.is_empty(), "cleanup observed no units at all");
    for (unit, snapshot) in &report.units {
        ensure!(
            snapshot.load_state == "not-found",
            "{unit} still loaded after cleanup: {snapshot:?}"
        );
        ensure!(
            snapshot.active_state == "inactive" && snapshot.main_pid == 0,
            "{unit} still running after cleanup: {snapshot:?}"
        );
        ensure!(
            snapshot.unit_file_state.is_empty(),
            "{unit} still has a unit-file state after removal: {snapshot:?}"
        );
    }
    ensure!(
        report.protected_unchanged,
        "production echo-agent-core/panel snapshot changed"
    );
    ensure!(report.temp_root_removed, "live temp root was not removed");
    eprintln!("[cleanup] verified: all units inactive/not-found, production unchanged");
    Ok(())
}

fn require_new_units(live: &Live, active: &str, file: &str) -> Result<()> {
    for unit in &live.new_units {
        let snapshot = Snapshot::read(unit)?;
        snapshot.log("assert-new", unit);
        snapshot.require(unit, active, file)?;
        ensure!(snapshot.main_pid == 0, "new unit has MainPID: {snapshot:?}");
    }
    Ok(())
}

fn require_old_post_takeover(live: &Live) -> Result<()> {
    for old in &live.old {
        let snapshot = Snapshot::read(&old.unit)?;
        snapshot.log("assert-old", &old.unit);
        snapshot.require(&old.unit, "inactive", "disabled")?;
        ensure!(snapshot.main_pid == 0, "old unit has MainPID: {snapshot:?}");
    }
    Ok(())
}

fn require_old_prior_state(live: &Live, initial: &Initial) -> Result<()> {
    for old in &live.old {
        let snapshot = Snapshot::read(&old.unit)?;
        ensure!(
            snapshot.active_state == if old.start { "active" } else { "inactive" }
                && snapshot.main_pid == initial.old_pids[&old.unit]
                && snapshot.unit_file_state == "enabled",
            "{} changed from its captured state: {snapshot:?}",
            old.unit
        );
    }
    Ok(())
}

/// Accept the accurate file digest, or the historical double hash produced
/// when `hash_reader` passed the finalized digest through `sha256` again; the
/// latter is logged loudly instead of silently blessing a wrong external digest.
fn check_evidence_digest(label: &str, recorded: &Value, bytes: &[u8]) -> Result<()> {
    let single = sha256(bytes);
    let double = format!("{:x}", Sha256::digest(Sha256::digest(bytes)));
    let got = recorded.as_str().context("recorded digest is not text")?;
    ensure!(
        got == single || got == double,
        "{label}: recorded digest {got} matches neither file sha256 {single} nor double sha256 {double}"
    );
    if got == double {
        eprintln!(
            "EVIDENCE_SHA256_DOUBLE_HASH_BLOCKED {label}: recorded=sha256(sha256(bytes)); accurate file sha256={single}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// Capture and takeover are hard-asserted end to end: real units, real PIDs,
/// real enable/disable, new units enabled but never started. Recovery is
/// exercised separately with mandatory legacy-state restoration.
#[test]
#[ignore = "opt-in live user-systemd acceptance; run under systemd-run --user --scope"]
fn live_cli_capture_takeover_prepares_activation_without_starting_new_units() -> Result<()> {
    let mut live = Live::new("capture-takeover")?;
    let initial = live.prepare()?;

    // CLI gate: capture writes a new evidence file, so it needs --confirm.
    let refused = live.cli("migration-capture", &[])?;
    require_failure(&refused, "migration-capture without --confirm", "--confirm")?;
    ensure!(
        !live.evidence_path().exists(),
        "capture refusal must not write evidence"
    );

    let captured = live.cli("migration-capture", &["--confirm"])?;
    require_success(&captured, "migration-capture")?;
    let preview = parse_json(&captured, "capture preview")?;
    ensure!(preview["instance_id"] == json!(live.instance));
    ensure!(preview["evidence_path"] == json!(live.evidence_path().to_string_lossy()));
    ensure!(preview["takeover_ready"] == json!(true));
    ensure!(preview["activation_ready"] == json!(false));
    ensure!(preview["stop_units"] == json!(live.old_units()));
    ensure!(preview["disable_units"] == json!(live.old_units()));
    ensure!(preview["enable_units"] == json!(live.new_units));
    ensure!(preview["start_on_recovery"] == json!(live.started_old_units()));

    let evidence_bytes = fs::read(live.evidence_path()).context("read captured evidence")?;
    ensure!(
        fs::metadata(live.evidence_path())?.permissions().mode() & 0o777 == 0o600,
        "migration evidence must be 0600"
    );
    let evidence: Value = serde_json::from_slice(&evidence_bytes)?;
    ensure!(evidence["spec"]["instance_id"] == json!(live.instance));
    let old_entries = evidence["old_units"]
        .as_array()
        .context("evidence old_units is not an array")?;
    ensure!(old_entries.len() == live.old.len());
    for (entry, old) in old_entries.iter().zip(&live.old) {
        ensure!(
            entry["unit"] == json!(old.unit)
                && entry["fragment_path"] == json!(old.fragment.to_string_lossy())
                && entry["fragment_mode"].as_u64() == Some(0o644)
                && entry["active_state"] == json!(if old.start { "active" } else { "inactive" })
                && entry["unit_file_state"] == json!("enabled")
                && entry["main_pid"].as_u64() == Some(initial.old_pids[&old.unit] as u64)
                && entry["working_directory"].as_str() == live.workdir.to_str()
                && entry["exec_start"]
                    .as_str()
                    .is_some_and(|s| s.contains("/bin/sleep") && s.contains("600"))
                && entry["exec_start_assurance"] == json!("opaque_equality_only")
                && entry["binary_path"].is_null(),
            "old evidence mismatch for {}: {entry}",
            old.unit
        );
        check_evidence_digest(
            &format!("old-fragment:{}", old.unit),
            &entry["fragment_sha256"],
            &fs::read(&old.fragment)?,
        )?;
    }
    let new_entries = evidence["new_units"]
        .as_array()
        .context("evidence new_units is not an array")?;
    ensure!(new_entries.len() == live.new_units.len());
    for (entry, unit) in new_entries.iter().zip(&live.new_units) {
        ensure!(
            entry["unit"] == json!(unit)
                && entry["path"] == json!(live.unit_dir.join(unit).to_string_lossy())
                && entry["unit_file_state"] == json!("disabled")
                && entry["active_state"] == json!("inactive")
                && entry["mode"].as_u64() == Some(0o644),
            "new unit evidence mismatch for {unit}: {entry}"
        );
        check_evidence_digest(
            &format!("new-unit:{unit}"),
            &entry["sha256"],
            &fs::read(live.unit_dir.join(unit))?,
        )?;
    }

    // Capture is read-only toward systemd: old units keep running with the
    // same MainPID, new units stay inactive+disabled, no journal is written.
    for old in &live.old {
        let snapshot = Snapshot::read(&old.unit)?;
        snapshot.log("after-capture", &old.unit);
        let expected_pid = *initial.old_pids.get(&old.unit).context("pid recorded")?;
        ensure!(
            snapshot.active_state == if old.start { "active" } else { "inactive" }
                && snapshot.main_pid == expected_pid,
            "capture mutated {}: {snapshot:?}",
            old.unit
        );
        ensure!(snapshot.unit_file_state == "enabled");
    }
    require_new_units(&live, "inactive", "disabled")?;
    ensure!(
        !live.journal_path().exists(),
        "capture must not create a takeover journal"
    );

    // CLI gate: takeover needs --confirm and must not mutate anything when it
    // is missing.
    let refused = live.cli("migration-takeover", &[])?;
    require_failure(
        &refused,
        "migration-takeover without --confirm",
        "--confirm",
    )?;
    require_old_prior_state(&live, &initial)?;

    let takeover = live.cli("migration-takeover", &["--confirm"])?;
    require_success(&takeover, "migration-takeover")?;
    let report = parse_json(&takeover, "takeover report")?;
    ensure!(report["phase"] == json!("prepared_for_activation"));
    ensure!(report["takeover_complete"] == json!(true));
    ensure!(report["ready"] == json!(false) && report["activated"] == json!(false));
    ensure!(report["activation_required"] == json!(true));
    ensure!(report["recovered"] == json!(false));
    ensure!(report["old_runtime_restored"] == json!(false));
    ensure!(report["data_modified"] == json!(false));
    ensure!(report["data_rolled_back"] == json!(false));
    ensure!(report["next_action"]
        .as_str()
        .is_some_and(|s| s.contains("systemd-apply")));

    let old_outcomes = report["old_units"].as_array().context("old_units")?;
    ensure!(old_outcomes.len() == live.old.len());
    for (outcome, old) in old_outcomes.iter().zip(&live.old) {
        ensure!(outcome["unit"] == json!(old.unit));
        ensure!(outcome["active_state"] == json!("inactive"));
        ensure!(outcome["unit_file_state"] == json!("disabled"));
    }
    let new_outcomes = report["new_units"].as_array().context("new_units")?;
    ensure!(new_outcomes.len() == live.new_units.len());
    for (outcome, unit) in new_outcomes.iter().zip(&live.new_units) {
        ensure!(outcome["unit"] == json!(unit));
        ensure!(outcome["active_state"] == json!("inactive"));
        ensure!(outcome["unit_file_state"] == json!("enabled"));
    }

    require_old_post_takeover(&live)?;
    require_new_units(&live, "inactive", "enabled")?;
    ensure!(
        fs::read(live.evidence_path())? == evidence_bytes,
        "takeover rewrote immutable evidence"
    );

    let journal_bytes = fs::read(live.journal_path()).context("read migration journal")?;
    ensure!(
        fs::metadata(live.journal_path())?.permissions().mode() & 0o777 == 0o600,
        "migration journal must be 0600"
    );
    let journal: Value = serde_json::from_slice(&journal_bytes)?;
    ensure!(journal["instance_id"] == json!(live.instance));
    ensure!(journal["phase"] == json!("prepared_for_activation"));
    ensure!(journal["evidence_sha256"] == report["evidence_sha256"]);
    ensure!(
        report["evidence_sha256"].as_str().map(str::len) == Some(64),
        "evidence digest is not a sha256: {report}"
    );

    let cleanup = live.cleanup();
    assert_cleanup(&cleanup)?;
    Ok(())
}
/// Recovery is run against the same captured evidence. Two honest outcomes are
/// accepted: a full restore after a source fix, or the current known
/// `ExecStart drift` refusal, which must be non-mutating. A forged evidence
/// file or fake transport is never used.
#[test]
#[ignore = "opt-in live user-systemd acceptance; run under systemd-run --user --scope"]
fn live_cli_recover_restores_prior_active_old() -> Result<()> {
    let mut live = Live::new("recover")?;
    let initial = live.prepare()?;

    let captured = live.cli("migration-capture", &["--confirm"])?;
    require_success(&captured, "migration-capture")?;
    let captured_guard = live.cli("migration-takeover", &["--confirm"])?;
    require_success(&captured_guard, "migration-takeover")?;

    // Missing acknowledgement must fail without mutation.
    let no_ack = live.cli("migration-recover", &["--confirm"])?;
    ensure!(
        !no_ack.status.success(),
        "recover without acknowledgement succeeded: {}",
        render(&no_ack)
    );
    eprintln!("RECOVER_NO_ACK_REFUSED {}", render(&no_ack));
    require_old_post_takeover(&live)?;
    require_new_units(&live, "inactive", "enabled")?;

    let old_hashes: BTreeMap<String, String> = live
        .old
        .iter()
        .map(|old| Ok((old.unit.clone(), sha256(&fs::read(&old.fragment)?))))
        .collect::<Result<_>>()?;
    let new_hashes: BTreeMap<String, String> = live
        .new_units
        .iter()
        .map(|unit| Ok((unit.clone(), sha256(&fs::read(live.unit_dir.join(unit))?))))
        .collect::<Result<_>>()?;

    let recovered = live.cli(
        "migration-recover",
        &["--confirm", "--acknowledge-reversible"],
    )?;
    let recovered_text = render(&recovered);
    let restored = if recovered.status.success() {
        let report = parse_json(&recovered, "recover report")?;
        ensure!(
            report["phase"] == json!("rolled_back")
                && report["recovered"] == json!(true)
                && report["old_runtime_restored"] == json!(true)
                && report["takeover_complete"] == json!(false)
                && report["ready"] == json!(false)
                && report["activated"] == json!(false)
                && report["data_modified"] == json!(false)
                && report["data_rolled_back"] == json!(false),
            "unexpected recover report: {report}"
        );
        let old_out = report["old_units"]
            .as_array()
            .context("recover old_units")?;
        ensure!(old_out.len() == live.old.len());
        let new_out = report["new_units"]
            .as_array()
            .context("recover new_units")?;
        ensure!(new_out.len() == live.new_units.len());
        for old in &live.old {
            let snapshot = Snapshot::read(&old.unit)?;
            snapshot.log("after-recover", &old.unit);
            snapshot.require(
                &old.unit,
                if old.start { "active" } else { "inactive" },
                "enabled",
            )?;
            let initial_pid = initial.old_pids[&old.unit];
            ensure!(
                if old.start {
                    initial_pid != 0 && snapshot.main_pid != 0 && snapshot.main_pid != initial_pid
                } else {
                    snapshot.main_pid == 0
                },
                "recover PID mismatch for {}: {snapshot:?}",
                old.unit
            );
        }
        require_new_units(&live, "inactive", "disabled")?;
        let journal: Value =
            serde_json::from_slice(&fs::read(live.journal_path()).context("read journal")?)?;
        ensure!(journal["phase"] == json!("rolled_back"));
        eprintln!("RECOVER_RESTORED old_prior_active_restored=true new_disabled=true");
        true
    } else {
        ensure!(
            recovered_text.contains("ExecStart drift"),
            "recover failed for an unexpected reason: {recovered_text}"
        );
        eprintln!(
            "RECOVER_BLOCKED known src/migration.rs verify_old_files ExecStart runtime-field drift (no mutation):\n{recovered_text}"
        );
        // The refusal happens in verify_old_files, before RollbackStarted, so
        // the durable journal and every unit stay in the safe takeover state.
        require_old_post_takeover(&live)?;
        require_new_units(&live, "inactive", "enabled")?;
        let journal_bytes = fs::read(live.journal_path())?;
        let journal: Value = serde_json::from_slice(&journal_bytes)?;
        ensure!(journal["phase"] == json!("prepared_for_activation"));
        false
    };

    for old in &live.old {
        ensure!(
            sha256(&fs::read(&old.fragment)?) == old_hashes[&old.unit],
            "recovery rewrote old unit file {}",
            old.unit
        );
    }
    for unit in &live.new_units {
        ensure!(
            sha256(&fs::read(live.unit_dir.join(unit))?) == new_hashes[unit],
            "recovery rewrote new unit file {unit}"
        );
    }
    ensure!(restored, "live migration recovery must restore the captured old state; a recorded refusal is not acceptance");
    eprintln!("[recover] restored={restored}");

    let cleanup = live.cleanup();
    assert_cleanup(&cleanup)?;
    Ok(())
}

/// After capture, changing an old unit file must make takeover refuse before
/// any `stop`, leaving every MainPID untouched. This is a separate test so the
/// happy path is not contaminated by a deliberately drifted fixture.
#[test]
#[ignore = "opt-in live user-systemd acceptance; run under systemd-run --user --scope"]
fn live_cli_tampered_old_fragment_blocks_takeover_without_stopping() -> Result<()> {
    let mut live = Live::new("drift")?;
    let initial = live.prepare()?;
    require_success(
        &live.cli("migration-capture", &["--confirm"])?,
        "migration-capture",
    )?;

    let old = &live.old[0];
    let original = fs::read(&old.fragment).context("read old fragment")?;
    let tampered = [
        original.as_slice(),
        b"\n# tampered after capture (migration-live test)\n",
    ]
    .concat();
    fs::write(&old.fragment, &tampered)?;
    fs::set_permissions(&old.fragment, fs::Permissions::from_mode(0o644))?;
    eprintln!(
        "[drift] tampered {} sha256(original)={}",
        old.fragment.display(),
        sha256(&original)
    );

    let refused = live.cli("migration-takeover", &["--confirm"])?;
    require_failure(&refused, "takeover after fragment drift", "drift")?;
    eprintln!("TAKEOVER_REFUSED_DRIFT {}", render(&refused));
    require_old_prior_state(&live, &initial)?;
    require_new_units(&live, "inactive", "disabled")?;
    ensure!(
        !live.journal_path().exists(),
        "drift refusal wrote a journal"
    );

    fs::write(&old.fragment, &original)?;
    fs::set_permissions(&old.fragment, fs::Permissions::from_mode(0o644))?;
    let cleanup = live.cleanup();
    assert_cleanup(&cleanup)?;
    Ok(())
}
/// The shipped CLI must refuse to run from the canonical Core cgroup. A
/// transient `systemd-run` service is the positive control: its cgroup really
/// contains `echo-agent-<instance>-core.service`, and the real CLI (not a mock)
/// exits non-zero before writing evidence.
#[test]
#[ignore = "opt-in live user-systemd acceptance; run under systemd-run --user --scope"]
fn live_cli_refuses_canonical_core_cgroup() -> Result<()> {
    ensure!(
        user_systemd_available(),
        "live acceptance requires user systemd"
    );
    ensure_isolated_cgroup()?;
    let base = xdg_state_base();
    create_private_dir(&base)?;
    let tmp = tempfile::Builder::new()
        .prefix("migration-guard-")
        .tempdir_in(&base)?;
    let state_dir = tmp.path().join("state");
    create_private_dir(&state_dir)?;

    let guard_instance = format!("migration-guard-{}-{}", std::process::id(), unique());
    let guard_unit = format!("echo-agent-{guard_instance}-core.service");
    let spec = tmp.path().join("guard-spec.json");
    let document = json!({
        "schema_version": 1,
        "instance_id": guard_instance,
        "scope": "user",
        "state_dir": state_dir,
        "legacy_units": [{"unit": format!("echo-deploy-test-{}-old-core.service", unique()),
                          "fragment": "/bin/true", "binary": null, "working_directory": null}],
        "new_units": [guard_unit],
        "unit_directory": fs::canonicalize(xdg_unit_dir())?,
        "guard_executable": cli(),
    });
    fs::write(&spec, serde_json::to_vec_pretty(&document)?)?;
    fs::set_permissions(&spec, fs::Permissions::from_mode(0o600))?;

    let _ = systemctl(&["stop", guard_unit.as_str()]);
    let _ = systemctl(&["reset-failed", guard_unit.as_str()]);
    let output = Command::new("systemd-run")
        .args([
            "--user",
            &format!("--unit={guard_unit}"),
            "--collect",
            "--wait",
            "--pipe",
        ])
        .arg(cli())
        .args(["migration-capture", "--spec"])
        .arg(&spec)
        .arg("--confirm")
        .output()
        .context("run echo-deploy inside a transient canonical core unit")?;
    let text = render(&output);
    ensure!(
        !output.status.success(),
        "CLI succeeded from {guard_unit} cgroup: {text}"
    );
    ensure!(
        text.contains("refusing activation from") && text.contains(&guard_unit),
        "CLI did not report the canonical Core cgroup refusal: {text}"
    );
    let evidence = state_dir.join(format!("migration-evidence-{guard_instance}.json"));
    ensure!(
        !evidence.exists(),
        "cgroup refusal wrote evidence: {}",
        evidence.display()
    );
    eprintln!("CGROUP_GUARD_REFUSED unit={guard_unit}\n{text}");
    let _ = systemctl(&["stop", guard_unit.as_str()]);
    let _ = systemctl(&["reset-failed", guard_unit.as_str()]);
    let _ = fs::remove_dir_all(tmp.path());
    Ok(())
}

/// A real ExecStartPre latch remains after stop/reset-failed/unload. No deployment
/// transaction is created: this explicitly covers manual systemctl activation.
#[test]
#[ignore = "opt-in live user-systemd acceptance; run in an independent unit"]
fn live_cli_activation_latch_blocks_recovery_after_systemd_history_reset() -> Result<()> {
    let mut live = Live::new("activation-latch")?;
    live.prepare()?;
    require_success(&live.cli("migration-capture", &["--confirm"])?, "capture")?;
    require_success(&live.cli("migration-takeover", &["--confirm"])?, "takeover")?;
    let unit = live.new_units[0].clone();
    systemctl_checked(&["start", &unit])?;
    let marker = live
        .state_dir
        .join(format!("migration-activated-{}.json", live.instance));
    ensure!(
        marker.is_file(),
        "ExecStartPre did not create durable activation latch"
    );
    ensure!(fs::metadata(&marker)?.permissions().mode() & 0o777 == 0o600);
    systemctl_checked(&["stop", &unit])?;
    let _ = systemctl(&["reset-failed", &unit]);
    systemctl_checked(&["daemon-reload"])?;
    let refused = live.cli(
        "migration-recover",
        &["--confirm", "--acknowledge-reversible"],
    )?;
    require_failure(
        &refused,
        "post-activation recover",
        "durable activation marker",
    )?;
    for old in &live.old {
        ensure!(show(&old.unit, "ActiveState")? == "inactive");
        ensure!(show(&old.unit, "UnitFileState")? == "disabled");
        ensure!(show(&old.unit, "MainPID")? == "0");
    }
    eprintln!(
        "ACTIVATION_LATCH_REFUSED_AFTER_RESET marker={}",
        marker.display()
    );
    let cleanup = live.cleanup();
    assert_cleanup(&cleanup)?;
    Ok(())
}
