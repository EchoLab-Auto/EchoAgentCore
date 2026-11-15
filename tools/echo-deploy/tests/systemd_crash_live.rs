//! **Opt-in, live** acceptance for *real interruption recovery* of
//! `echo-deploy systemd-apply` against a real user `systemd` manager.
//!
//! Run it from a transient scope, never from the production Core/Panel cgroup
//! (the deploy CLI refuses that on its own):
//!
//! ```bash
//! cd tools/echo-deploy
//! systemd-run --user --scope \
//!   cargo test --offline --test systemd_crash_live -- \
//!     --ignored --nocapture --test-threads=1
//! ```
//!
//! What this test proves
//! ---------------------
//!
//! 1. A real `echo-deploy systemd-apply` process upgrades a pre-installed,
//!    *running* release (`fixture-r1`) and reaches the pre-commit window where
//!    the durable `current` selector already points at the target release
//!    (`fixture-r2-crash`) and the target unit is `activating` but has **not**
//!    reached `Ready`.
//! 2. That `systemd-apply` process is killed with `SIGKILL` at exactly that
//!    observed condition. Nothing waits on an unconditional sleep: the kill
//!    happens only after the selector, `ActiveState=activating`,
//!    `SubState=start`, the target ELF's own start marker, an absent control
//!    socket, an empty `ActiveEnterTimestamp`, `MainPID>0` and a live
//!    `systemctl start` child were all observed.
//! 3. A *separate* `echo-deploy systemd-recover` process (not the test's own
//!    library call) then reads the durable record and rolls back: the previous
//!    release is restored as a real running process, the selector moves back,
//!    the transaction ends `rolled_back` with error `interrupted`, and the
//!    maintenance gate is released.
//! 4. The durable evidence is checked directly: the transaction document, the
//!    immutable previous-release sidecar, and the pinned systemd snapshot +
//!    evidence (full config hash, adapter hash, snapshot digest) must all exist
//!    and agree *before* recovery; a corrupt/absent one would make recovery
//!    fail closed.
//! 5. Production safety: a random throw-away unit in a temporary directory, a
//!    read-only snapshot of `echo-agent-core.service` /
//!    `echo-agent-panel.service` / `echo-agent-core-update.service` before and
//!    after, and cleanup in `Drop` (stop, reset-failed, remove unit,
//!    daemon-reload, bounded wait for `MainPID=0`).
//!
//! Fixtures, and what they are not
//! -------------------------------
//!
//! * `tests/fixtures/crash_core.rs` is a **fixture**, not Core. It speaks the
//!   `echo-deploy-control/1` `status`/`prepare`/`cancel`/`ready` protocol and is
//!   published as the target release's `bin/echo-agent-core`. It deliberately
//!   stays unready until a gate file appears, which is the observable brake the
//!   test uses. It never calls `prctl(PR_SET_PTRACER)` at all.
//! * The *initial*, pre-installed release reuses the existing, unmodified
//!   `tests/fixtures/core_fixture.rs`. That fixture carries its own documented
//!   fixture-only `PR_SET_PTRACER` opt-in; this test neither adds nor depends on
//!   it: no `/proc/<pid>/exe` hash is taken anywhere in this file, and no
//!   production process's ptrace scope is widened.
//! * `tests/fixtures/crash_adapter.py` is a **trusted test adapter**, not
//!   `echo-deploy runtime-adapter`. It verifies the selector, `release.json`
//!   digest and the fixture Core's acknowledgements, and it never hashes a
//!   running image. It is self-contained (its configuration is embedded) because
//!   the CLI pins the adapter bytes into the activation snapshot and recovery
//!   executes those pinned bytes.
//! * The shipping adapter's `/proc/<pid>/exe` verification is *not* exercised
//!   here; `tests/systemd_live.rs` documents that this host's
//!   `kernel.yama.ptrace_scope=1` denies it for systemd-managed processes, and
//!   `tests/core_live.rs` is where the real Core control protocol is covered.
//!   Nothing in this file is a statement about the shipping Core.
//!
//! Real in this test: `systemctl --user` start/stop/show/kill, real unit
//! lifecycle, `Type=notify` blocking start, real MainPIDs, the real
//! `echo-deploy` CLI (`systemd-apply` + independent `systemd-recover`), the
//! production `TransactionStore` / `SystemdTransactionBackend` state machine,
//! the durable selector, and SHA-256 digests of the actual fixture ELF bytes
//! that execute.
//!
//! No production configuration, state, credential, API or unit is modified.
//! The only files outside the temporary directory are the one unique unit file
//! under the XDG user unit directory (removed by `Drop`) and systemd's own
//! journal.

use anyhow::{bail, ensure, Context, Result};
use echo_deploy::systemd_activation::systemd_config_sha256;
use echo_deploy::systemd_transaction::SystemdExecution;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{symlink, PermissionsExt},
        net::UnixStream,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Core artifact path inside a release directory (must match `release.json`).
const CORE_ARTIFACT: &str = "bin/echo-agent-core";
/// Real user units that must never be touched by this test.
const PROTECTED_UNITS: [&str; 3] = [
    "echo-agent-core.service",
    "echo-agent-panel.service",
    "echo-agent-core-update.service",
];
const CONTROL_PROTOCOL: &str = "echo-deploy-control/1";

// ---------------------------------------------------------------------------
// small utilities
// ---------------------------------------------------------------------------

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The CLI under test. Cargo builds it for this integration test target.
fn cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_echo-deploy"))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn unique() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}")
}

fn rustc() -> PathBuf {
    if let Some(configured) = env::var_os("RUSTC") {
        return PathBuf::from(configured);
    }
    if let Some(home) = env::var_os("HOME") {
        let candidate = PathBuf::from(home).join(".cargo/bin/rustc");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("rustc")
}

fn xdg_unit_dir() -> PathBuf {
    if let Some(config) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(config).join("systemd/user");
    }
    PathBuf::from(env::var_os("HOME").expect("HOME is required")).join(".config/systemd/user")
}

