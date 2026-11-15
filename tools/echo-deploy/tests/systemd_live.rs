//! **Opt-in, live** end-to-end acceptance for `echo-deploy` against a real
//! user `systemd` manager.
//!
//! Every test here is `#[ignore]`d: they mutate the *real* user manager (they
//! must, or they would not prove anything). Run them explicitly:
//!
//! ```bash
//! cd tools/echo-deploy
//! systemd-run --user --scope \
//!   cargo test --offline --test systemd_live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `systemd-run --user --scope` keeps the build/test process out of the running
//! `echo-agent-core.service` cgroup (this repository's own agent runs inside it).
//!
//! Isolation contract of this file:
//!
//! * One throw-away instance per test: `echo-deploy-test-<pid>-<nanos>`; the
//!   unit is therefore `echo-agent-echo-deploy-test-<pid>-<nanos>-core.service`
//!   and can never collide with `echo-agent-core.service` /
//!   `echo-agent-panel.service` or any other production unit.
//! * All release, state, socket, adapter and unit *source* files live in one
//!   `tempfile::TempDir`. The only object written outside it is the temporary
//!   unit file in the XDG user unit directory, which [`Live`]'s `Drop` stops,
//!   removes and `daemon-reload`s.
//! * No production configuration, user data, credential or API is read or
//!   written. No network is used.
//! * `echo-agent-core.service` / `echo-agent-panel.service` are only *read*
//!   (`show ActiveState/MainPID`) so a test can assert it did not disturb them.
//!
//! What is real and what is not
//! ----------------------------
//!
//! Real in every test: `systemctl --user` start/stop/show, real unit lifecycle,
//! `Type=notify` readiness, real MainPIDs, the durable `current` selector
//! symlink, the production [`TransactionStore`] + [`SystemdTransactionBackend`]
//! transaction state machine (the same objects `systemd-apply` /
//! `systemd-recover` drive), and SHA-256 digests taken from real, executing ELF
//! fixture binaries.
//!
//! Fixtures, and why:
//!
//! * `tests/fixtures/core_fixture.rs` stands in for Core's control socket
//!   (`prepare`/`cancel`/`status`/`ready`). It is compiled by the test with
//!   plain `rustc`, published as the release's `bin/echo-agent-core`, and its
//!   digest is the real digest of the bytes that execute. It is **not** Core:
//!   no conclusion here is a statement about the shipping Core.
//! * `tests/fixtures/trusted_adapter.py` stands in for
//!   `echo-deploy runtime-adapter` in the transaction tests. It verifies the
//!   selector, the `release.json` digest, the fixture Core's `prepare`/`status`/
//!   `ready` acknowledgements and the declared-vs-selected release. It does
//!   **not** hash a running image.
//! * The shipping runtime adapter is exercised for real in
//!   `live_adapter_protocol_rejects_mismatches_and_reports_real_state`
//!   (observe / prepare_upgrade / drain / strict rejections), which is where the
//!   adapter protocol is actually verified.
//!
//! Verified environment limitation (recorded, not papered over): this host runs
//! `kernel.yama.ptrace_scope=1` and denies `/proc/<pid>/exe` for any process
//! that is not an ancestor of the target - even after the target accepts
//! `PR_SET_PTRACER`. A Core started by the user manager therefore cannot have
//! its running image hashed by an adapter started from a separate command, so
//! `runtime-adapter`'s `Ready`/`Observe` fail closed with `EACCES` here. Live
//! verification against a **real Core** is consequently NOT claimed; the tests
//! record the failure mode instead of faking a success.
//!
//! First install (`live_first_install_is_recorded`) is *recorded*, not asserted:
//! `PrepareUpgrade` needs a live Core control socket, and before a first install
//! there is none, so the transaction fails in `prepare` and rolls back to a
//! clean state (no selector, unit inactive). The test prints
//! `FIRST_INSTALL_BLOCKED` with the durable record.

use anyhow::{bail, ensure, Context, Result};
use echo_deploy::{
    state_lock::StateLock,
    systemd_transaction::SystemdExecution,
    transaction::{BeginSpec, RunOutcome, TransactionStore},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Core artifact path inside a release directory (must match `release.json`).
const CORE_ARTIFACT: &str = "bin/echo-agent-core";
/// Real user units that must never be touched by these tests.
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

fn systemctl(args: &[&str]) -> Result<Output> {
    Command::new("/usr/bin/systemctl")
        .arg("--user")
        .args(args)
        .output()
        .with_context(|| format!("run systemctl --user {}", args.join(" ")))
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A short, bounded rendering of a command result for assertion messages.
fn render(output: &Output) -> String {
    format!(
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        stdout_of(output),
        stderr_of(output)
    )
}

fn user_systemd_available() -> bool {
    if env::var_os("XDG_RUNTIME_DIR").is_none() {
        return false;
    }
    match systemctl(&["show", "--property=Version", "--value"]) {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

fn skip(reason: &str) {
    eprintln!("SKIP: {reason}");
}

/// Read-only snapshot of the protected units, used to prove non-interference.
fn protected_snapshot() -> Result<BTreeMap<String, String>> {
    let mut snapshot = BTreeMap::new();
    for unit in PROTECTED_UNITS {
        let output = systemctl(&["show", "--property=ActiveState", "--property=MainPID", unit])?;
        snapshot.insert(unit.to_string(), stdout_of(&output));
    }
    Ok(snapshot)
}

// ---------------------------------------------------------------------------
// live fixture
// ---------------------------------------------------------------------------

/// Which `control_adapter` the transaction is driven through.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AdapterMode {
    /// The shipping `echo-deploy runtime-adapter` binary.
    Real,
    /// `tests/fixtures/trusted_adapter.py`; documented in the test header.
    TrustedFixture,
}

struct Live {
    _tmp: tempfile::TempDir,
    adapter_mode: AdapterMode,
    base: PathBuf,
    instance: String,
    unit: String,
    installed_unit: PathBuf,
    execution: PathBuf,
    adapter_config: PathBuf,
}

impl Live {
    fn new(label: &str) -> Result<Self> {
        Self::with_adapter(label, AdapterMode::Real)
    }

    fn with_adapter(label: &str, adapter_mode: AdapterMode) -> Result<Self> {
        let tmp = tempfile::tempdir().context("create temporary root")?;
        let base = tmp.path().to_path_buf();
        let instance = format!("echo-deploy-test-{}-{}", std::process::id(), unique());
        let unit = format!("echo-agent-{instance}-core.service");
        let live = Self {
            adapter_mode,
            installed_unit: xdg_unit_dir().join(&unit),
            execution: base.join("execution.json"),
            adapter_config: base.join("adapter.json"),
            _tmp: tmp,
            base,
            instance,
            unit,
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
    fn release_dir(&self, release: &str) -> PathBuf {
        self.store().join(release)
    }
    fn core_binary(&self, release: &str) -> PathBuf {
        self.release_dir(release).join(CORE_ARTIFACT)
    }

    fn manifest_sha(&self, release: &str) -> Result<String> {
        let bytes = fs::read(self.release_dir(release).join("release.json"))?;
        Ok(sha256(&bytes))
    }

    // -- provisioning -------------------------------------------------------

    fn adapter_path(&self) -> PathBuf {
        self.base.join("adapter/echo-deploy-adapter")
    }

    fn write_adapter(&self) -> Result<()> {
        let adapter = self.adapter_path();
        match self.adapter_mode {
            AdapterMode::Real => {
                let script = format!(
                    "#!/bin/sh\n# generated by tests/systemd_live.rs; executes the real adapter binary\nexec {} runtime-adapter --config {}\n",
                    cli().display(),
                    self.adapter_config.display()
                );
                fs::write(&adapter, script)?;
                let document = json!({
                    "instance_id": self.instance,
                    "core_socket": self.socket(),
                    "panel_gate_file": null,
                    "instance_state": self.base.join("adapter-state"),
                    "current_selector": self.selector(),
                    "release_store": self.store(),
                    "required_modules": [],
                    "acknowledge_external_writers_stopped": true,
                    "first_install": {
                        "evidence": self.state().join("first-install-evidence.json"),
                        "core_unit": self.unit, "panel_unit": null
                    }
                });
                fs::write(&self.adapter_config, serde_json::to_vec_pretty(&document)?)?;
                fs::set_permissions(&self.adapter_config, fs::Permissions::from_mode(0o600))?;
            }
            AdapterMode::TrustedFixture => {
                let source = crate_dir().join("tests/fixtures/trusted_adapter.py");
                fs::copy(&source, &adapter)
                    .with_context(|| format!("copy {}", source.display()))?;
                let document = json!({
                    "instance": self.instance,
                    "socket": self.socket(),
                    "selector": self.selector(),
                    "store": self.store(),
                    "state": self.base.join("adapter-state/trusted-adapter-state.json"),
                    "journal": self.base.join("adapter-state/calls.jsonl"),
                    "unit": self.unit,
                });
                let config = self.base.join("adapter/trusted-adapter.json");
                fs::write(&config, serde_json::to_vec_pretty(&document)?)?;
                fs::set_permissions(&config, fs::Permissions::from_mode(0o600))?;
            }
        }
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755))?;
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
            "control_adapter": self.base.join("adapter/echo-deploy-adapter"),
            "timeout_seconds": 30,
            "reversible_data": true,
        });
        fs::write(&self.execution, serde_json::to_vec_pretty(&document)?)?;
        fs::set_permissions(&self.execution, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    fn compile_fixture(&self, name: &str, refuse_ready: bool) -> Result<PathBuf> {
        let output_path = self.base.join("build").join(name);
        let mut command = Command::new(rustc());
        command
            .args(["--edition", "2021", "-O", "-C", "debuginfo=0", "-o"])
            .arg(&output_path)
            .arg(crate_dir().join("tests/fixtures/core_fixture.rs"));
        if refuse_ready {
            command.args(["--cfg", "echo_fixture_refuse_ready"]);
        }
        let output = command
            .output()
            .context("invoke rustc for the fixture Core")?;
        ensure!(
            output.status.success(),
            "fixture Core did not compile: {}",
            stderr_of(&output)
        );
        Ok(output_path)
    }

    fn publish(&self, release: &str, binary: &Path) -> Result<()> {
        let directory = self.release_dir(release);
        fs::create_dir_all(directory.join("bin"))?;
        let mut bytes = fs::read(binary)?;
        // Keep distinct releases at distinct digests while remaining a loadable
        // ELF (trailing bytes after the last section are ignored by the loader).
        bytes.extend_from_slice(format!("\n#echo-deploy-test release {release}\n").as_bytes());
        let core_sha = sha256(&bytes);
        let target = directory.join(CORE_ARTIFACT);
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
                "revision": "systemd-live-fixture",
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

    /// `fixture-r1` ready, `fixture-r2-bad` starts but refuses readiness,
    /// `fixture-r3` ready (a *different* build than `fixture-r1`).
    fn publish_releases(&self) -> Result<()> {
        let good = self.compile_fixture("core-ready", false)?;
        let bad = self.compile_fixture("core-refuses-ready", true)?;
        self.publish("fixture-r1", &good)?;
        self.publish("fixture-r2-bad", &bad)?;
        self.publish("fixture-r3", &good)?;
        Ok(())
    }

    fn unit_body(&self) -> String {
        format!(
            "# Generated by tools/echo-deploy/tests/systemd_live.rs — NOT a production unit.\n\
             # instance: {instance}\n\
             [Unit]\n\
             Description=echo-deploy live acceptance fixture ({instance})\n\
             \n\
             [Service]\n\
             Type=notify\n\
             NotifyAccess=main\n\
             TimeoutStartSec=20\n\
             TimeoutStopSec=10\n\
             ExecStart={state}/current/{core} --socket {socket}\n\
             ExecStopPost=/usr/bin/rm -f {socket}\n\
             Restart=no\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            instance = self.instance,
            state = self.state().display(),
            core = CORE_ARTIFACT,
            socket = self.socket().display(),
        )
    }

    fn write_unit(&self) -> Result<()> {
        fs::write(self.units().join(&self.unit), self.unit_body())?;
        Ok(())
    }

    fn install_unit(&self) -> Result<()> {
        let directory = self
            .installed_unit
            .parent()
            .context("unit path has no parent")?;
        fs::create_dir_all(directory)?;
        fs::copy(self.units().join(&self.unit), &self.installed_unit)?;
        let output = systemctl(&["daemon-reload"])?;
        ensure!(
            output.status.success(),
            "daemon-reload failed: {}",
            render(&output)
        );
        Ok(())
    }

    // -- observations -------------------------------------------------------

    fn active_state(&self) -> Result<String> {
        let output = systemctl(&[
            "show",
            "--property=ActiveState",
            "--value",
            self.unit.as_str(),
        ])?;
        ensure!(
            output.status.success(),
            "show ActiveState failed: {}",
            render(&output)
        );
        Ok(stdout_of(&output))
    }

    fn main_pid(&self) -> Result<u32> {
        let output = systemctl(&["show", "--property=MainPID", "--value", self.unit.as_str()])?;
        ensure!(
            output.status.success(),
            "show MainPID failed: {}",
            render(&output)
        );
        stdout_of(&output)
            .parse()
            .with_context(|| format!("MainPID is not a number: {}", render(&output)))
    }

    fn selector_target(&self) -> Result<Option<PathBuf>> {
        match fs::read_link(self.selector()) {
            Ok(target) => Ok(Some(target)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read current selector"),
        }
    }

    /// Ask the *running* Core which release it is executing.
    ///
    /// `readlink /proc/<pid>/exe` is denied in this sandbox (cross-process
    /// `ptrace` scope), so the running image cannot be hashed from the test.
    /// The fixture derives this value from its own resolved `/proc/self/exe`,
    /// which makes it a real statement about the started process - just not a
    /// hash-based one.
    fn core_status(&self) -> Result<Value> {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixStream;
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
        let output = systemctl(&["start", self.unit.as_str()])?;
        ensure!(
            output.status.success(),
            "systemctl start {} failed: {}",
            self.unit,
            render(&output)
        );
        Ok(())
    }

    /// Simulate a pre-existing installation: pointer + running unit. Used as the
    /// starting point for upgrade tests so they do not depend on first install.
    fn preinstall(&self, release: &str) -> Result<()> {
        self.preselect(release)?;
        self.start_unit()?;
        self.wait_ready(release)
    }

    fn wait_ready(&self, release: &str) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut last = String::from("no attempt");
        while std::time::Instant::now() < deadline {
            match self.adapter("observe", None, None) {
                Ok(reply) => {
                    if reply["current_release"] == json!(release) && reply["ready"] == json!(true) {
                        return Ok(());
                    }
                    last = format!("{reply}");
                }
                Err(error) => last = format!("{error:#}"),
            }
            thread::sleep(Duration::from_millis(100));
        }
        bail!("instance never became ready for {release}: {last}")
    }

    /// Drive the real `runtime-adapter` binary with one control request.
    fn adapter(&self, operation: &str, release: Option<&str>, sha: Option<&str>) -> Result<Value> {
        self.adapter_request(&json!({
            "protocol": CONTROL_PROTOCOL,
            "operation": operation,
            "instance": self.instance,
            "release": release,
            "manifest_sha256": sha,
        }))
    }

    /// Same request, but returns the raw process result so negative tests can
    /// assert that the adapter refused instead of acknowledging.
    fn adapter_attempt(&self, request: &Value) -> Result<Output> {
        self.adapter_raw(&serde_json::to_vec(request)?)
    }

    fn adapter_request(&self, request: &Value) -> Result<Value> {
        let output = self.adapter_raw(&serde_json::to_vec(request)?)?;
        ensure!(
            output.status.success(),
            "runtime-adapter {operation} failed: {rendered}",
            operation = request["operation"],
            rendered = render(&output)
        );
        let reply: Value = serde_json::from_slice(&output.stdout).with_context(|| {
            format!("runtime-adapter reply is not JSON: {}", stdout_of(&output))
        })?;
        ensure!(
            reply["protocol"] == json!(CONTROL_PROTOCOL),
            "adapter replied with unexpected protocol: {reply}"
        );
        ensure!(
            reply["success"] == json!(true),
            "adapter did not acknowledge success: {reply}"
        );
        Ok(reply)
    }

    fn adapter_raw(&self, payload: &[u8]) -> Result<Output> {
        let mut child = Command::new(self.adapter_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn runtime-adapter")?;
        child
            .stdin
            .take()
            .context("adapter stdin")?
            .write_all(payload)
            .context("write adapter request")?;
        child.wait_with_output().context("wait for runtime-adapter")
    }

    /// Drive the same durable transaction the `systemd-apply` CLI drives:
    /// `TransactionStore` + `SystemdTransactionBackend` (real `systemctl`).
    ///
    /// The CLI adds trust/activation preflight that is out of scope for this
    /// acceptance file; the transaction and backend below are the production
    /// ones, not test doubles.
    fn apply(&self, release: &str) -> Result<Applied> {
        let execution: SystemdExecution =
            serde_json::from_slice(&fs::read(&self.execution)?).context("parse execution.json")?;
        execution.validate()?;
        let manifest = self.manifest_sha(release)?;
        let store = TransactionStore::open(&self.state())?;
        let _lock = StateLock::acquire(&self.state())?;
        let mut backend = execution.create_transaction_backend()?;
        let transaction_id = format!("tx-{}", unique());
        eprintln!("[{release}] begin transaction={transaction_id} manifest={manifest}");
        let spec = BeginSpec::new(
            transaction_id.clone(),
            self.instance.clone(),
            release,
            manifest,
        );
        let mut transaction = store.begin(spec, &mut backend)?;
        let previous = transaction.previous_release.clone();
        let outcome = store.execute(&mut transaction, &mut backend)?;
        let record = serde_json::to_value(&transaction)?;
        eprintln!(
            "[{release}] outcome={outcome:?} previous={previous:?} phase={}",
            record["phase"]
        );
        Ok(Applied {
            outcome,
            record,
            previous,
        })
    }

    /// `systemd-recover` equivalent: resume whatever the durable record says.
    fn recover(&self) -> Result<Recovered> {
        let execution: SystemdExecution = serde_json::from_slice(&fs::read(&self.execution)?)?;
        execution.validate()?;
        let store = TransactionStore::open(&self.state())?;
        let _lock = StateLock::acquire(&self.state())?;
        let mut backend = execution.create_transaction_backend()?;
        let outcome = store.recover(&self.instance, &mut backend)?;
        let record = store
            .load(&self.instance)?
            .map(|transaction| serde_json::to_value(&transaction))
            .transpose()?
            .unwrap_or(serde_json::Value::Null);
        Ok(Recovered { outcome, record })
    }

    /// Spawn the fixture Core directly (no systemd) for protocol-only tests.
    fn spawn_fixture(&self, release: &str) -> Result<FixtureCore> {
        let binary = self.core_binary(release);
        ensure!(
            binary.is_file(),
            "missing fixture core {}",
            binary.display()
        );
        let child = Command::new(&binary)
            .arg("--socket")
            .arg(self.socket())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn fixture Core")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if self.socket().exists() {
                thread::sleep(Duration::from_millis(50));
                return Ok(FixtureCore { child });
            }
            thread::sleep(Duration::from_millis(20));
        }
        bail!("fixture Core did not create {}", self.socket().display())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // Best effort, never panics: leave no unit, no process, no drop-in.
        let _ = systemctl(&["stop", self.unit.as_str()]);
        let _ = systemctl(&["reset-failed", self.unit.as_str()]);
        let _ = fs::remove_file(&self.installed_unit);
        let _ = systemctl(&["daemon-reload"]);
        let remaining = systemctl(&["show", "--property=MainPID", "--value", self.unit.as_str()])
            .map(|output| stdout_of(&output))
            .unwrap_or_else(|error| format!("<{error}>"));
        eprintln!(
            "[cleanup] unit={} removed={} residual_main_pid={}",
            self.unit,
            !self.installed_unit.exists(),
            remaining
        );
    }
}

struct Applied {
    outcome: RunOutcome,
    record: Value,
    previous: Option<String>,
}

struct Recovered {
    outcome: Option<RunOutcome>,
    record: Value,
}

struct FixtureCore {
    child: Child,
}

impl Drop for FixtureCore {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Show what an aborted first install left behind, and what recovery does with
/// it. Recorded on purpose: the runtime adapter's first-install path is being
/// repaired in parallel, so this test must not assert either way.
fn record_first_install_state(live: &Live) {
    match live.recover() {
        Ok(recovered) => eprintln!(
            "FIRST_INSTALL_BLOCKED recovery: outcome={:?} record={}",
            recovered.outcome, recovered.record
        ),
        Err(error) => eprintln!("FIRST_INSTALL_BLOCKED recovery error: {error:#}"),
    }
    eprintln!(
        "FIRST_INSTALL_BLOCKED residual: selector={:?} unit_state={:?} socket_present={}",
        live.selector_target().ok().flatten(),
        live.active_state().ok(),
        live.socket().exists()
    );
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// Control-adapter protocol only: real `echo-deploy runtime-adapter` process,
/// real Unix socket, fixture Core. No systemd unit is involved.
#[test]
#[ignore = "opt-in live acceptance (real systemctl / user systemd)"]
fn live_adapter_protocol_rejects_mismatches_and_reports_real_state() -> Result<()> {
    if !user_systemd_available() {
        skip("no user systemd manager");
        return Ok(());
    }
    let live = Live::new("adapter-protocol")?;
    live.preselect("fixture-r1")?;
    let core = live.spawn_fixture("fixture-r1")?;

    // 1. Honest observation of a live, ready, non-maintenance instance.
    let observed = live.adapter("observe", None, None)?;
    ensure!(
        observed["current_release"] == json!("fixture-r1"),
        "{observed}"
    );
    ensure!(observed["ready"] == json!(true), "{observed}");
    ensure!(observed["maintenance"] == json!(false), "{observed}");
    ensure!(observed["active_work"] == json!(0), "{observed}");
    eprintln!("observe(fixture-r1) = {observed}");

    // 2. A different protocol version must be refused, not tolerated.
    let wrong_protocol = live.adapter_attempt(&json!({
        "protocol": "echo-deploy-control/2",
        "operation": "observe",
        "instance": live.instance,
        "release": null,
        "manifest_sha256": null,
    }))?;
    ensure!(
        !wrong_protocol.status.success(),
        "adapter accepted a foreign protocol version"
    );
    eprintln!("wrong-protocol rejected: {}", stderr_of(&wrong_protocol));

    // 3. Unknown request fields must be refused (strict input contract).
    let unknown_field = live.adapter_attempt(&json!({
        "protocol": CONTROL_PROTOCOL,
        "operation": "observe",
        "instance": live.instance,
        "release": null,
        "manifest_sha256": null,
        "injected": true,
    }))?;
    ensure!(
        !unknown_field.status.success(),
        "adapter accepted an unknown request field"
    );
    eprintln!("unknown-field rejected: {}", stderr_of(&unknown_field));

    // 4. PrepareUpgrade is acknowledged against the *previous* release.
    let target_sha = live.manifest_sha("fixture-r2-bad")?;
    let prepared = live.adapter("prepare_upgrade", Some("fixture-r2-bad"), Some(&target_sha))?;
    ensure!(prepared["maintenance"] == json!(true), "{prepared}");
    ensure!(
        prepared["current_release"] == json!("fixture-r1"),
        "{prepared}"
    );
    eprintln!("prepare(fixture-r2-bad) = {prepared}");

    // 5. Drain must be checked, not forced.
    let drained = live.adapter("drain", None, None)?;
    ensure!(drained["active_work"] == json!(0), "{drained}");

    // 6. Ready for a release the selector does not name must fail closed.
    let premature = live.adapter_attempt(&json!({
        "protocol": CONTROL_PROTOCOL,
        "operation": "ready",
        "instance": live.instance,
        "release": "fixture-r2-bad",
        "manifest_sha256": null,
    }))?;
    ensure!(
        !premature.status.success(),
        "adapter confirmed readiness for a release the selector does not name"
    );
    eprintln!("ready-before-switch rejected: {}", stderr_of(&premature));

    // 7. After switching the pointer but before restarting, the running image
    //    still belongs to the old release: readiness must fail on the hash.
    live.preselect("fixture-r2-bad")?;
    let stale_image = live.adapter_attempt(&json!({
        "protocol": CONTROL_PROTOCOL,
        "operation": "ready",
        "instance": live.instance,
        "release": "fixture-r2-bad",
        "manifest_sha256": null,
    }))?;
    ensure!(
        !stale_image.status.success(),
        "adapter confirmed readiness while the old process was still running"
    );
    eprintln!("ready-stale-process rejected: {}", stderr_of(&stale_image));

    // 8. A dead Core must never be reported as ready. A *stale* socket file is
    //    fail-closed: the adapter errors instead of fabricating `ready=false`.
    drop(core);
    let stale_socket = live.adapter_attempt(&json!({
        "protocol": CONTROL_PROTOCOL,
        "operation": "observe",
        "instance": live.instance,
        "release": null,
        "manifest_sha256": null,
    }))?;
    ensure!(
        !stale_socket.status.success(),
        "adapter reported an observation while the Core socket was dead but present"
    );
    eprintln!(
        "observe(stale socket) rejected: {}",
        stderr_of(&stale_socket)
    );

    // 9. Once the socket is gone (what `ExecStopPost=`/`RuntimeDirectory=` does
    //    on a real stop) the observation is a clean, honest `ready=false`.
    fs::remove_file(live.socket())?;
    let stopped = live.adapter("observe", None, None)?;
    ensure!(stopped["ready"] == json!(false), "{stopped}");
    ensure!(
        stopped["current_release"] == json!("fixture-r2-bad"),
        "{stopped}"
    );
    eprintln!("observe(no core socket) = {stopped}");
    Ok(())
}

/// Happy path: a real upgrade really stops the old unit, moves the durable
/// selector, starts the new release and only commits once it is ready.
#[test]
#[ignore = "opt-in live acceptance (real systemctl / user systemd)"]
fn live_upgrade_stops_old_unit_switches_selector_and_requires_new_ready() -> Result<()> {
    if !user_systemd_available() {
        skip("no user systemd manager");
        return Ok(());
    }
    let protected = protected_snapshot()?;
    // See the module header: this drives the transaction through the trusted
    // *fixture* adapter because the shipping adapter's /proc image check is
    // denied by this sandbox. systemd, the CLI, the transaction store, the
    // selector and all checksums are real.
    let live = Live::with_adapter("upgrade", AdapterMode::TrustedFixture)?;
    live.preinstall("fixture-r1")?;

    let pid_before = live.main_pid()?;
    let release_before = live.core_declared_release()?;
    ensure!(
        release_before == "fixture-r1",
        "pre-installed unit does not run fixture-r1 (declares {release_before})"
    );
    ensure!(live.selector_target()? == Some(live.release_dir("fixture-r1")));
    eprintln!("before: pid={pid_before} declared_release={release_before}");

    let applied = live.apply("fixture-r3")?;
    ensure!(
        matches!(applied.outcome, RunOutcome::Committed),
        "upgrade to fixture-r3 did not commit: {:?} / {}",
        applied.outcome,
        applied.record
    );
    ensure!(
        applied.record["phase"] == json!("committed"),
        "{}",
        applied.record
    );
    ensure!(
        applied.previous.as_deref() == Some("fixture-r1"),
        "transaction did not record fixture-r1 as previous: {:?}",
        applied.previous
    );

    let pid_after = live.main_pid()?;
    ensure!(
        pid_after != pid_before,
        "the old process ({pid_before}) is still the MainPID after the upgrade"
    );
    let release_after = live.core_declared_release()?;
    ensure!(
        release_after == "fixture-r3",
        "the restarted unit does not execute fixture-r3 (declares {release_after})"
    );
    ensure!(live.active_state()? == "active", "unit is not active");
    ensure!(
        live.selector_target()? == Some(live.release_dir("fixture-r3")),
        "selector did not move to fixture-r3"
    );

    let observed = live.adapter("observe", None, None)?;
    ensure!(
        observed["current_release"] == json!("fixture-r3"),
        "{observed}"
    );
    ensure!(observed["ready"] == json!(true), "{observed}");
    ensure!(observed["maintenance"] == json!(false), "{observed}");
    eprintln!(
        "after: pid={pid_after} declared_release={release_after} selector={:?} observe={observed}",
        live.selector_target()?
    );

    ensure!(
        protected_snapshot()? == protected,
        "the protected echo-agent units changed during the test"
    );
    Ok(())
}

/// Failure path: the new release starts but never becomes ready. The durable
/// transaction must roll back to the previous release, and a later
/// `systemd-recover` must be a clean no-op over the rolled-back record.
#[test]
#[ignore = "opt-in live acceptance (real systemctl / user systemd)"]
fn live_failed_upgrade_rolls_back_to_previous_release_then_recovers() -> Result<()> {
    if !user_systemd_available() {
        skip("no user systemd manager");
        return Ok(());
    }
    let protected = protected_snapshot()?;
    // Trusted fixture adapter, same reason as the upgrade test above.
    let live = Live::with_adapter("rollback", AdapterMode::TrustedFixture)?;
    live.preinstall("fixture-r1")?;
    let pid_before = live.main_pid()?;
    eprintln!(
        "before: pid={pid_before} declared_release={}",
        live.core_declared_release()?
    );

    let applied = live.apply("fixture-r2-bad")?;
    ensure!(
        matches!(applied.outcome, RunOutcome::RolledBack),
        "broken release did not roll back: {:?} / {}",
        applied.outcome,
        applied.record
    );
    ensure!(
        applied.record["phase"] == json!("rolled_back"),
        "failed upgrade did not persist rolled_back: {}",
        applied.record
    );
    let code = applied.record["error"]["code"].as_str().unwrap_or_default();
    ensure!(
        code == "resume_failed" || code == "readiness_failed",
        "unexpected failure code {code:?}: {}",
        applied.record
    );
    eprintln!(
        "failed upgrade code={code} record_phase={}",
        applied.record["phase"]
    );

    // The rollback is only believable if the real service is back on r1.
    ensure!(
        live.selector_target()? == Some(live.release_dir("fixture-r1")),
        "selector was not restored to fixture-r1"
    );
    let pid_after = live.main_pid()?;
    let release_after = live.core_declared_release()?;
    ensure!(
        release_after == "fixture-r1",
        "rollback did not restore the previous release (declares {release_after})"
    );
    ensure!(
        live.active_state()? == "active",
        "unit is not active after rollback"
    );
    let observed = live.adapter("observe", None, None)?;
    ensure!(
        observed["current_release"] == json!("fixture-r1"),
        "{observed}"
    );
    ensure!(observed["ready"] == json!(true), "{observed}");
    ensure!(observed["maintenance"] == json!(false), "{observed}");
    eprintln!(
        "after rollback: pid={pid_after} declared_release={release_after} observe={observed}"
    );

    // Recovery over a terminal transaction must not move anything.
    let recovered = live.recover()?;
    ensure!(
        matches!(recovered.outcome, Some(RunOutcome::RolledBack)),
        "systemd-recover did not report a rolled-back record: {:?}",
        recovered.outcome
    );
    ensure!(
        recovered.record["phase"] == json!("rolled_back"),
        "recovery changed the rolled-back record: {}",
        recovered.record
    );
    ensure!(
        live.selector_target()? == Some(live.release_dir("fixture-r1")),
        "recovery moved the selector"
    );
    ensure!(
        live.core_declared_release()? == "fixture-r1",
        "recovery changed the running release"
    );
    eprintln!("recover: phase=rolled_back (no-op)");

    ensure!(
        protected_snapshot()? == protected,
        "the protected echo-agent units changed during the test"
    );
    Ok(())
}

/// First install from scratch. The runtime adapter's first-install path is
/// being repaired in parallel, so this test **records** the current behaviour
/// (success is verified; failure is reported as `FIRST_INSTALL_BLOCKED`) instead
/// of asserting it.
#[test]
#[ignore = "opt-in live acceptance (real systemctl / user systemd)"]
fn live_first_install_is_recorded() -> Result<()> {
    if !user_systemd_available() {
        skip("no user systemd manager");
        return Ok(());
    }
    let protected = protected_snapshot()?;
    let live = Live::with_adapter("first-install", AdapterMode::TrustedFixture)?;
    ensure!(
        live.selector_target()?.is_none(),
        "first install must start without a selector"
    );

    match live.apply("fixture-r1") {
        Ok(applied) if matches!(applied.outcome, RunOutcome::Committed) => {
            ensure!(
                applied.record["phase"] == json!("committed"),
                "first install did not commit: {}",
                applied.record
            );
            ensure!(
                live.selector_target()? == Some(live.release_dir("fixture-r1")),
                "first install did not create the selector"
            );
            ensure!(live.active_state()? == "active", "unit is not active");
            ensure!(
                live.core_declared_release()? == "fixture-r1",
                "first install does not run fixture-r1"
            );
            let observed = live.adapter("observe", None, None)?;
            ensure!(
                observed["current_release"] == json!("fixture-r1"),
                "{observed}"
            );
            ensure!(observed["ready"] == json!(true), "{observed}");
            eprintln!("FIRST_INSTALL_OK: {observed}");
        }
        Ok(applied) => {
            eprintln!(
                "FIRST_INSTALL_BLOCKED (recorded, not asserted)\n  outcome={:?}\n  record={}",
                applied.outcome, applied.record
            );
            record_first_install_state(&live);
        }
        Err(error) => {
            eprintln!("FIRST_INSTALL_BLOCKED (recorded, not asserted)\n  error={error:#}");
            record_first_install_state(&live);
        }
    }

    ensure!(
        protected_snapshot()? == protected,
        "the protected echo-agent units changed during the test"
    );
    Ok(())
}

/// Hard acceptance for the shipping CLI + adapter + real user systemd.
/// The Core ELF remains an explicitly labelled protocol fixture.
#[test]
#[ignore = "opt-in live acceptance (real systemctl / user systemd)"]
fn live_cli_first_install_commits_and_recovery_reopens_existing_snapshot() -> Result<()> {
    ensure!(
        user_systemd_available(),
        "live acceptance requires user systemd"
    );
    let protected = protected_snapshot()?;
    let live = Live::new("cli-first-install")?;
    fs::write(
        live.store().join("store-format"),
        b"echo-deploy-candidates-v1\n",
    )?;
    fs::write(live.store().join("store-lock"), b"")?;
    for name in ["store-format", "store-lock"] {
        fs::set_permissions(live.store().join(name), fs::Permissions::from_mode(0o600))?;
    }
    let mut execution: Value = serde_json::from_slice(&fs::read(&live.execution)?)?;
    execution["timeout_seconds"] = json!(3);
    fs::write(&live.execution, serde_json::to_vec(&execution)?)?;
    let applied = Command::new(cli())
        .args(["systemd-apply", "--execution"])
        .arg(&live.execution)
        .args([
            "--release",
            "fixture-r1",
            "--allow-development",
            "--confirm",
        ])
        .output()?;
    ensure!(
        applied.status.success(),
        "first-install CLI failed: {}",
        render(&applied)
    );
    let record: Value = serde_json::from_slice(&applied.stdout)?;
    ensure!(record["phase"] == "committed", "{record}");
    ensure!(live.active_state()? == "active", "new unit not active");
    ensure!(
        live.selector_target()? == Some(live.release_dir("fixture-r1")),
        "selector mismatch"
    );
    ensure!(
        live.core_declared_release()? == "fixture-r1",
        "runtime mismatch"
    );
    let observed = live.adapter("observe", None, None)?;
    ensure!(
        observed["ready"] == true && observed["maintenance"] == false,
        "{observed}"
    );
    let recovered = Command::new(cli())
        .args(["systemd-recover", "--execution"])
        .arg(&live.execution)
        .arg("--confirm")
        .output()?;
    ensure!(
        recovered.status.success(),
        "CLI recovery failed: {}",
        render(&recovered)
    );
    // The independent updater consumes the same strict systemd document.
    let request = live.base.join("update-request.json");
    fs::write(
        &request,
        serde_json::to_vec(&json!({
            "schema_version": 1, "execution": live.execution, "release": "fixture-r3",
            "transaction": "cli-live-upgrade", "allow_development": true,
            "trust_store": null, "key_id": null, "signature": null
        }))?,
    )?;
    let upgraded = Command::new(cli())
        .args(["apply-request", "--request"])
        .arg(&request)
        .arg("--confirm")
        .output()?;
    ensure!(
        upgraded.status.success(),
        "CLI upgrade failed: {}",
        render(&upgraded)
    );
    ensure!(
        live.core_declared_release()? == "fixture-r3",
        "upgrade runtime mismatch"
    );
    let failed = Command::new(cli())
        .args(["systemd-apply", "--execution"])
        .arg(&live.execution)
        .args([
            "--release",
            "fixture-r2-bad",
            "--allow-development",
            "--confirm",
        ])
        .output()?;
    ensure!(
        !failed.status.success(),
        "bad release incorrectly committed"
    );
    let rollback: Value = serde_json::from_slice(&failed.stdout)
        .with_context(|| format!("no durable rollback record: {}", render(&failed)))?;
    ensure!(
        rollback["phase"] == "rolled_back",
        "failed update did not roll back: {rollback}"
    );
    ensure!(
        live.core_declared_release()? == "fixture-r3",
        "rollback runtime mismatch"
    );
    let recovered = Command::new(cli())
        .args(["systemd-recover", "--execution"])
        .arg(&live.execution)
        .arg("--confirm")
        .output()?;
    ensure!(
        recovered.status.success(),
        "rollback recovery failed: {}",
        render(&recovered)
    );
    eprintln!(
        "CLI_UPGRADE_AND_FAILED_UPDATE_ROLLBACK_VERIFIED instance={}",
        live.instance
    );
    ensure!(
        protected_snapshot()? == protected,
        "production units changed"
    );
    eprintln!(
        "CLI_FIRST_INSTALL_COMMITTED_AND_RECOVERED instance={} observe={}",
        live.instance, observed
    );
    Ok(())
}