/// A bounded command result. Output is captured through temporary files, not
/// pipes, so a killed child can never block the test on a full pipe.
struct Captured {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Captured {
    fn render(&self) -> String {
        format!(
            "status={:?}\nstdout={}\nstderr={}",
            self.status.code(),
            self.stdout,
            self.stderr
        )
    }
}

fn read_all(mut file: fs::File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).trim_end().to_string())
}

/// A capture file that is readable *and* writable (`File::create` is
/// write-only, and reading it back after the child exits would fail with
/// `EBADF`).
fn create_capture(path: &Path) -> Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("create capture {}", path.display()))
}

fn kill_group(pid: u32) {
    if pid > 0 {
        // SAFETY: best-effort SIGKILL of the process group whose id equals the
        // pid we spawned with `process_group(0)`. The caller only uses this for
        // its own children; production PIDs are never addressed.
        let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

/// Run one command under a hard deadline in its own process group. On timeout
/// only that group is killed; an independent CLI or `systemctl` client can
/// never leave the test hanging.
fn run_bounded(
    mut command: Command,
    timeout: Duration,
    label: &str,
    stdin: Option<&[u8]>,
) -> Result<Captured> {
    ensure!(!timeout.is_zero(), "deadline must be non-zero");
    let stdout = tempfile::tempfile().context("create stdout capture")?;
    let stderr = tempfile::tempfile().context("create stderr capture")?;
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    command.process_group(0);
    let mut child = command.spawn().with_context(|| format!("spawn {label}"))?;
    let pid = child.id();
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().context("child stdin")?;
        pipe.write_all(input).context("write child stdin")?;
        // Dropping the pipe closes stdin, so the adapter sees EOF.
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().with_context(|| format!("wait {label}"))? {
            break status;
        }
        let now = Instant::now();
        if now >= deadline {
            kill_group(pid);
            let _ = child.wait();
            bail!("{label} timed out after {timeout:?}");
        }
        thread::sleep(Duration::from_millis(10).min(deadline.saturating_duration_since(now)));
    };
    Ok(Captured {
        status,
        stdout: read_all(stdout)?,
        stderr: read_all(stderr)?,
    })
}

fn systemctl(args: &[&str], timeout: Duration) -> Result<Captured> {
    let mut command = Command::new("/usr/bin/systemctl");
    command.arg("--user").args(args);
    run_bounded(
        command,
        timeout,
        &format!("systemctl --user {}", args.join(" ")),
        None,
    )
}

fn user_systemd_available() -> bool {
    if env::var_os("XDG_RUNTIME_DIR").is_none() {
        return false;
    }
    systemctl(
        &["show", "--property=Version", "--value"],
        Duration::from_secs(10),
    )
    .map(|output| output.status.success())
    .unwrap_or(false)
}

fn skip(reason: &str) {
    eprintln!("SKIP: {reason}");
}

/// Read-only snapshot of the protected units, used to prove non-interference.
fn protected_snapshot() -> Result<BTreeMap<String, String>> {
    let mut snapshot = BTreeMap::new();
    for unit in PROTECTED_UNITS {
        let output = systemctl(
            &["show", "--property=ActiveState", "--property=MainPID", unit],
            Duration::from_secs(10),
        )?;
        ensure!(
            output.status.success(),
            "show {unit} failed: {}",
            output.render()
        );
        snapshot.insert(unit.to_string(), output.stdout);
    }
    Ok(snapshot)
}

/// Refuse to run from a Core/Panel service cgroup. The deploy CLI enforces the
/// same rule; failing here with a clear hint is safer than a confusing refusal
/// after provisioning files.
fn protected_cgroup_component() -> Option<String> {
    let text = fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in text.lines() {
        let path = line.splitn(3, ':').nth(2)?;
        for component in path.split('/').filter(|part| !part.is_empty()) {
            if PROTECTED_UNITS.contains(&component)
                || (component.starts_with("echo-agent-")
                    && (component.ends_with("-core.service")
                        || component.ends_with("-panel.service")))
            {
                return Some(component.to_string());
            }
        }
    }
    None
}

fn has_step(record: &Value, step: &str, outcome: &str) -> bool {
    record["steps"].as_array().is_some_and(|steps| {
        steps
            .iter()
            .any(|entry| entry["step"] == json!(step) && entry["outcome"] == json!(outcome))
    })
}

fn parse_show(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// live fixture
// ---------------------------------------------------------------------------

struct Live {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    instance: String,
    unit: String,
    installed_unit: PathBuf,
    execution: PathBuf,
    adapter: PathBuf,
    installed: bool,
}

impl Live {
    fn new(label: &str) -> Result<Self> {
        let tmp = tempfile::tempdir().context("create temporary root")?;
        let base = tmp.path().to_path_buf();
        let instance = format!("echo-deploy-test-{}-{}", std::process::id(), unique());
        let unit = format!("echo-agent-{instance}-core.service");
        let mut live = Self {
            installed_unit: xdg_unit_dir().join(&unit),
            execution: base.join("execution.json"),
            adapter: base.join("adapter/echo-deploy-adapter"),
            _tmp: tmp,
            base,
            instance,
            unit,
            installed: false,
        };
        eprintln!(
            "[{label}] instance={} unit={} base={}",
            live.instance,
            live.unit,
            live.base.display()
        );
        for (name, mode) in [
            ("store", 0o700u32),
            ("state", 0o700),
            ("units", 0o755),
            ("run", 0o700),
            ("adapter-state", 0o700),
            ("adapter", 0o755),
            ("build", 0o755),
        ] {
            let path = live.base.join(name);
            fs::create_dir(&path).with_context(|| format!("create {}", path.display()))?;
            fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        }
        live.write_adapter()?;
        live.write_execution()?;
        live.write_store_markers()?;
        live.publish_releases()?;
        live.write_unit()?;
        live.install_unit()?;
        Ok(live)
    }

    fn store(&self) -> PathBuf {
        self.base.join("store")
    }
    fn state(&self) -> PathBuf {
        self.base.join("state")
    }
    fn selector(&self) -> PathBuf {
        self.base.join("state/current")
    }
    fn units(&self) -> PathBuf {
        self.base.join("units")
    }
    fn socket(&self) -> PathBuf {
        self.base.join("run/core.sock")
    }
    /// Written by the target release's ELF before it waits for the ready gate.
    /// ExecStopPost does not remove it, so it survives recovery.
    fn started_marker(&self) -> PathBuf {
        self.base.join("run/target-started")
    }
    /// The ready gate is never created before the kill; the fixture Core can
    /// therefore never become Ready by accident.
    fn ready_gate(&self) -> PathBuf {
        self.base.join("run/ready-gate")
    }
    fn release_dir(&self, release: &str) -> PathBuf {
        self.store().join(release)
    }
    fn adapter_state(&self) -> PathBuf {
        self.base.join("adapter-state/adapter-state.json")
    }
    fn adapter_journal(&self) -> PathBuf {
        self.base.join("adapter-state/calls.jsonl")
    }

    fn manifest_sha(&self, release: &str) -> Result<String> {
        let bytes = fs::read(self.release_dir(release).join("release.json"))?;
        Ok(sha256(&bytes))
    }

    // -- provisioning -------------------------------------------------------

    /// Write the trusted *test* adapter with its configuration embedded.
    ///
    /// Embedding matters: `systemd-apply` copies these bytes into the activation
    /// snapshot and `systemd-recover` executes that pinned copy. A sibling
    /// configuration file would not exist next to the pinned copy.
    fn write_adapter(&self) -> Result<()> {
        let template = fs::read_to_string(crate_dir().join("tests/fixtures/crash_adapter.py"))
            .context("read crash_adapter.py fixture")?;
        let document = json!({
            "instance": self.instance,
            "socket": self.socket(),
            "selector": self.selector(),
            "store": self.store(),
            "state": self.adapter_state(),
            "journal": self.adapter_journal(),
            "unit": self.unit,
        });
        let text = serde_json::to_string(&document)?;
        ensure!(
            !text.contains("'''"),
            "embedded adapter config must not contain a triple quote"
        );
        let script = template.replace("__CRASH_ADAPTER_CONFIG_JSON__", &text);
        ensure!(
            script != template,
            "crash_adapter.py fixture lacks its config placeholder"
        );
        fs::write(&self.adapter, script).context("write trusted test adapter")?;
        fs::set_permissions(&self.adapter, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    fn write_execution(&self) -> Result<()> {
        let document = json!({
            "deployment": {
                "schema_version": 1,
                "instance_id": self.instance,
                "topology": "core_only",
                "scope": "user",
                "target_os": "linux",
                "target_arch": env::consts::ARCH,
                "store": self.store(),
                "required_modules": [],
            },
            "state_dir": self.state(),
            "unit_directory": self.units(),
            "current_selector": self.selector(),
            "core_unit": self.unit,
            "panel_unit": null,
            "control_adapter": self.adapter,
            "timeout_seconds": 30,
            "reversible_data": true,
        });
        fs::write(&self.execution, serde_json::to_vec_pretty(&document)?)?;
        fs::set_permissions(&self.execution, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    fn write_store_markers(&self) -> Result<()> {
        fs::write(
            self.store().join("store-format"),
            b"echo-deploy-candidates-v1\n",
        )?;
        fs::write(self.store().join("store-lock"), b"")?;
        for name in ["store-format", "store-lock"] {
            fs::set_permissions(self.store().join(name), fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    fn compile_fixture(&self, name: &str, source: &str) -> Result<PathBuf> {
        let output_path = self.base.join("build").join(name);
        let mut command = Command::new(rustc());
        command
            .args(["--edition", "2021", "-O", "-C", "debuginfo=0", "-o"])
            .arg(&output_path)
            .arg(crate_dir().join("tests/fixtures").join(source));
        let output = run_bounded(
            command,
            Duration::from_secs(120),
            &format!("rustc {source}"),
            None,
        )?;
        ensure!(
            output.status.success(),
            "rustc {source} failed: {}",
            output.render()
        );
        Ok(output_path)
    }

    fn publish(&self, release: &str, core: &[u8]) -> Result<()> {
        let directory = self.release_dir(release);
        fs::create_dir(&directory).with_context(|| format!("create {}", directory.display()))?;
        // Keep distinct releases at distinct digests while remaining a loadable
        // ELF (trailing bytes after the last section are ignored by the loader).
        let mut bytes = core.to_vec();
        bytes
            .extend_from_slice(format!("\n#echo-deploy-crash-test release {release}\n").as_bytes());
        let core_sha = sha256(&bytes);
        let target = directory.join(CORE_ARTIFACT);
        fs::create_dir_all(
            target
                .parent()
                .context("core artifact path has no parent")?,
        )?;
        fs::write(&target, &bytes)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        let manifest = json!({
            "schema_version": 1,
            "release_id": release,
            "target_os": "linux",
            "target_arch": env::consts::ARCH,
            "management_protocol": "management-contract-1",
            "development": true,
            "sources": [{
                "id": "fixture",
                "revision": "systemd-crash-live-fixture",
                "snapshot_sha256": "a".repeat(64),
                "lock_sha256": "b".repeat(64),
            }],
            "artifacts": [{
                "id": "core",
                "kind": "core",
                "path": CORE_ARTIFACT,
                "sha256": core_sha,
                "size": bytes.len(),
                "executable": true,
                "source_id": "fixture",
            }],
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        fs::write(directory.join("release.json"), &manifest_bytes)?;
        fs::set_permissions(
            directory.join("release.json"),
            fs::Permissions::from_mode(0o644),
        )?;
        eprintln!(
            "release {release}: core_sha={core_sha} manifest_sha={} size={}",
            sha256(&manifest_bytes),
            bytes.len()
        );
        Ok(())
    }

    /// `fixture-r1` is the existing, unmodified `core_fixture.rs` (ready).
    /// `fixture-r2-crash` is the slow-start `crash_core.rs` (never ready before
    /// the test's kill).
    fn publish_releases(&self) -> Result<()> {
        let good = self.compile_fixture("core-ready", "core_fixture.rs")?;
        let crash = self.compile_fixture("core-crash-gated", "crash_core.rs")?;
        self.publish("fixture-r1", &fs::read(&good)?)?;
        self.publish("fixture-r2-crash", &fs::read(&crash)?)?;
        Ok(())
    }

    fn unit_body(&self) -> String {
        format!(
            "# Generated by tools/echo-deploy/tests/systemd_crash_live.rs — NOT a production unit.\n\
             # instance: {instance}\n\
             [Unit]\n\
             Description=echo-deploy crash-recovery fixture ({instance})\n\
             \n\
             [Service]\n\
             Type=notify\n\
             NotifyAccess=main\n\
             TimeoutStartSec=120\n\
             TimeoutStopSec=20\n\
             ExecStart={state}/current/{core} --socket {socket} --started {started} --ready-gate {gate}\n\
             ExecStopPost=/usr/bin/rm -f {socket}\n\
             Restart=no\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            instance = self.instance,
            state = self.state().display(),
            core = CORE_ARTIFACT,
            socket = self.socket().display(),
            started = self.started_marker().display(),
            gate = self.ready_gate().display(),
        )
    }

    fn write_unit(&self) -> Result<()> {
        fs::write(self.units().join(&self.unit), self.unit_body())?;
        Ok(())
    }

    fn install_unit(&mut self) -> Result<()> {
        let directory = self
            .installed_unit
            .parent()
            .context("unit path has no parent")?;
        fs::create_dir_all(directory)?;
        fs::copy(self.units().join(&self.unit), &self.installed_unit)?;
        // Record the file even if the reload below fails, so Drop removes it.
        self.installed = true;
        let output = systemctl(&["daemon-reload"], Duration::from_secs(20))?;
        ensure!(
            output.status.success(),
            "daemon-reload failed: {}",
            output.render()
        );
        Ok(())
    }

    // -- observations -------------------------------------------------------

    /// One `systemctl show` for all the state the interruption point needs.
    fn unit_status(&self) -> Result<UnitStatus> {
        let output = systemctl(
            &[
                "show",
                "--property=ActiveState",
                "--property=SubState",
                "--property=MainPID",
                "--property=ActiveEnterTimestamp",
                self.unit.as_str(),
            ],
            Duration::from_secs(15),
        )?;
        ensure!(
            output.status.success(),
            "show unit status failed: {}",
            output.render()
        );
        let values = parse_show(&output.stdout);
        Ok(UnitStatus {
            active_state: values.get("ActiveState").cloned().unwrap_or_default(),
            sub_state: values.get("SubState").cloned().unwrap_or_default(),
            main_pid: values
                .get("MainPID")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            active_enter: values
                .get("ActiveEnterTimestamp")
                .cloned()
                .unwrap_or_default(),
        })
    }

    fn main_pid(&self) -> Result<u32> {
        Ok(self.unit_status()?.main_pid)
    }

    fn selector_target(&self) -> Result<Option<PathBuf>> {
        match fs::read_link(self.selector()) {
            Ok(target) => Ok(Some(target)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read current selector"),
        }
    }

    /// Ask the running fixture Core which release it declares.
    fn core_status(&self) -> Result<Value> {
        use std::io::{BufRead, BufReader};
        let mut stream = UnixStream::connect(self.socket())
            .with_context(|| format!("connect fixture Core {}", self.socket().display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.write_all(b"{\"operation\":\"status\",\"transaction_id\":\"test-assert\"}\n")?;
        stream
            .shutdown(std::net::Shutdown::Write)
            .context("close fixture Core request")?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .context("read fixture Core status")?;
        serde_json::from_str(&line).context("parse fixture Core status")
    }

    fn core_declared_release(&self) -> Result<String> {
        let status = self.core_status()?;
        status["release"]["declared"]
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("fixture Core has no declared release: {status}"))
    }

    /// The durable transaction document, read directly from the state
    /// directory (the same strict JSON `systemd-recover` opens).
    fn transaction_record(&self) -> Result<Value> {
        let path = self
            .state()
            .join(format!("transaction-{}.json", self.instance));
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
    }

    /// The immutable previous-release evidence written before the selector
    /// switch. Exactly one file must exist.
    fn previous_evidence(&self) -> Result<Value> {
        let mut matches = Vec::new();
        for entry in fs::read_dir(self.state())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("previous-") && name.ends_with(".json") {
                matches.push(entry.path());
            }
        }
        ensure!(
            matches.len() == 1,
            "expected exactly one previous-release evidence file, found {matches:?}"
        );
        let bytes = fs::read(&matches[0])?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Read the pinned activation snapshot + the full-config evidence.
    fn snapshot_evidence(&self) -> Result<SnapshotEvidence> {
        let mut evidence_dirs = Vec::new();
        let mut snapshot_dirs = Vec::new();
        for entry in fs::read_dir(self.state())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("systemd-snapshot-") {
                evidence_dirs.push(entry.path());
            } else if name.starts_with("snapshot-") {
                snapshot_dirs.push(entry.path());
            }
        }
        ensure!(
            evidence_dirs.len() == 1 && snapshot_dirs.len() == 1,
            "expected one systemd evidence dir and one activation snapshot, found {evidence_dirs:?} / {snapshot_dirs:?}"
        );
        let config = fs::read(evidence_dirs[0].join("systemd.json"))?;
        let evidence: Value =
            serde_json::from_slice(&fs::read(evidence_dirs[0].join("evidence.json"))?)?;
        let adapter = fs::read(snapshot_dirs[0].join("adapter"))?;
        let snapshot = fs::read(snapshot_dirs[0].join("snapshot.json"))?;
        Ok(SnapshotEvidence {
            meta: evidence,
            config,
            adapter,
            snapshot,
        })
    }

    fn assert_no_snapshot_temps(&self) -> Result<()> {
        for entry in fs::read_dir(self.state())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            ensure!(
                !name.contains("tmp"),
                "abandoned snapshot temporary entry: {name}"
            );
        }
        Ok(())
    }

    fn adapter_script_state(&self) -> Result<Value> {
        match fs::read(self.adapter_state()) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(json!({"maintenance": false, "prepared": null}))
            }
            Err(error) => Err(error).context("read adapter state"),
        }
    }

    fn adapter_journal_has_success(&self, event: &str) -> Result<bool> {
        let text = match fs::read_to_string(self.adapter_journal()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("read adapter journal"),
        };
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value["event"] == json!(event) && value["detail"]["success"] == json!(true) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // -- actions ------------------------------------------------------------

    fn preselect(&self, release: &str) -> Result<()> {
        let target = self.release_dir(release);
        ensure!(target.is_dir(), "release {release} does not exist");
        if self.selector().exists() {
            fs::remove_file(self.selector())?;
        }
        symlink(&target, self.selector()).context("create current selector")?;
        Ok(())
    }

    fn start_unit(&self) -> Result<()> {
        let output = systemctl(&["start", self.unit.as_str()], Duration::from_secs(60))?;
        ensure!(
            output.status.success(),
            "systemctl start {} failed: {}",
            self.unit,
            output.render()
        );
        Ok(())
    }

    /// Simulate a pre-existing installation: pointer + running unit.
    fn preinstall(&self, release: &str) -> Result<()> {
        self.preselect(release)?;
        self.start_unit()?;
        self.wait_ready(release)
    }

    fn wait_ready(&self, release: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut last = String::from("no attempt");
        while Instant::now() < deadline {
            match self.adapter("observe", None, None) {
                Ok(reply) => {
                    if reply["current_release"] == json!(release) && reply["ready"] == json!(true) {
                        return Ok(());
                    }
                    last = format!("{reply}");
                }
                Err(error) => last = format!("{error:#}"),
            }
            thread::sleep(Duration::from_millis(50));
        }
        bail!("instance never became ready for {release}: {last}")
    }

    /// Drive the trusted test adapter with one control request.
    fn adapter(&self, operation: &str, release: Option<&str>, sha: Option<&str>) -> Result<Value> {
        let request = json!({
            "protocol": CONTROL_PROTOCOL,
            "operation": operation,
            "instance": self.instance,
            "release": release,
            "manifest_sha256": sha,
        });
        let output = self.adapter_attempt(&request)?;
        ensure!(
            output.status.success(),
            "test adapter {operation} failed: {}",
            output.render()
        );
        let reply: Value = serde_json::from_str(&output.stdout)
            .with_context(|| format!("test adapter reply is not JSON: {}", output.stdout))?;
        ensure!(
            reply["protocol"] == json!(CONTROL_PROTOCOL),
            "test adapter replied with unexpected protocol: {reply}"
        );
        ensure!(
            reply["success"] == json!(true),
            "test adapter did not acknowledge success: {reply}"
        );
        ensure!(
            reply["instance"] == json!(self.instance),
            "test adapter replied for another instance: {reply}"
        );
        Ok(reply)
    }

    fn adapter_attempt(&self, request: &Value) -> Result<Captured> {
        let payload = serde_json::to_vec(request)?;
        let mut command = Command::new(&self.adapter);
        command.current_dir("/");
        run_bounded(
            command,
            Duration::from_secs(20),
            &format!("test adapter {}", request["operation"]),
            Some(&payload),
        )
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if !self.installed {
            return;
        }
        // Best effort, never panics: leave no unit, no process, no drop-in.
        let _ = systemctl(&["stop", self.unit.as_str()], Duration::from_secs(25));
        let _ = systemctl(
            &["reset-failed", self.unit.as_str()],
            Duration::from_secs(15),
        );
        let _ = fs::remove_file(&self.installed_unit);
        let _ = systemctl(&["daemon-reload"], Duration::from_secs(20));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut residual = self.unit_status().map(|status| status.main_pid);
        while !matches!(residual, Ok(0)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            residual = self.unit_status().map(|status| status.main_pid);
        }
        if !matches!(residual, Ok(0)) {
            // Only this unique unit is addressed. `systemctl kill` cannot touch
            // an unrelated PID.
            let _ = systemctl(
                &[
                    "kill",
                    "--signal=SIGKILL",
                    "--kill-whom=all",
                    self.unit.as_str(),
                ],
                Duration::from_secs(10),
            );
            let _ = systemctl(&["stop", self.unit.as_str()], Duration::from_secs(10));
            residual = self.unit_status().map(|status| status.main_pid);
        }
        eprintln!(
            "[cleanup] unit={} removed={} residual_main_pid={:?}",
            self.unit,
            !self.installed_unit.exists(),
            residual
        );
    }
}

#[derive(Debug)]
struct UnitStatus {
    active_state: String,
    sub_state: String,
    main_pid: u32,
    active_enter: String,
}

struct SnapshotEvidence {
    meta: Value,
    config: Vec<u8>,
    adapter: Vec<u8>,
    snapshot: Vec<u8>,
}

/// The `systemd-apply` process under test, in its own process group so a
/// SIGKILL can be delivered to the CLI and its `systemctl`/adapter children
/// without ever addressing an unrelated (production) PID.
struct ApplyProcess {
    child: Child,
    pgid: i32,
    stdout: fs::File,
    stderr: fs::File,
    reaped: bool,
}

impl ApplyProcess {
    fn spawn(live: &Live, release: &str, transaction: &str) -> Result<Self> {
        let stdout = create_capture(&live.base.join("apply.stdout"))?;
        let stderr = create_capture(&live.base.join("apply.stderr"))?;
        let mut command = Command::new(cli());
        command
            .args(["systemd-apply", "--execution"])
            .arg(&live.execution)
            .args([
                "--release",
                release,
                "--transaction",
                transaction,
                "--allow-development",
                "--confirm",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?))
            .process_group(0);
        let child = command.spawn().context("spawn echo-deploy systemd-apply")?;
        let pgid = i32::try_from(child.id()).context("child pid does not fit i32")?;
        Ok(Self {
            child,
            pgid,
            stdout,
            stderr,
            reaped: false,
        })
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        let status = self.child.try_wait().context("poll systemd-apply")?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    fn logs(&self) -> Result<(String, String)> {
        Ok((
            read_all(self.stdout.try_clone()?)?,
            read_all(self.stderr.try_clone()?)?,
        ))
    }

    fn kill(&mut self) -> Result<ExitStatus> {
        ensure!(!self.reaped, "systemd-apply has already been reaped");
        ensure!(
            self.pgid > 1,
            "refusing to signal process group {}",
            self.pgid
        );
        // SAFETY: `pgid` is the process group of the child we spawned with
        // `process_group(0)`; it cannot be our own group or an unrelated one.
        let result = unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
        ensure!(
            result == 0,
            "kill(-{}) failed: {}",
            self.pgid,
            std::io::Error::last_os_error()
        );
        let status = self.child.wait().context("reap SIGKILLed systemd-apply")?;
        self.reaped = true;
        Ok(status)
    }
}

impl Drop for ApplyProcess {
    fn drop(&mut self) {
        if !self.reaped {
            kill_group(u32::try_from(self.pgid).unwrap_or(0));
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

// ---------------------------------------------------------------------------
// test
// ---------------------------------------------------------------------------

/// Real SIGKILL of `systemd-apply` in the observed pre-commit window (selector
/// already switched, target unit `activating` but not `Ready`), followed by an
/// independent `systemd-recover` that must restore the previous release from
/// durable evidence and release the maintenance gate.
#[test]
#[ignore = "opt-in live acceptance: real user systemd, SIGKILL of systemd-apply"]
fn interrupted_upgrade_after_selector_switch_is_recovered_by_independent_cli() -> Result<()> {
    ensure!(
        user_systemd_available(),
        "live acceptance requires a running user systemd manager"
    );
    if let Some(component) = protected_cgroup_component() {
        skip(&format!(
            "running inside {component}; execute under `systemd-run --user --scope`"
        ));
        return Ok(());
    }
    let protected = protected_snapshot()?;
    let live = Live::new("crash-recovery")?;
    live.preinstall("fixture-r1")?;
    ensure!(
        !live.ready_gate().exists(),
        "the ready gate must not exist before the interruption"
    );
    let initial_pid = live.main_pid()?;
    let initial_release = live.core_declared_release()?;
    ensure!(
        initial_release == "fixture-r1",
        "pre-installed unit does not run fixture-r1 (declares {initial_release})"
    );
    ensure!(initial_pid > 0, "pre-installed unit has no MainPID");
    eprintln!("before: pid={initial_pid} declared_release={initial_release}");

    let target = "fixture-r2-crash";
    let transaction = format!("tx-crash-{}", unique());
    let target_manifest = live.manifest_sha(target)?;
    let previous_manifest = live.manifest_sha("fixture-r1")?;
    let mut apply = ApplyProcess::spawn(&live, target, &transaction)?;

    // Wait for an observable pre-commit interruption point. Every condition
    // below is durable or a real systemd/process observation; there is no
    // "sleep and hope" step.
    let deadline = Instant::now() + Duration::from_secs(60);
    let target_pid;
    loop {
        if let Some(status) = apply.try_wait()? {
            let (stdout, stderr) = apply.logs()?;
            bail!(
                "systemd-apply exited before the interruption point: {status:?}\nstdout={stdout}\nstderr={stderr}"
            );
        }
        let status = live.unit_status()?;
        let selector = live.selector_target()?;
        let marker = fs::read_to_string(live.started_marker()).ok();
        let socket = live.socket().exists();
        let observation = format!(
            "selector={selector:?} active={:?} sub={:?} main_pid={} active_enter={:?} started={:?} socket={socket}",
            status.active_state, status.sub_state, status.main_pid, status.active_enter, marker
        );
        if selector.as_deref() == Some(live.release_dir(target).as_path())
            && status.active_state == "activating"
            && status.sub_state == "start"
            && status.main_pid > 0
            && status.active_enter.is_empty()
            && marker.is_some()
            && !socket
        {
            target_pid = status.main_pid;
            break;
        }
        if Instant::now() >= deadline {
            bail!("interruption point was never observed within 60s: {observation}");
        }
        thread::sleep(Duration::from_millis(40));
    }

    // The ELF that wrote the start marker must be the unit's MainPID, and the
    // apply process must still have a live `systemctl start <unit>` child. That
    // is what makes "target is activating but not Ready" a fact, not a guess.
    let marker = fs::read_to_string(live.started_marker())?;
    ensure!(
        marker.contains(&format!("pid={target_pid} ")),
        "start marker does not belong to MainPID {target_pid}: {marker}"
    );
    ensure!(
        marker.contains(&format!("declared={target} ")),
        "start marker does not declare the target release: {marker}"
    );
    let children = process_children(apply.child.id());
    ensure!(
        children.iter().any(|(_, command)| {
            command.contains("systemctl")
                && command.contains("start")
                && command.contains(&live.unit)
        }),
        "systemd-apply has no live `systemctl start` child: {children:?}"
    );
    let crash_pid = target_pid;
    eprintln!(
        "interruption point: selector={:?} active=activating sub=start main_pid={crash_pid} \
         started={:?} socket_absent=true apply_children={:?}",
        live.selector_target()?,
        fs::read_to_string(live.started_marker())?,
        children
    );

    // Kill only the apply process group. Production PIDs are not addressed.
    ensure!(
        apply.try_wait()?.is_none(),
        "systemd-apply exited between the observation and the kill"
    );
    let status = apply.kill()?;
    ensure!(
        status.signal() == Some(libc::SIGKILL),
        "systemd-apply was not killed by SIGKILL: {status:?}"
    );
    let (apply_stdout, apply_stderr) = apply.logs()?;
    ensure!(
        !apply_stdout.contains("\"phase\""),
        "SIGKILLed systemd-apply printed a transaction record: {apply_stdout}"
    );
    eprintln!(
        "SIGKILL delivered: status={status:?} stdout={apply_stdout:?} stderr={apply_stderr:?}"
    );

    ensure!(
        live.selector_target()? == Some(live.release_dir(target)),
        "the durable selector did not remain on the target release after the crash"
    );

    // Durable evidence *before* recovery: the transaction must be in the
    // pre-commit Verifying phase with no error, and the old-release sidecar must
    // already exist.
    let record = live.transaction_record()?;
    ensure!(record["id"] == json!(transaction), "{record}");
    ensure!(record["phase"] == json!("verifying"), "{record}");
    ensure!(
        record["error"].is_null(),
        "crashed record has an error: {record}"
    );
    ensure!(
        record["previous_release"] == json!("fixture-r1"),
        "{record}"
    );
    ensure!(record["target_release"] == json!(target), "{record}");
    ensure!(
        record["manifest_sha256"] == json!(target_manifest),
        "{record}"
    );
    ensure!(
        has_step(&record, "drain", "succeeded") && has_step(&record, "switch", "succeeded"),
        "crashed record lacks drain/switch evidence: {record}"
    );
    ensure!(
        !has_step(&record, "resume", "failed")
            && !has_step(&record, "verify_readiness", "succeeded"),
        "transaction progressed past the intended interruption point: {record}"
    );
    let evidence = live.previous_evidence()?;
    ensure!(
        evidence["transaction_id"] == json!(transaction),
        "{evidence}"
    );
    ensure!(
        evidence["instance_id"] == json!(live.instance),
        "{evidence}"
    );
    ensure!(
        evidence["previous_release"] == json!("fixture-r1"),
        "{evidence}"
    );
    ensure!(
        evidence["previous_manifest_sha256"] == json!(previous_manifest),
        "previous-release evidence does not pin fixture-r1's exact manifest: {evidence}"
    );
    ensure!(evidence["target_release"] == json!(target), "{evidence}");
    ensure!(
        evidence["target_manifest_sha256"] == json!(target_manifest),
        "{evidence}"
    );
    eprintln!(
        "pre-recovery durable record: phase={} previous={} target={} \
         previous_evidence_sha={}",
        record["phase"], record["previous_release"], record["target_release"], previous_manifest
    );

    // The pinned full-config evidence and adapter bytes must be present and
    // self-consistent; recovery reads these, not the mutable disk.
    let SnapshotEvidence {
        meta: snapshot,
        config: config_bytes,
        adapter: pinned_adapter,
        snapshot: snapshot_json,
    } = live.snapshot_evidence()?;
    let execution: SystemdExecution = serde_json::from_slice(&fs::read(&live.execution)?)?;
    ensure!(
        snapshot["transaction_id"] == json!(transaction)
            && snapshot["instance_id"] == json!(live.instance)
            && snapshot["release_id"] == json!(target)
            && snapshot["manifest_sha256"] == json!(target_manifest),
        "{snapshot}"
    );
    ensure!(
        snapshot["systemd_config_sha256"] == json!(systemd_config_sha256(&execution)?),
        "full systemd config hash differs from the pinned document: {snapshot}"
    );
    ensure!(
        snapshot["config_file_sha256"] == json!(sha256(&config_bytes)),
        "pinned systemd.json bytes do not match their recorded hash: {snapshot}"
    );
    ensure!(
        snapshot["adapter_sha256"] == json!(sha256(&fs::read(&live.adapter)?)),
        "pinned adapter hash differs from the trusted test adapter: {snapshot}"
    );
    ensure!(
        snapshot["snapshot_digest"] == json!(sha256(&snapshot_json)),
        "snapshot.json bytes do not match the recorded snapshot digest: {snapshot}"
    );
    let frozen: Value = serde_json::from_slice(&config_bytes)?;
    ensure!(
        frozen == serde_json::to_value(&execution)?,
        "pinned systemd.json is not the execution document"
    );
    ensure!(
        pinned_adapter == fs::read(&live.adapter)?,
        "pinned adapter bytes differ from the trusted adapter"
    );
    live.assert_no_snapshot_temps()?;
    eprintln!(
        "pinned evidence: systemd_config_sha={} adapter_sha={} snapshot_digest={}",
        snapshot["systemd_config_sha256"], snapshot["adapter_sha256"], snapshot["snapshot_digest"]
    );

    // The adapter journal is the call-level proof that the maintenance gate was
    // really entered, never released, and released only by recovery.
    ensure!(
        live.adapter_journal_has_success("prepare_upgrade")?,
        "adapter journal has no successful prepare_upgrade"
    );
    ensure!(
        live.adapter_journal_has_success("maintenance_enter")?,
        "adapter journal has no successful maintenance_enter"
    );
    ensure!(
        !live.adapter_journal_has_success("maintenance_leave")?,
        "maintenance leave happened before the crash"
    );

    // Observe the crashed unit without asserting a transient post-kill state;
    // the durable record above is the authority.
    let after_kill = live.unit_status()?;
    eprintln!(
        "after SIGKILL: active={} sub={} main_pid={} selector={:?}",
        after_kill.active_state,
        after_kill.sub_state,
        after_kill.main_pid,
        live.selector_target()?
    );

    // Independent CLI recovery: a fresh process, no in-test library call.
    let recovered = run_bounded(
        {
            let mut command = Command::new(cli());
            command
                .args(["systemd-recover", "--execution"])
                .arg(&live.execution)
                .arg("--confirm");
            command
        },
        Duration::from_secs(120),
        "echo-deploy systemd-recover",
        None,
    )?;
    ensure!(
        recovered.status.success(),
        "systemd-recover failed: {}",
        recovered.render()
    );
    let recovered_record: Value = serde_json::from_str(&recovered.stdout)
        .with_context(|| format!("systemd-recover printed no record: {}", recovered.stdout))?;
    ensure!(
        recovered_record["phase"] == json!("rolled_back"),
        "recovery did not reach rolled_back: {recovered_record}"
    );
    ensure!(
        recovered_record["error"]["code"] == json!("interrupted"),
        "recovery did not record an interrupted pre-commit failure: {recovered_record}"
    );
    ensure!(
        recovered_record["error"]["step"] == json!("interrupted"),
        "{recovered_record}"
    );
    ensure!(
        recovered_record["previous_release"] == json!("fixture-r1")
            && recovered_record["target_release"] == json!(target)
            && recovered_record["manifest_sha256"] == json!(target_manifest),
        "{recovered_record}"
    );
    for (step, outcome) in [
        ("interrupted", "failed"),
        ("restore", "succeeded"),
        ("restore_readiness", "succeeded"),
        ("release_maintenance", "succeeded"),
        ("rollback_verify", "succeeded"),
        ("recover", "succeeded"),
    ] {
        ensure!(
            has_step(&recovered_record, step, outcome),
            "recovered record lacks step {step}/{outcome}: {recovered_record}"
        );
    }
    // The JSON printed by the independent CLI must be the durable document.
    let recovered_on_disk = live.transaction_record()?;
    ensure!(
        recovered_on_disk == recovered_record,
        "systemd-recover stdout differs from the durable record:\nstdout={recovered_record}\ndisk={recovered_on_disk}"
    );
    eprintln!(
        "recovery record (durable): phase={} error={} steps={}",
        recovered_record["phase"],
        recovered_record["error"],
        recovered_record["steps"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    );

    // Old release really restored: selector, MainPID, declared release and the
    // read-only adapter observation all agree, and the maintenance gate is open.
    ensure!(
        live.selector_target()? == Some(live.release_dir("fixture-r1")),
        "selector was not restored to fixture-r1"
    );
    let post = live.unit_status()?;
    ensure!(
        post.active_state == "active",
        "unit is not active after recovery: {post:?}"
    );
    ensure!(
        post.main_pid > 0 && post.main_pid != crash_pid,
        "recovered MainPID {} is still the crashed target PID {crash_pid}",
        post.main_pid
    );
    let restored_release = live.core_declared_release()?;
    ensure!(
        restored_release == "fixture-r1",
        "rollback did not restore the previous release (declares {restored_release})"
    );
    let observed = live.adapter("observe", None, None)?;
    ensure!(
        observed["current_release"] == json!("fixture-r1")
            && observed["ready"] == json!(true)
            && observed["maintenance"] == json!(false),
        "recovered observation is not a ready, ungated fixture-r1: {observed}"
    );
    let adapter_state = live.adapter_script_state()?;
    ensure!(
        adapter_state["maintenance"] == json!(false),
        "maintenance gate is still held: {adapter_state}"
    );
    ensure!(
        adapter_state["prepared"].is_null(),
        "adapter still holds a prepared transaction: {adapter_state}"
    );
    ensure!(
        live.adapter_journal_has_success("maintenance_leave")?,
        "adapter journal has no successful maintenance_leave"
    );
    // The gate was never opened, so "not Ready" was real, and recovery did not
    // depend on making the target ready.
    ensure!(
        !live.ready_gate().exists(),
        "ready gate was created; the target may have reached Ready"
    );
    let marker_after = fs::read_to_string(live.started_marker())?;
    ensure!(
        marker_after.contains(&format!("pid={crash_pid} ")),
        "target start marker was lost: {marker_after}"
    );
    eprintln!(
        "after recovery: pid={} declared_release={} observe={} adapter_state={} \
         crashed_target_pid={crash_pid}",
        post.main_pid, restored_release, observed, adapter_state
    );

    // A second independent recovery over the terminal record must be a no-op.
    let again = run_bounded(
        {
            let mut command = Command::new(cli());
            command
                .args(["systemd-recover", "--execution"])
                .arg(&live.execution)
                .arg("--confirm");
            command
        },
        Duration::from_secs(120),
        "echo-deploy systemd-recover (terminal)",
        None,
    )?;
    ensure!(
        again.status.success(),
        "second systemd-recover failed: {}",
        again.render()
    );
    let again_record: Value = serde_json::from_str(&again.stdout)?;
    ensure!(
        again_record == recovered_record,
        "second recovery changed the terminal record:\nfirst={recovered_record}\nsecond={again_record}"
    );
    ensure!(
        live.selector_target()? == Some(live.release_dir("fixture-r1")),
        "second recovery moved the selector"
    );
    ensure!(
        live.main_pid()? == post.main_pid,
        "second recovery restarted the restored release"
    );

    ensure!(
        protected_snapshot()? == protected,
        "the protected echo-agent units changed during the test"
    );
    eprintln!("production units unchanged; cleanup follows in Drop");
    Ok(())
}

/// Direct children of `pid` with their NUL-separated cmdline rendered as text.
fn process_children(pid: u32) -> Vec<(u32, String)> {
    let mut children = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return children;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(child_pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{child_pid}/stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 2 || fields[1].parse::<u32>().ok() != Some(pid) {
            continue;
        }
        let command = fs::read(format!("/proc/{child_pid}/cmdline"))
            .map(|bytes| {
                bytes
                    .split(|byte| *byte == 0)
                    .filter(|part| !part.is_empty())
                    .map(|part| String::from_utf8_lossy(part).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        children.push((child_pid, command));
    }
    children
}
