//! **Opt-in, live** end-to-end acceptance for a real standalone Core + Panel
//! deployment on a real user `systemd` manager.
//!
//! This file is deliberately not a fixture-Core / fixture-Panel test. It uses
//! the two exact managed debug binaries:
//!
//! * Core  = `/home/evence/.local/share/echo-agent-core/source/target/debug/echo-agent-core`
//! * Panel = `/home/evence/.local/share/echo-agent-panel/source/target/debug/echo-agent-panel`
//!
//! and drives the real `echo-deploy` CLI (`systemd-apply`, the rendered
//! independent updater unit with `apply-request`, and `stage`/`init-store`).
//! The units and launchers are the real text rendered by
//! [`echo_deploy::install::InstallSpec::render`] with an opted-in
//! `InstallSpec.deployment`; the rendered unit files are installed into a
//! private temp unit directory with [`echo_deploy::installation::install_files`]
//! and loaded by linking them into the real per-user unit directory. No
//! existing source file is modified by this test.
//!
//! Run it explicitly, outside the production `echo-agent-core.service` cgroup:
//!
//! ```bash
//! cd tools/echo-deploy
//! systemd-run --user --scope \
//!   --setenv=ECHO_DEPLOY_RUN_STANDALONE_LIVE=1 \
//!   cargo test --locked --test standalone_live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `systemd-run --user --scope` is required because the repository's own agent
//! runs inside `echo-agent-core.service`; the test fails closed with that
//! instruction instead of starting a deployment from a protected cgroup.
//!
//! Isolation contract
//! ------------------
//!
//! * One throw-away instance per test:
//!   `standalone-live-<pid>-<nanos>`; unit names are
//!   `echo-agent-standalone-live-<pid>-<nanos>-{core,panel,update}.service`,
//!   so they can never collide with the production units.
//! * The test root lives under `$HOME/.local/state/echo-deploy-live/` (not
//!   `/tmp`) because the real rendered units set `PrivateTmp=true`; all config,
//!   release, state, socket, launcher and web-stub files live inside it.
//! * The only objects written outside the temp root are symlinks for this
//!   instance's unit files / drop-in dirs in the real per-user unit directory.
//!   They are removed by the cleanup guard, which only removes a link when it
//!   still points at this test's temp tree.
//! * No production configuration, credential, persona, QQ account, federation
//!   secret or user data is read or written. Core uses an empty API key, the
//!   discard provider `127.0.0.1:9`, no teams, QQ disabled and an explicit
//!   `federation.listen = ""`; Panel uses an empty access token / panel key and
//!   a random loopback bind port. All service HOME/XDG/TMPDIR paths point into
//!   the temp root through test-only unit drop-ins (the rendered unit text is
//!   untouched; drop-ins only add the environment the deployment operator owns).
//! * The web frontend is an obvious byte-level r1/r2 `index.html` stub, **not**
//!   the built Vue frontend. `/index` and `/index.html` are asserted by exact
//!   bytes only; this is not a browser/frontend acceptance claim. The same
//!   boundary applies to `/health`: this Panel build has no real health route,
//!   so the probe classifies the SPA fallback instead of pretending it is a
//!   health endpoint.
//!
//! Real recovery / rollback evidence
//! ---------------------------------
//!
//! * First install: `systemd-apply` (real CLI) -> r1.
//! * Upgrade: a frozen `update-request.json` consumed by the rendered,
//!   independent updater unit (`Type=oneshot`, its own cgroup) -> r2.
//! * Bad Panel: an r2-bad release whose declared Panel image is a shell stub
//!   that execs `sleep`; the real systemd backend must reject readiness (the
//!   running `/proc/<pid>/exe` is not the declared Panel image) and roll back
//!   to r2 before returning the durable transaction to `rolled_back`.
//! * Stop order Panel -> Core and start order Core -> Panel are checked against
//!   the real user journal. The durable transaction record is read from
//!   `transaction-<instance>.json`; a failed record is never treated as a pass.
//! * The cleanup guard records the production `echo-agent-core.service` /
//!   `echo-agent-panel.service` / `echo-agent-core-update.service` state before
//!   and after the run and fails if their ActiveState/MainPID changed.
//!
//! Running-process image verification
//! ----------------------------------
//!
//! The transitively exercised `systemd.rs` transport (including its default
//! awaitable/checking interface) hashes `/proc/<MainPID>/exe`. This test also
//! performs its own independent `/proc/<pid>/exe` hash after every successful
//! phase. If the host denies that read, the test fails honestly and prints the
//! permission boundary: it never calls `PR_SET_PTRACER`, never changes host
//! sysctls/policies, and never converts a permission failure into a pass.
//!
//! All assertions are opt-in (`#[ignore]`) and require
//! `ECHO_DEPLOY_RUN_STANDALONE_LIVE=1`. Without the marker the test prints
//! `SKIP` and does nothing.

use anyhow::{bail, ensure, Context, Result};
use echo_deploy::{
    install::{self, DeploymentRuntime, InstallSpec, RuntimeAdapterConfig, INSTALL_SCHEMA_VERSION},
    installation,
    model::{Deployment, Scope, Topology},
    systemd_transaction::SystemdExecution,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    os::unix::{
        fs::{symlink, MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const FIXTURE_MANIFEST: &str = "standalone_live.manifest.json";
const PRIVATE_DIR_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const PUBLIC_DIR_MODE: u32 = 0o755;
const MIN_REAL_CORE_BYTES: u64 = 10 * 1024 * 1024;
const MIN_REAL_PANEL_BYTES: u64 = 10 * 1024 * 1024;
const FIXED_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const CORE_ARTIFACT_PATH: &str = "bin/echo-agent-core";
const PANEL_ARTIFACT_PATH: &str = "bin/echo-agent-panel";
const WEB_ARTIFACT_PATH: &str = "web/index.html";
const PROTECTED_UNITS: [&str; 3] = [
    "echo-agent-core.service",
    "echo-agent-panel.service",
    "echo-agent-core-update.service",
];
const MAX_HTTP_BYTES: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// fixture manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    description: String,
    opt_in_env: String,
    core_root: PathBuf,
    core_binary: PathBuf,
    core_binary_env: String,
    panel_root: PathBuf,
    panel_binary: PathBuf,
    panel_binary_env: String,
    core_template: String,
    panel_template: String,
    r1_index: String,
    r2_index: String,
    bad_panel: String,
    release_r1: String,
    release_r2: String,
    release_r2_bad: String,
    instance_prefix: String,
    management_protocol: String,
    required_modules: Vec<String>,
    forbidden_ports: Vec<u16>,
    max_help_seconds: u64,
    max_startup_seconds: u64,
    max_transaction_seconds: u64,
    max_total_seconds: u64,
    systemd_execution_timeout_seconds: u64,
}

impl Manifest {
    fn help_timeout(&self) -> Duration {
        Duration::from_secs(self.max_help_seconds)
    }
    fn startup_timeout(&self) -> Duration {
        Duration::from_secs(self.max_startup_seconds)
    }
    fn transaction_timeout(&self) -> Duration {
        Duration::from_secs(self.max_transaction_seconds)
    }
    fn total_timeout(&self) -> Duration {
        Duration::from_secs(self.max_total_seconds)
    }
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixtures_dir() -> PathBuf {
    crate_dir().join("tests/fixtures")
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

fn sha256_reader(mut reader: impl Read) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let read = reader.read(&mut buffer).context("hash reader")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    sha256_reader(file).with_context(|| format!("hash {}", path.display()))
}

fn load_manifest() -> Result<(Manifest, PathBuf)> {
    let path = fixtures_dir().join(FIXTURE_MANIFEST);
    ensure!(
        path.is_absolute() && path.is_file(),
        "fixture manifest must be an absolute regular file: {}",
        path.display()
    );
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).context("parse standalone live fixture manifest")?;
    validate_manifest(&manifest, &path)?;
    Ok((manifest, path))
}

fn validate_manifest(manifest: &Manifest, manifest_path: &Path) -> Result<()> {
    ensure!(
        manifest.schema_version == 1,
        "unsupported fixture manifest schema"
    );
    ensure!(
        !manifest.description.is_empty(),
        "fixture description must not be empty"
    );
    ensure!(
        manifest.opt_in_env == "ECHO_DEPLOY_RUN_STANDALONE_LIVE",
        "fixture opt_in_env must be ECHO_DEPLOY_RUN_STANDALONE_LIVE"
    );
    ensure!(
        manifest.core_binary_env == "ECHO_DEPLOY_STANDALONE_CORE_BINARY",
        "fixture core_binary_env mismatch"
    );
    ensure!(
        manifest.panel_binary_env == "ECHO_DEPLOY_STANDALONE_PANEL_BINARY",
        "fixture panel_binary_env mismatch"
    );
    ensure!(
        manifest.core_root.is_absolute()
            && manifest.core_binary.is_absolute()
            && manifest.panel_root.is_absolute()
            && manifest.panel_binary.is_absolute(),
        "roots and binaries must be absolute"
    );
    ensure!(
        manifest.core_binary.starts_with(&manifest.core_root),
        "Core binary must live under the declared Core root"
    );
    ensure!(
        manifest.panel_binary.starts_with(&manifest.panel_root),
        "Panel binary must live under the declared Panel root"
    );
    ensure!(
        manifest.forbidden_ports.contains(&3132) && manifest.forbidden_ports.contains(&3133),
        "forbidden_ports must include the production management/federation ports"
    );
    ensure!(
        (1..=300).contains(&manifest.max_help_seconds)
            && (1..=600).contains(&manifest.max_startup_seconds)
            && (1..=1200).contains(&manifest.max_transaction_seconds)
            && (1..=3600).contains(&manifest.max_total_seconds)
            && (1..=600).contains(&manifest.systemd_execution_timeout_seconds),
        "fixture timeouts are outside the accepted bounds"
    );
    ensure!(
        !manifest.required_modules.is_empty()
            && manifest
                .required_modules
                .iter()
                .all(|module| safe_systemd_id(module)),
        "required_modules must be non-empty safe identifiers"
    );
    for release in [
        &manifest.release_r1,
        &manifest.release_r2,
        &manifest.release_r2_bad,
    ] {
        ensure!(
            safe_systemd_id(release),
            "release id {release:?} is not a safe systemd identifier"
        );
    }
    for fixture in [
        &manifest.core_template,
        &manifest.panel_template,
        &manifest.r1_index,
        &manifest.r2_index,
        &manifest.bad_panel,
    ] {
        ensure!(
            !fixture.contains('/') && !fixture.is_empty(),
            "fixture file name must be a leaf name: {fixture:?}"
        );
        let path = fixtures_dir().join(fixture);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("stat fixture {}", path.display()))?;
        ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "fixture must be a regular non-symlink file: {}",
            path.display()
        );
    }
    let _ = manifest_path;
    Ok(())
}

fn safe_systemd_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn opted_in(manifest: &Manifest) -> Result<bool> {
    match env::var(&manifest.opt_in_env) {
        Ok(value) if value == "1" => Ok(true),
        Ok(value) => bail!(
            "{}={value:?}; refusing a partial opt-in (set it to 1)",
            manifest.opt_in_env
        ),
        Err(env::VarError::NotPresent) => Ok(false),
        Err(error) => Err(error).context("read opt-in environment variable"),
    }
}

fn selected_binary(manifest: &Manifest, kind: &str) -> Result<PathBuf> {
    let (env_name, default, root, minimum) = match kind {
        "core" => (
            &manifest.core_binary_env,
            &manifest.core_binary,
            &manifest.core_root,
            MIN_REAL_CORE_BYTES,
        ),
        "panel" => (
            &manifest.panel_binary_env,
            &manifest.panel_binary,
            &manifest.panel_root,
            MIN_REAL_PANEL_BYTES,
        ),
        other => bail!("unknown binary kind {other}"),
    };
    let candidate = match env::var_os(env_name) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => default.clone(),
    };
    ensure!(
        candidate.is_absolute(),
        "{kind} binary path must be absolute: {}",
        candidate.display()
    );
    ensure!(
        candidate.starts_with(root),
        "{kind} binary {} must live under the declared root {}",
        candidate.display(),
        root.display()
    );
    let metadata = fs::symlink_metadata(&candidate)
        .with_context(|| format!("stat {kind} binary {}", candidate.display()))?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "{kind} binary must be a regular non-symlink file: {}",
        candidate.display()
    );
    ensure!(
        metadata.permissions().mode() & 0o111 != 0,
        "{kind} binary is not executable: {}",
        candidate.display()
    );
    ensure!(
        metadata.len() >= minimum,
        "{kind} binary {} is only {} bytes; refusing a non-real build",
        candidate.display(),
        metadata.len()
    );
    let mut magic = [0_u8; 4];
    File::open(&candidate)
        .and_then(|mut file| file.read_exact(&mut magic))
        .with_context(|| format!("read {kind} ELF magic"))?;
    ensure!(
        magic == [0x7f, b'E', b'L', b'F'],
        "{kind} binary {} is not an ELF executable",
        candidate.display()
    );
    Ok(candidate)
}

// ---------------------------------------------------------------------------
// user systemd / cgroup preflight
// ---------------------------------------------------------------------------

fn systemctl_output(args: &[&str]) -> Result<std::process::Output> {
    Command::new("/usr/bin/systemctl")
        .arg("--user")
        .args(args)
        .output()
        .with_context(|| format!("run systemctl --user {}", args.join(" ")))
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

fn render_output(output: &std::process::Output) -> String {
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
    match systemctl_output(&["show", "--property=Version", "--value"]) {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

fn protected_snapshot() -> Result<BTreeMap<String, String>> {
    let mut snapshot = BTreeMap::new();
    for unit in PROTECTED_UNITS {
        let output = systemctl_output(&[
            "show",
            "--property=ActiveState",
            "--property=MainPID",
            "--property=InvocationID",
            unit,
        ])?;
        ensure!(
            output.status.success(),
            "protect snapshot for {unit} failed: {}",
            render_output(&output)
        );
        snapshot.insert(unit.to_string(), stdout_of(&output));
    }
    Ok(snapshot)
}

fn current_cgroup_text() -> Result<String> {
    fs::read_to_string("/proc/self/cgroup").context("read /proc/self/cgroup")
}

fn ensure_not_in_protected_cgroup() -> Result<()> {
    let text = current_cgroup_text()?;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let path = line
            .splitn(3, ':')
            .nth(2)
            .context("malformed /proc/self/cgroup entry")?;
        for component in path.split('/').filter(|part| !part.is_empty()) {
            if PROTECTED_UNITS.contains(&component) {
                bail!(
                    "refusing standalone live activation from protected cgroup component \
                     {component:?}; run the test via:\n\
                     systemd-run --user --scope --setenv=ECHO_DEPLOY_RUN_STANDALONE_LIVE=1 \
                     cargo test --locked --test standalone_live -- --ignored --nocapture \
                     --test-threads=1"
                );
            }
        }
    }
    Ok(())
}

fn protected_unchanged(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Result<()> {
    for unit in PROTECTED_UNITS {
        ensure!(
            before.get(unit) == after.get(unit),
            "protected production unit {unit} changed during the test:\nbefore={:?}\nafter={:?}",
            before.get(unit),
            after.get(unit)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// bounded command execution
// ---------------------------------------------------------------------------

struct CommandResult {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl CommandResult {
    fn success(&self) -> bool {
        self.status.success()
    }
    fn text(&self) -> String {
        format!(
            "status={:?}\nstdout={}\nstderr={}",
            self.status.code(),
            String::from_utf8_lossy(&self.stdout).trim(),
            String::from_utf8_lossy(&self.stderr).trim()
        )
    }
}

fn run_bounded(mut command: Command, timeout: Duration, label: &str) -> Result<CommandResult> {
    ensure!(!timeout.is_zero(), "{label}: timeout must be non-zero");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let started = Instant::now();
    let mut child = command.spawn().with_context(|| format!("spawn {label}"))?;
    let pid = child.id();
    let stdout = child.stdout.take().context("child stdout")?;
    let stderr = child.stderr.take().context("child stderr")?;
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.take(u64::MAX).read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.take(u64::MAX).read_to_end(&mut bytes);
        bytes
    });
    let status = loop {
        if let Some(status) = child.try_wait().with_context(|| format!("wait {label}"))? {
            break status;
        }
        if started.elapsed() > timeout {
            // SAFETY: kill(2) with a negative pid targets only this child's
            // process group, which was created above with process_group(0).
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            let stdout = stdout_reader.join().unwrap_or_default();
            let stderr = stderr_reader.join().unwrap_or_default();
            bail!(
                "{label} exceeded {:?}\nstdout={}\nstderr={}",
                timeout,
                String::from_utf8_lossy(&stdout).trim(),
                String::from_utf8_lossy(&stderr).trim()
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(CommandResult {
        status,
        stdout,
        stderr,
    })
}

fn run_cli(args: &[&str], timeout: Duration) -> Result<CommandResult> {
    let mut command = Command::new(cli());
    command
        .args(args)
        .current_dir(crate_dir())
        .env("LC_ALL", "C");
    run_bounded(command, timeout, &format!("echo-deploy {}", args.join(" ")))
}

fn run_cli_json(args: &[&str], timeout: Duration) -> Result<Value> {
    let result = run_cli(args, timeout)?;
    ensure!(
        result.success(),
        "echo-deploy {} failed:\n{}",
        args.join(" "),
        result.text()
    );
    serde_json::from_slice(&result.stdout).with_context(|| {
        format!(
            "echo-deploy {} stdout is not JSON: {}",
            args.join(" "),
            String::from_utf8_lossy(&result.stdout)
        )
    })
}

/// First install is the only long-running `systemd-apply` call whose
/// maintenance window is worth probing from this process: on hosts where the
/// adapter's `/proc/<pid>/exe` check is denied, the transaction fails before
/// the post-commit HTTP assertions can run. This keeps the real CLI as the
/// only writer while observing the real Panel gate/HTTP/WS surface.
fn run_first_install_with_probe(
    live: &Live,
    manifest: &Manifest,
) -> Result<(CommandResult, Value)> {
    let mut command = Command::new(cli());
    command
        .args([
            "systemd-apply",
            "--execution",
            live.execution_path.to_str().unwrap(),
            "--release",
            &manifest.release_r1,
            "--confirm",
            "--allow-development",
            "--transaction",
            "standalone-first-r1",
        ])
        .current_dir(crate_dir())
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let started = Instant::now();
    let mut child = command
        .spawn()
        .context("spawn first-install systemd-apply")?;
    let pid = child.id();
    let stdout = child.stdout.take().context("first-install stdout")?;
    let stderr = child.stderr.take().context("first-install stderr")?;
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.take(u64::MAX).read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.take(u64::MAX).read_to_end(&mut bytes);
        bytes
    });
    let panel_port = live.panel_port();
    let mut probes = Vec::new();
    let mut maintenance_probed = false;
    let mut serving_probed = false;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context("wait first-install systemd-apply")?
        {
            break status;
        }
        if started.elapsed() > manifest.transaction_timeout() {
            // SAFETY: only the child's own process group is signalled.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            bail!(
                "first-install systemd-apply exceeded {:?}",
                manifest.transaction_timeout()
            );
        }
        if let Ok(response) = http_get(panel_port, "/api/deployment", Duration::from_millis(500)) {
            if let Ok(value) = serde_json::from_slice::<Value>(&response.body) {
                let state = value["state"].as_str().unwrap_or("");
                if response.status == 503 && state == "maintenance" && !maintenance_probed {
                    maintenance_probed = true;
                    let index_status =
                        http_get(panel_port, "/index.html", Duration::from_millis(500))
                            .map(|response| response.status)
                            .unwrap_or(0);
                    let health_status = http_get(panel_port, "/health", Duration::from_millis(500))
                        .map(|response| response.status)
                        .unwrap_or(0);
                    let ws_status = ws_handshake(panel_port, Duration::from_millis(500))
                        .map(|handshake| handshake.status)
                        .unwrap_or(0);
                    probes.push(json!({
                        "phase": "maintenance",
                        "status": response.status,
                        "deployment": value,
                        "index_status": index_status,
                        "health_status": health_status,
                        "ws_status": ws_status,
                        "gate": live.gate_value().unwrap_or(Value::Null),
                    }));
                } else if response.status == 200
                    && value["release_id"] == json!(&manifest.release_r1)
                    && !serving_probed
                {
                    serving_probed = true;
                    let index = http_get(panel_port, "/index.html", Duration::from_millis(500))
                        .ok()
                        .map(|response| {
                            json!({
                                "status": response.status,
                                "bytes": response.body.len(),
                                "sha256": sha256(&response.body),
                                "matches_fixture": response.body
                                    == fs::read(fixtures_dir().join(&manifest.r1_index)).unwrap_or_default(),
                            })
                        })
                        .unwrap_or(Value::Null);
                    let health = http_get(panel_port, "/health", Duration::from_millis(500))
                        .ok()
                        .map(|response| {
                            json!({"status": response.status, "bytes": response.body.len()})
                        })
                        .unwrap_or(Value::Null);
                    probes.push(json!({
                        "phase": "serving_before_commit_or_after",
                        "status": response.status,
                        "deployment": value,
                        "index": index,
                        "health": health,
                    }));
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok((
        CommandResult {
            status,
            stdout,
            stderr,
        },
        json!({
            "probes": probes,
            "saw_maintenance": maintenance_probed,
            "saw_serving": serving_probed,
        }),
    ))
}

fn run_systemctl_bounded(args: &[&str], timeout: Duration) -> Result<CommandResult> {
    let mut command = Command::new("/usr/bin/systemctl");
    command.arg("--user").args(args);
    run_bounded(
        command,
        timeout,
        &format!("systemctl --user {}", args.join(" ")),
    )
}

fn systemctl_checked(args: &[&str]) -> Result<String> {
    let output = systemctl_output(args)?;
    ensure!(
        output.status.success(),
        "systemctl --user {} failed: {}",
        args.join(" "),
        render_output(&output)
    );
    Ok(stdout_of(&output))
}

fn systemctl_show(unit: &str, properties: &[&str]) -> Result<BTreeMap<String, String>> {
    let mut args = vec!["show".to_string()];
    for property in properties {
        args.push(format!("--property={property}"));
    }
    args.push(unit.to_string());
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let text = systemctl_checked(&borrowed)?;
    let mut map = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.to_string(), value.to_string());
        }
    }
    Ok(map)
}

fn unit_active_state(unit: &str) -> Result<String> {
    Ok(systemctl_show(unit, &["ActiveState"])?
        .remove("ActiveState")
        .unwrap_or_default())
}

fn unit_main_pid(unit: &str) -> Result<u32> {
    let value = systemctl_show(unit, &["MainPID"])?
        .remove("MainPID")
        .unwrap_or_default();
    value
        .parse::<u32>()
        .with_context(|| format!("parse MainPID {value:?} for {unit}"))
}

fn unit_check_active(unit: &str) -> Result<u32> {
    let state = unit_active_state(unit)?;
    ensure!(state == "active", "unit {unit} is not active: {state:?}");
    let pid = unit_main_pid(unit)?;
    ensure!(pid > 0, "unit {unit} has no MainPID");
    Ok(pid)
}

// ---------------------------------------------------------------------------
// small filesystem / config helpers
// ---------------------------------------------------------------------------

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    set_mode(path, PRIVATE_DIR_MODE)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{} must be a real directory",
        path.display()
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "{} must be private, mode {:o}",
        path.display(),
        metadata.permissions().mode() & 0o777
    );
    Ok(())
}

fn create_public_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    set_mode(path, PUBLIC_DIR_MODE)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{} must be a real directory",
        path.display()
    );
    ensure!(
        metadata.permissions().mode() & 0o022 == 0,
        "{} must not be group/other writable, mode {:o}",
        path.display(),
        metadata.permissions().mode() & 0o777
    );
    Ok(())
}

fn write_new_file(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write {}", path.display()))?;
    file.flush()?;
    file.sync_all()?;
    set_mode(path, mode)?;
    Ok(())
}

fn write_private_json(path: &Path, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_new_file(path, &bytes, PRIVATE_FILE_MODE)
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let parent = path.parent().context("json path has no parent")?;
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("json path has invalid file name")?;
    let temp = parent.join(format!(".{leaf}.{}.tmp", unique()));
    write_new_file(&temp, &bytes, PRIVATE_FILE_MODE)?;
    fs::rename(&temp, path).with_context(|| format!("replace {}", path.display()))?;
    File::open(parent)?
        .sync_all()
        .context("fsync json parent")?;
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn render_template(path: &Path, replacements: &[(&str, String)]) -> Result<String> {
    let mut text =
        fs::read_to_string(path).with_context(|| format!("read template {}", path.display()))?;
    for (needle, value) in replacements {
        ensure!(
            text.contains(needle),
            "template {} is missing placeholder {needle}",
            path.display()
        );
        text = text.replace(needle, value);
    }
    ensure!(
        !text.contains('@') || !text.contains("@TMP_ROOT@") && !text.contains("@MANAGEMENT_PORT@"),
        "template {} still contains test placeholders",
        path.display()
    );
    Ok(text)
}

fn choose_two_ports(forbidden: &[u16]) -> Result<(u16, u16)> {
    for _ in 0..64 {
        let first = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("bind ephemeral loopback listener")?;
        let second = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("bind second ephemeral loopback listener")?;
        let first_port = first.local_addr()?.port();
        let second_port = second.local_addr()?.port();
        drop(first);
        drop(second);
        if first_port != 0
            && second_port != 0
            && first_port != second_port
            && !forbidden.contains(&first_port)
            && !forbidden.contains(&second_port)
        {
            return Ok((first_port, second_port));
        }
    }
    bail!("could not allocate two random loopback ports outside {forbidden:?}")
}

fn xdg_unit_dir() -> PathBuf {
    if let Some(config) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(config).join("systemd/user");
    }
    PathBuf::from(env::var_os("HOME").expect("HOME is required")).join(".config/systemd/user")
}

fn minimal_environment(root: &Path) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("PATH".to_string(), FIXED_PATH.to_string()),
        ("HOME".to_string(), root.join("home").display().to_string()),
        ("TMPDIR".to_string(), root.join("tmp").display().to_string()),
        (
            "XDG_CONFIG_HOME".to_string(),
            root.join("xdg/config").display().to_string(),
        ),
        (
            "XDG_DATA_HOME".to_string(),
            root.join("xdg/data").display().to_string(),
        ),
        (
            "XDG_CACHE_HOME".to_string(),
            root.join("xdg/cache").display().to_string(),
        ),
        (
            "XDG_STATE_HOME".to_string(),
            root.join("xdg/state").display().to_string(),
        ),
        (
            "XDG_RUNTIME_DIR".to_string(),
            root.join("xdg/runtime").display().to_string(),
        ),
    ])
}

fn check_binary_help(binary: &Path, label: &str, root: &Path, timeout: Duration) -> Result<String> {
    let mut command = Command::new(binary);
    command
        .arg("--help")
        .current_dir(root.join("work"))
        .env_clear()
        .envs(minimal_environment(root));
    let result = run_bounded(command, timeout, &format!("{label} --help"))?;
    ensure!(
        result.success(),
        "{label} --help failed:\n{}",
        result.text()
    );
    let text = String::from_utf8_lossy(&result.stdout).to_string();
    ensure!(!text.trim().is_empty(), "{label} --help produced no stdout");
    Ok(text)
}

// ---------------------------------------------------------------------------
// live fixture
// ---------------------------------------------------------------------------

/// Remove a test-owned tree even when staged release files/directories are
/// deliberately read-only (0444/0555). `rm -rf`/plain `remove_dir_all` cannot
/// unlink children from a mode-0555 directory without restoring write bits
/// first. This never follows symlinks and only touches paths below the caller's
/// private temp root.
fn force_remove_tree(path: &Path) -> Result<()> {
    fn make_writable(path: &Path) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).with_context(|| format!("stat {}", path.display())),
        };
        if metadata.file_type().is_symlink() {
            return Ok(());
        }
        if metadata.is_dir() {
            set_mode(path, 0o700)?;
            for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
                let entry = entry?;
                make_writable(&entry.path())?;
            }
        } else {
            set_mode(path, 0o600)?;
        }
        Ok(())
    }
    if !path.exists() {
        return Ok(());
    }
    make_writable(path)?;
    fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
}

#[allow(dead_code)] // retained in the structured live-test evidence model
struct ReleaseInfo {
    id: String,
    directory: PathBuf,
    manifest_sha256: String,
    core_sha256: String,
    panel_sha256: String,
    index_bytes: Vec<u8>,
    management_protocol: String,
}

struct Bundles {
    r1: PathBuf,
    r2: PathBuf,
    bad: PathBuf,
}

struct CleanupReport {
    stopped_units: Vec<String>,
    unit_states: BTreeMap<String, String>,
    removed_links: Vec<String>,
    temp_root_removed: bool,
    production_unchanged: bool,
    elapsed_seconds: f64,
}

#[allow(dead_code)] // retained in the structured live-test evidence model
struct Live {
    temp: Option<tempfile::TempDir>,
    base: PathBuf,
    instance: String,
    core_unit: String,
    panel_unit: String,
    updater_unit: String,
    core_binary: PathBuf,
    panel_binary: PathBuf,
    root: PathBuf,
    config_dir: PathBuf,
    core_config: PathBuf,
    panel_config: PathBuf,
    state_dir: PathBuf,
    release_root: PathBuf,
    store: PathBuf,
    instance_state: PathBuf,
    core_socket: PathBuf,
    runtime_config: PathBuf,
    panel_gate: PathBuf,
    unit_dir: PathBuf,
    launcher_dir: PathBuf,
    execution_path: PathBuf,
    deployment_path: PathBuf,
    releases_dir: PathBuf,
    real_unit_dir: PathBuf,
    links: Vec<PathBuf>,
    dropin_links: Vec<PathBuf>,
    spec: InstallSpec,
    execution: SystemdExecution,
    protected_before: BTreeMap<String, String>,
    cleaned: bool,
}

impl Live {
    fn new(manifest: &Manifest, core_binary: PathBuf, panel_binary: PathBuf) -> Result<Self> {
        let state_base = PathBuf::from(env::var_os("HOME").context("HOME is required")?)
            .join(".local/state/echo-deploy-live");
        create_private_dir(&state_base)?;
        let temp = tempfile::Builder::new()
            .prefix(&format!("{}-", manifest.instance_prefix))
            .tempdir_in(&state_base)
            .context("create standalone live temp root")?;
        let root = temp.path().to_path_buf();
        create_private_dir(&root)?;

        let instance = format!(
            "{}-{}-{}",
            manifest.instance_prefix,
            std::process::id(),
            unique()
        );
        ensure!(
            instance.len() <= 100 && safe_systemd_id(&instance),
            "generated instance id is not safe: {instance}"
        );
        let unit_names = (
            format!("echo-agent-{instance}-core.service"),
            format!("echo-agent-{instance}-panel.service"),
            format!("echo-agent-{instance}-update.service"),
        );

        let config_dir = root.join("config");
        let work = root.join("work");
        let home = root.join("home");
        let xdg_root = root.join("xdg");
        let tmp = root.join("tmp");
        let state_dir = root.join("state");
        let instance_state = root.join("instance-state");
        let unit_dir = root.join("units");
        let launcher_dir = root.join("launchers");
        let releases_dir = root.join("releases");
        for directory in [
            &config_dir,
            &work,
            &home,
            &xdg_root.join("config"),
            &xdg_root.join("data"),
            &xdg_root.join("cache"),
            &xdg_root.join("state"),
            &xdg_root.join("runtime"),
            &tmp,
            &state_dir,
            &instance_state,
            &releases_dir,
        ] {
            create_private_dir(directory)?;
        }
        for directory in [&unit_dir, &launcher_dir] {
            create_public_dir(directory)?;
        }
        // The candidate store is created by `echo-deploy init-store`, not here.
        let store = root.join("store");
        ensure!(
            !store.exists(),
            "candidate store must not exist before init-store"
        );

        let (management_port, panel_port) = choose_two_ports(&manifest.forbidden_ports)?;
        let core_config = config_dir.join("echo-agent-core.toml");
        let panel_config = config_dir.join("echo-agent-panel.toml");
        let core_template = fixtures_dir().join(&manifest.core_template);
        let panel_template = fixtures_dir().join(&manifest.panel_template);
        let rendered_core = render_template(
            &core_template,
            &[
                ("@TMP_ROOT@", root.display().to_string()),
                ("@MANAGEMENT_PORT@", management_port.to_string()),
            ],
        )?;
        let rendered_panel = render_template(
            &panel_template,
            &[
                ("@TMP_ROOT@", root.display().to_string()),
                ("@PANEL_PORT@", panel_port.to_string()),
                ("@MANAGEMENT_PORT@", management_port.to_string()),
            ],
        )?;
        write_new_file(&core_config, rendered_core.as_bytes(), PRIVATE_FILE_MODE)?;
        write_new_file(&panel_config, rendered_panel.as_bytes(), PRIVATE_FILE_MODE)?;

        let core_socket = instance_state.join("core.sock");
        let runtime_config = instance_state.join("runtime-adapter.json");
        let panel_gate = instance_state.join("panel-gate.json");
        ensure!(
            core_socket.as_os_str().as_encoded_bytes().len() < 108,
            "control socket path is too long for sockaddr_un: {}",
            core_socket.display()
        );
        let execution_path = root.join("execution.json");
        let deployment_path = root.join("deployment.json");
        let release_root = state_dir.join("current");

        let deployment = Deployment {
            schema_version: 1,
            instance_id: instance.clone(),
            topology: Topology::Standalone,
            scope: Scope::User,
            target_os: "linux".to_string(),
            target_arch: env::consts::ARCH.to_string(),
            store: store.display().to_string(),
            required_modules: manifest.required_modules.clone(),
        };
        let runtime = DeploymentRuntime {
            schema_version: 1,
            core_root: manifest.core_root.clone(),
            panel_root: Some(manifest.panel_root.clone()),
            launcher_directory: launcher_dir.clone(),
            release_store: store.clone(),
            instance_state: instance_state.clone(),
            core_socket: core_socket.clone(),
            runtime_config: runtime_config.clone(),
            panel_gate_file: Some(panel_gate.clone()),
            required_modules: manifest.required_modules.clone(),
            acknowledge_external_writers_stopped: true,
        };
        let spec = InstallSpec {
            instance_id: instance.clone(),
            scope: install::InstallScope::User,
            release_root: release_root.clone(),
            core_config: core_config.clone(),
            panel_config: Some(panel_config.clone()),
            topology: install::InstallTopology::Standalone,
            deploy_executable: cli(),
            state_dir: state_dir.clone(),
            deployment: Some(runtime),
        };
        spec.validate().context("validate InstallSpec")?;
        let rendered = spec.render().context("render real launcher units")?;
        ensure!(
            rendered.schema_version == INSTALL_SCHEMA_VERSION && rendered.plan_preview,
            "rendered install contract changed unexpectedly"
        );
        ensure!(
            rendered.files.len() == 5,
            "standalone render must produce 3 units + 2 launchers, got {}",
            rendered.files.len()
        );
        for required in [
            unit_names.0.as_str(),
            unit_names.1.as_str(),
            unit_names.2.as_str(),
        ] {
            let file = rendered
                .file(required)
                .with_context(|| format!("rendered install is missing {required}"))?;
            ensure!(
                file.contents.contains("[Service]"),
                "rendered unit {required} has no [Service] section"
            );
        }
        let core_launcher_path = spec
            .core_launcher_path()
            .context("deployment core launcher path")?;
        let panel_launcher_path = spec
            .panel_launcher_path()
            .context("deployment panel launcher path")?;
        ensure!(
            rendered
                .file(core_launcher_path.file_name().unwrap().to_str().unwrap())
                .is_some(),
            "rendered core launcher is missing"
        );
        ensure!(
            rendered
                .file(panel_launcher_path.file_name().unwrap().to_str().unwrap())
                .is_some(),
            "rendered panel launcher is missing"
        );
        let panel_unit_text = rendered.panel_unit().context("panel unit")?;
        ensure!(
            panel_unit_text.contents.contains(&unit_names.0),
            "Panel unit must order itself after the Core unit"
        );
        let updater_text = rendered.updater_unit().context("updater unit")?;
        ensure!(
            updater_text.contents.contains("apply-request")
                && updater_text.contents.contains("--request")
                && updater_text.contents.contains("--confirm"),
            "updater unit must invoke the real apply-request CLI"
        );
        ensure!(
            updater_text.contents.contains("Type=oneshot"),
            "updater unit must be an independent oneshot service"
        );

        let report = match installation::install_files(&spec, &unit_dir, &launcher_dir, true) {
            Ok(report) => report,
            Err(failure) => bail!(
                "installation_files failed: {:#}\npartial_ledger={}",
                failure.error,
                failure
                    .partial_ledger_json()
                    .unwrap_or_else(|error| format!("<unserializable: {error}>"))
            ),
        };
        ensure!(
            report.status == "files_installed_not_activated",
            "unexpected installation status: {}",
            report.status
        );
        ensure!(
            !report.systemctl_invoked && !report.services_started,
            "installation_files must not invoke systemctl or start services"
        );
        ensure!(
            report.ledger.files.len() == 5,
            "ledger must own 5 files, got {}",
            report.ledger.files.len()
        );
        for owned in &report.ledger.files {
            ensure!(
                owned.path.is_absolute() && owned.path.starts_with(&unit_dir)
                    || owned.path.starts_with(&launcher_dir),
                "ledger file is outside the test directories: {}",
                owned.path.display()
            );
            let actual = sha256_file(&owned.path)?;
            ensure!(
                actual == owned.sha256,
                "installed file hash mismatch: {}",
                owned.path.display()
            );
        }

        let runtime_adapter: RuntimeAdapterConfig = spec
            .runtime_adapter_config()?
            .context("deployment runtime adapter config")?;
        write_private_json(&runtime_config, &serde_json::to_value(&runtime_adapter)?)?;
        let gate_document = spec
            .panel_gate_document(&manifest.release_r1, &manifest.management_protocol)?
            .context("panel gate document")?;
        write_private_json(&panel_gate, &serde_json::to_value(&gate_document)?)?;

        let execution = SystemdExecution {
            deployment: deployment.clone(),
            state_dir: state_dir.clone(),
            unit_directory: unit_dir.clone(),
            current_selector: release_root.clone(),
            core_unit: unit_names.0.clone(),
            panel_unit: Some(unit_names.1.clone()),
            control_adapter: core_launcher_path.clone(),
            timeout_seconds: manifest.systemd_execution_timeout_seconds,
            reversible_data: true,
        };
        execution.validate().context("validate SystemdExecution")?;
        write_private_json(&execution_path, &serde_json::to_value(&execution)?)?;
        write_private_json(&deployment_path, &serde_json::to_value(&deployment)?)?;

        let mut live = Self {
            temp: Some(temp),
            base: state_base,
            instance,
            core_unit: unit_names.0,
            panel_unit: unit_names.1,
            updater_unit: unit_names.2,
            core_binary,
            panel_binary,
            root: root.clone(),
            config_dir,
            core_config,
            panel_config,
            state_dir,
            release_root,
            store,
            instance_state,
            core_socket,
            runtime_config,
            panel_gate,
            unit_dir,
            launcher_dir,
            execution_path,
            deployment_path,
            releases_dir,
            real_unit_dir: xdg_unit_dir(),
            links: Vec::new(),
            dropin_links: Vec::new(),
            spec,
            execution,
            protected_before: BTreeMap::new(),
            cleaned: false,
        };
        live.write_dropins()?;
        live.link_and_reload()?;
        live.protected_before = protected_snapshot()?;
        eprintln!(
            "[standalone-live] instance={} root={} core_port={} panel_port={}",
            live.instance,
            live.root.display(),
            management_port,
            panel_port
        );
        eprintln!(
            "[standalone-live] units: {} {} {}",
            live.core_unit, live.panel_unit, live.updater_unit
        );
        Ok(live)
    }

    fn panel_port(&self) -> u16 {
        let text = fs::read_to_string(&self.panel_config).expect("panel config");
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("bind_address") {
                let value = rest.trim().trim_start_matches('=').trim().trim_matches('"');
                if let Some((_, port)) = value.rsplit_once(':') {
                    return port.parse().expect("panel port");
                }
            }
        }
        panic!("panel config has no bind_address");
    }

    fn management_port(&self) -> u16 {
        let text = fs::read_to_string(&self.core_config).expect("core config");
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("management_address") {
                let value = rest.trim().trim_start_matches('=').trim().trim_matches('"');
                if let Some((_, port)) = value.rsplit_once(':') {
                    return port.parse().expect("management port");
                }
            }
        }
        panic!("core config has no management_address");
    }

    fn write_dropins(&self) -> Result<()> {
        let home = self.root.join("home");
        let xdg = self.root.join("xdg");
        let tmp = self.root.join("tmp");
        let common = [
            ("HOME", home.display().to_string()),
            ("XDG_CONFIG_HOME", xdg.join("config").display().to_string()),
            ("XDG_DATA_HOME", xdg.join("data").display().to_string()),
            ("XDG_CACHE_HOME", xdg.join("cache").display().to_string()),
            ("XDG_STATE_HOME", xdg.join("state").display().to_string()),
            ("TMPDIR", tmp.display().to_string()),
        ];
        for (unit, extra) in [
            (self.core_unit.as_str(), Vec::new()),
            (
                self.panel_unit.as_str(),
                vec![(
                    "ECHO_DEPLOY_GATE_FILE",
                    self.panel_gate.display().to_string(),
                )],
            ),
            (self.updater_unit.as_str(), Vec::new()),
        ] {
            let directory = self.unit_dir.join(format!("{unit}.d"));
            create_public_dir(&directory)?;
            let mut contents = String::from(
                "# Test-only isolation drop-in for tests/standalone_live.rs.\n\
                 # The rendered unit text is not modified; these Environment= lines\n\
                 # are the operator-owned HOME/XDG/TMPDIR and Panel gate wiring.\n\
                 [Service]\n",
            );
            for (key, value) in common.iter().chain(extra.iter()) {
                contents.push_str("Environment=");
                contents.push_str(&install::systemd_env_assignment(key, value)?);
                contents.push('\n');
            }
            if unit == self.panel_unit.as_str() {
                contents.push_str(
                    "# The gate file is read on every request; the path is fixed for this\n\
                     # instance and is written atomically by the runtime adapter.\n",
                );
            }
            write_new_file(
                &directory.join("50-standalone-live-isolation.conf"),
                contents.as_bytes(),
                0o644,
            )?;
        }
        Ok(())
    }

    fn link_and_reload(&mut self) -> Result<()> {
        fs::create_dir_all(&self.real_unit_dir).with_context(|| {
            format!(
                "create real user unit directory {}",
                self.real_unit_dir.display()
            )
        })?;
        let unit_files = [
            self.unit_dir.join(&self.core_unit),
            self.unit_dir.join(&self.panel_unit),
            self.unit_dir.join(&self.updater_unit),
        ];
        for source in unit_files {
            ensure!(
                source.is_file() && !source.is_symlink(),
                "rendered unit is missing: {}",
                source.display()
            );
            let target = self
                .real_unit_dir
                .join(source.file_name().context("unit name")?);
            ensure!(
                fs::symlink_metadata(&target).is_err(),
                "refusing to overwrite existing user unit {}",
                target.display()
            );
            symlink(&source, &target)
                .with_context(|| format!("link {} -> {}", target.display(), source.display()))?;
            self.links.push(target);
        }
        for unit in [
            self.core_unit.clone(),
            self.panel_unit.clone(),
            self.updater_unit.clone(),
        ] {
            let source = self.unit_dir.join(format!("{unit}.d"));
            let target = self.real_unit_dir.join(format!("{unit}.d"));
            ensure!(
                fs::symlink_metadata(&target).is_err(),
                "refusing to overwrite existing drop-in dir {}",
                target.display()
            );
            symlink(&source, &target)
                .with_context(|| format!("link {} -> {}", target.display(), source.display()))?;
            self.dropin_links.push(target);
        }
        systemctl_checked(&["daemon-reload"])?;
        for unit in [
            self.core_unit.as_str(),
            self.panel_unit.as_str(),
            self.updater_unit.as_str(),
        ] {
            let state = systemctl_show(unit, &["LoadState"])?
                .remove("LoadState")
                .unwrap_or_default();
            ensure!(
                state == "loaded",
                "unit {unit} was not loaded by the user manager (LoadState={state:?})"
            );
        }
        Ok(())
    }

    fn journal_tail(&self, unit: &str, lines: usize) -> String {
        let output = Command::new("/usr/bin/journalctl")
            .args([
                "--user",
                "-u",
                unit,
                "-n",
                &lines.to_string(),
                "--no-pager",
                "-o",
                "cat",
            ])
            .output();
        match output {
            Ok(output) => {
                let text = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                text.trim().to_string()
            }
            Err(error) => format!("<journalctl failed: {error}>"),
        }
    }

    fn all_tails(&self, lines: usize) -> String {
        format!(
            "core journal:\n{}\npanel journal:\n{}\nupdater journal:\n{}",
            self.journal_tail(&self.core_unit, lines),
            self.journal_tail(&self.panel_unit, lines),
            self.journal_tail(&self.updater_unit, lines)
        )
    }

    fn selector_target(&self) -> Result<Option<PathBuf>> {
        match fs::read_link(&self.release_root) {
            Ok(target) => Ok(Some(target)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read current selector"),
        }
    }

    fn assert_selector(&self, release: &str) -> Result<()> {
        let expected = self.store.join(release);
        let actual = self
            .selector_target()?
            .with_context(|| format!("no current selector at {}", self.release_root.display()))?;
        ensure!(
            actual == expected,
            "selector points at {}, expected {}",
            actual.display(),
            expected.display()
        );
        Ok(())
    }

    fn write_update_request(&self, release: &str, transaction: &str) -> Result<PathBuf> {
        let request = json!({
            "schema_version": 1,
            "execution": self.execution_path,
            "release": release,
            "transaction": transaction,
            "allow_development": true,
            "trust_store": null,
            "key_id": null,
            "signature": null,
        });
        let path = self.spec.update_request_path();
        write_json_atomic(&path, &request)?;
        Ok(path)
    }

    fn start_updater(&self, timeout: Duration) -> Result<CommandResult> {
        run_systemctl_bounded(&["start", self.updater_unit.as_str()], timeout)
    }

    fn read_transaction(&self) -> Result<Value> {
        let path = self
            .state_dir
            .join(format!("transaction-{}.json", self.instance));
        read_json(&path)
    }

    fn gate_value(&self) -> Result<Value> {
        read_json(&self.panel_gate)
    }

    fn set_gate_maintenance(&self, maintenance: bool) -> Result<Value> {
        let mut gate = self.gate_value()?;
        gate["maintenance"] = Value::Bool(maintenance);
        write_json_atomic(&self.panel_gate, &gate)?;
        Ok(gate)
    }

    fn cleanup_links(&mut self) -> Vec<String> {
        let mut removed = Vec::new();
        for target in self.dropin_links.drain(..) {
            if let Ok(actual) = fs::read_link(&target) {
                if actual.starts_with(&self.unit_dir) && fs::remove_file(&target).is_ok() {
                    removed.push(format!("{}", target.display()));
                }
            }
        }
        for target in self.links.drain(..) {
            if let Ok(actual) = fs::read_link(&target) {
                if actual.starts_with(&self.unit_dir) && fs::remove_file(&target).is_ok() {
                    removed.push(format!("{}", target.display()));
                }
            }
        }
        removed
    }

    fn cleanup(&mut self, remove_temp: bool) -> Result<CleanupReport> {
        let started = Instant::now();
        if self.cleaned {
            return Ok(CleanupReport {
                stopped_units: Vec::new(),
                unit_states: BTreeMap::new(),
                removed_links: Vec::new(),
                temp_root_removed: !self.root.exists(),
                production_unchanged: true,
                elapsed_seconds: started.elapsed().as_secs_f64(),
            });
        }
        self.cleaned = true;
        let mut stopped_units = Vec::new();
        for unit in [
            self.panel_unit.as_str(),
            self.core_unit.as_str(),
            self.updater_unit.as_str(),
        ] {
            // Never stop a unit name that is not ours: these names contain the
            // per-test instance token and were checked at load time.
            let _ = systemctl_output(&["stop", unit]);
            stopped_units.push(unit.to_string());
        }
        let _ = systemctl_output(&["reset-failed", self.core_unit.as_str()]);
        let _ = systemctl_output(&["reset-failed", self.panel_unit.as_str()]);
        let _ = systemctl_output(&["reset-failed", self.updater_unit.as_str()]);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let mut all_gone = true;
            for unit in [
                self.core_unit.as_str(),
                self.panel_unit.as_str(),
                self.updater_unit.as_str(),
            ] {
                if let Ok(values) = systemctl_show(unit, &["ActiveState", "MainPID"]) {
                    let state = values.get("ActiveState").map(String::as_str).unwrap_or("");
                    let pid = values
                        .get("MainPID")
                        .and_then(|value| value.parse::<u32>().ok())
                        .unwrap_or(0);
                    if state != "inactive" && state != "failed" || pid != 0 {
                        all_gone = false;
                    }
                }
            }
            if all_gone || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        // Capture the stopped unit states while the unit files are still
        // loaded; after daemon-reload + link removal systemctl may report
        // LoadState=not-found instead of the inactive state.
        let mut unit_states = BTreeMap::new();
        for unit in [
            self.core_unit.as_str(),
            self.panel_unit.as_str(),
            self.updater_unit.as_str(),
        ] {
            let values = systemctl_show(unit, &["ActiveState", "MainPID"]).unwrap_or_default();
            unit_states.insert(
                unit.to_string(),
                format!(
                    "ActiveState={} MainPID={}",
                    values.get("ActiveState").cloned().unwrap_or_default(),
                    values.get("MainPID").cloned().unwrap_or_default()
                ),
            );
        }
        let removed_links = self.cleanup_links();
        let _ = systemctl_output(&["daemon-reload"]);
        let mut temp_root_removed = false;
        if remove_temp {
            if let Some(temp) = self.temp.take() {
                let path = temp.path().to_path_buf();
                drop(temp);
                force_remove_tree(&path)?;
                temp_root_removed = !path.exists();
            }
        }
        Ok(CleanupReport {
            stopped_units,
            unit_states,
            removed_links,
            temp_root_removed,
            production_unchanged: false,
            elapsed_seconds: started.elapsed().as_secs_f64(),
        })
    }

    fn finish(&mut self) -> Result<Value> {
        let mut report = self.cleanup(true)?;
        let after = protected_snapshot()?;
        protected_unchanged(&self.protected_before, &after)?;
        report.production_unchanged = true;
        for (unit, state) in &report.unit_states {
            ensure!(
                state.starts_with("ActiveState=inactive")
                    || state.starts_with("ActiveState=failed"),
                "unit {unit} was not stopped by cleanup: {state}"
            );
        }
        ensure!(
            report.temp_root_removed || self.root.exists(),
            "cleanup report is inconsistent"
        );
        eprintln!(
            "[standalone-live] cleanup: stopped={:?} states={:?} removed_links={} elapsed={:.3}s",
            report.stopped_units,
            report.unit_states,
            report.removed_links.len(),
            report.elapsed_seconds
        );
        Ok(serde_json::to_value(json!({
            "stopped_units": report.stopped_units,
            "unit_states": report.unit_states,
            "removed_links": report.removed_links,
            "temp_root_removed": report.temp_root_removed,
            "production_unchanged": report.production_unchanged,
            "protected_before": self.protected_before,
            "protected_after": after,
            "elapsed_seconds": report.elapsed_seconds,
        }))?)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if !self.cleaned {
            if let Err(error) = self.cleanup(true) {
                eprintln!(
                    "[standalone-live] CLEANUP ERROR for instance {}: {error:#}",
                    self.instance
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// release bundles and staging
// ---------------------------------------------------------------------------

fn append_release_marker(bytes: &mut Vec<u8>, release: &str) {
    bytes.extend_from_slice(format!("\n# standalone-live release {release}\n").as_bytes());
}

fn write_release_manifest(
    directory: &Path,
    release: &str,
    artifacts: &[(&str, &str, &str, Vec<u8>, bool)],
    management_protocol: &str,
) -> Result<()> {
    let mut artifact_values = Vec::new();
    for (id, kind, path, bytes, executable) in artifacts {
        artifact_values.push(json!({
            "id": id,
            "kind": kind,
            "path": path,
            "sha256": sha256(bytes),
            "size": bytes.len(),
            "executable": executable,
            "source_id": "standalone-live",
        }));
    }
    let manifest = json!({
        "schema_version": 1,
        "release_id": release,
        "target_os": "linux",
        "target_arch": env::consts::ARCH,
        "management_protocol": management_protocol,
        "development": true,
        "sources": [{
            "id": "standalone-live",
            "revision": "standalone-live-fixture",
            "snapshot_sha256": "a".repeat(64),
            "lock_sha256": "b".repeat(64),
        }],
        "artifacts": artifact_values,
    });
    write_new_file(
        &directory.join("release.json"),
        &serde_json::to_vec_pretty(&manifest)?,
        0o644,
    )
}

fn build_bundles(live: &Live, manifest: &Manifest) -> Result<Bundles> {
    let r1 = live.releases_dir.join(&manifest.release_r1);
    let r2 = live.releases_dir.join(&manifest.release_r2);
    let bad = live.releases_dir.join(&manifest.release_r2_bad);
    for directory in [&r1, &r2, &bad] {
        fs::create_dir_all(directory.join("bin"))?;
        fs::create_dir_all(directory.join("web"))?;
    }

    let core_r1 = fs::read(&live.core_binary).context("read real Core binary")?;
    let panel_r1 = fs::read(&live.panel_binary).context("read real Panel binary")?;
    ensure!(
        core_r1.starts_with(&[0x7f, b'E', b'L', b'F'])
            && panel_r1.starts_with(&[0x7f, b'E', b'L', b'F']),
        "real binaries must be ELF"
    );

    let mut core_r2 = core_r1.clone();
    let mut panel_r2 = panel_r1.clone();
    append_release_marker(&mut core_r2, &manifest.release_r2);
    append_release_marker(&mut panel_r2, &manifest.release_r2);
    let mut core_bad = core_r1.clone();
    append_release_marker(&mut core_bad, &manifest.release_r2_bad);
    let bad_panel =
        fs::read(fixtures_dir().join(&manifest.bad_panel)).context("read bad Panel fixture")?;
    let index_r1 = fs::read(fixtures_dir().join(&manifest.r1_index))?;
    let index_r2 = fs::read(fixtures_dir().join(&manifest.r2_index))?;

    for (directory, core, panel, index, release) in [
        (&r1, &core_r1, &panel_r1, &index_r1, &manifest.release_r1),
        (&r2, &core_r2, &panel_r2, &index_r2, &manifest.release_r2),
        (
            &bad,
            &core_bad,
            &bad_panel,
            &index_r2,
            &manifest.release_r2_bad,
        ),
    ] {
        write_new_file(&directory.join(CORE_ARTIFACT_PATH), core, 0o755)?;
        write_new_file(&directory.join(PANEL_ARTIFACT_PATH), panel, 0o755)?;
        write_new_file(&directory.join(WEB_ARTIFACT_PATH), index, 0o644)?;
        write_release_manifest(
            directory,
            release,
            &[
                ("core", "core", CORE_ARTIFACT_PATH, core.clone(), true),
                ("panel", "panel", PANEL_ARTIFACT_PATH, panel.clone(), true),
                ("web", "web", WEB_ARTIFACT_PATH, index.clone(), false),
            ],
            &manifest.management_protocol,
        )?;
    }
    Ok(Bundles { r1, r2, bad })
}

fn release_info(live: &Live, release: &str, index_fixture: &Path) -> Result<ReleaseInfo> {
    let directory = live.store.join(release);
    let manifest = read_json(&directory.join("release.json"))?;
    ensure!(
        manifest["release_id"] == json!(release),
        "staged release id mismatch"
    );
    let artifact = |kind: &str| -> Result<(String, String)> {
        let entries = manifest["artifacts"]
            .as_array()
            .context("release artifacts array")?;
        let mut found = entries.iter().filter(|entry| entry["kind"] == json!(kind));
        let entry = found
            .next()
            .with_context(|| format!("missing {kind} artifact"))?;
        ensure!(
            found.next().is_none(),
            "duplicate {kind} artifact in {release}"
        );
        Ok((
            entry["sha256"]
                .as_str()
                .context("artifact sha")?
                .to_string(),
            entry["path"].as_str().context("artifact path")?.to_string(),
        ))
    };
    let (core_sha, core_path) = artifact("core")?;
    let (panel_sha, panel_path) = artifact("panel")?;
    let (web_sha, web_path) = artifact("web")?;
    ensure!(
        core_path == CORE_ARTIFACT_PATH
            && panel_path == PANEL_ARTIFACT_PATH
            && web_path == WEB_ARTIFACT_PATH,
        "release artifact paths changed"
    );
    let index_bytes = fs::read(directory.join(WEB_ARTIFACT_PATH))?;
    ensure!(
        sha256(&index_bytes) == web_sha && index_bytes == fs::read(index_fixture)?,
        "staged index bytes do not match the fixture"
    );
    let manifest_sha256 = sha256_file(&directory.join("release.json"))?;
    Ok(ReleaseInfo {
        id: release.to_string(),
        directory,
        manifest_sha256,
        core_sha256: core_sha,
        panel_sha256: panel_sha,
        index_bytes,
        management_protocol: manifest["management_protocol"]
            .as_str()
            .context("management_protocol")?
            .to_string(),
    })
}

fn stage_releases(live: &Live, manifest: &Manifest, bundles: &Bundles) -> Result<()> {
    run_cli_json(
        &["init-store", "--store", live.store.to_str().unwrap()],
        Duration::from_secs(60),
    )
    .context("init-store")?;
    for (bundle, release) in [
        (&bundles.r1, &manifest.release_r1),
        (&bundles.r2, &manifest.release_r2),
        (&bundles.bad, &manifest.release_r2_bad),
    ] {
        let output = run_cli(
            &[
                "stage",
                "--bundle",
                bundle.to_str().unwrap(),
                "--deployment",
                live.deployment_path.to_str().unwrap(),
                "--allow-development",
            ],
            Duration::from_secs(300),
        )?;
        ensure!(
            output.success(),
            "stage {release} failed:\n{}",
            output.text()
        );
        ensure!(
            live.store.join(release).is_dir(),
            "stage {release} did not publish a store directory"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// independent /proc image probe
// ---------------------------------------------------------------------------

fn proc_start_time(pid: u32) -> Result<u64> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat"))
        .with_context(|| format!("read /proc/{pid}/stat"))?;
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

#[allow(dead_code)] // retained in the structured live-test evidence model
struct ProcImage {
    unit: String,
    pid: u32,
    exe_path: PathBuf,
    sha256: String,
    cgroup: String,
    start_time: u64,
}

fn probe_process_image(unit: &str, expected_sha256: &str) -> Result<ProcImage> {
    let pid = unit_main_pid(unit)?;
    ensure!(pid > 0, "unit {unit} has no MainPID for /proc probe");
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .with_context(|| format!("read /proc/{pid}/cgroup"))?;
    ensure!(
        cgroup.contains(unit),
        "MainPID {pid} for {unit} is not in that unit's cgroup: {cgroup:?}"
    );
    let exe_path = fs::read_link(format!("/proc/{pid}/exe"))
        .with_context(|| format!("readlink /proc/{pid}/exe for {unit}"))?;
    let metadata = fs::metadata(format!("/proc/{pid}/exe"))
        .with_context(|| format!("stat /proc/{pid}/exe for {unit}"))?;
    let euid = unsafe { libc::geteuid() };
    ensure!(
        metadata.uid() == euid || metadata.uid() == 0,
        "process image for {unit} is owned by an untrusted uid"
    );
    ensure!(
        metadata.permissions().mode() & 0o022 == 0,
        "process image for {unit} is group/other writable"
    );
    let start_before = proc_start_time(pid)?;
    let mut file = match File::open(format!("/proc/{pid}/exe")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            bail!(
                "permission denied reading /proc/{pid}/exe for {unit}: {error}. \
                 This test does not call PR_SET_PTRACER and does not change host \
                 ptrace/Yama policy; the permission boundary is reported as a failure, \
                 not converted into a pass."
            );
        }
        Err(error) => {
            return Err(error).with_context(|| format!("open /proc/{pid}/exe for {unit}"));
        }
    };
    let actual = sha256_reader(&mut file).with_context(|| format!("hash /proc/{pid}/exe"))?;
    ensure!(
        proc_start_time(pid)? == start_before,
        "process start time changed while hashing {unit}"
    );
    ensure!(
        unit_main_pid(unit)? == pid,
        "MainPID changed while hashing {unit}"
    );
    ensure!(
        actual == expected_sha256,
        "running image sha {actual} for {unit} does not match release sha {expected_sha256}"
    );
    Ok(ProcImage {
        unit: unit.to_string(),
        pid,
        exe_path,
        sha256: actual,
        cgroup,
        start_time: start_before,
    })
}

// ---------------------------------------------------------------------------
// HTTP / WebSocket client (std only, no new dependencies)
// ---------------------------------------------------------------------------

struct HttpResponse {
    status: u16,
    reason: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

fn connect_loopback(port: u16, timeout: Duration) -> Result<TcpStream> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let stream = TcpStream::connect_timeout(&address, timeout)
        .with_context(|| format!("connect 127.0.0.1:{port}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .context("set read timeout")?;
    stream
        .set_write_timeout(Some(timeout))
        .context("set write timeout")?;
    Ok(stream)
}

fn read_until_headers(stream: &mut TcpStream, limit: usize) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut all = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).context("read HTTP headers")?;
        if read == 0 {
            bail!("connection closed before HTTP headers were complete");
        }
        all.extend_from_slice(&buffer[..read]);
        ensure!(all.len() <= limit, "HTTP headers exceeded {limit} bytes");
        if let Some(index) = all.windows(4).position(|window| window == b"\r\n\r\n") {
            let tail = all.split_off(index + 4);
            return Ok((all, tail));
        }
    }
}

fn decode_chunked(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut cursor = 0_usize;
    loop {
        let line_end = bytes[cursor..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .context("chunked body: missing size line terminator")?
            + cursor;
        let line = std::str::from_utf8(&bytes[cursor..line_end]).context("chunk size utf8")?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).context("chunk size hex")?;
        cursor = line_end + 2;
        if size == 0 {
            // Consume optional trailers until the terminating empty line.
            let end = bytes[cursor..]
                .windows(2)
                .position(|window| window == b"\r\n")
                .map(|offset| cursor + offset)
                .unwrap_or(bytes.len());
            let _ = end;
            return Ok(output);
        }
        ensure!(
            cursor + size + 2 <= bytes.len(),
            "chunked body is truncated"
        );
        output.extend_from_slice(&bytes[cursor..cursor + size]);
        ensure!(
            &bytes[cursor + size..cursor + size + 2] == b"\r\n",
            "chunked body: missing CRLF after chunk"
        );
        cursor += size + 2;
    }
}

fn parse_http_response(header_bytes: &[u8], mut body: Vec<u8>) -> Result<HttpResponse> {
    let text = std::str::from_utf8(header_bytes).context("HTTP headers are not UTF-8")?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().context("missing HTTP status line")?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().context("missing HTTP version")?;
    ensure!(
        version.starts_with("HTTP/"),
        "unexpected HTTP status line: {status_line:?}"
    );
    let status = parts
        .next()
        .context("missing HTTP status")?
        .parse::<u16>()
        .context("invalid HTTP status")?;
    let reason = parts.next().unwrap_or("").to_string();
    let mut headers = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .with_context(|| format!("malformed HTTP header: {line:?}"))?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    if headers
        .get("transfer-encoding")
        .map(|value| value.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        body = decode_chunked(&body)?;
    } else if let Some(length) = headers.get("content-length") {
        let length = length.parse::<usize>().context("invalid content-length")?;
        ensure!(
            body.len() >= length,
            "HTTP body is shorter than content-length ({}/{length})",
            body.len()
        );
        body.truncate(length);
    }
    Ok(HttpResponse {
        status,
        reason,
        headers,
        body,
    })
}

fn http_get(port: u16, path: &str, timeout: Duration) -> Result<HttpResponse> {
    let mut stream = connect_loopback(port, timeout)?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .with_context(|| format!("write GET {path}"))?;
    let (headers, mut body) = read_until_headers(&mut stream, 64 * 1024)?;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stream.read(&mut buffer).context("read HTTP body")?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..read]);
        ensure!(
            body.len() <= MAX_HTTP_BYTES,
            "HTTP body exceeded {MAX_HTTP_BYTES} bytes"
        );
    }
    parse_http_response(&headers, body)
}

fn wait_for_http_serving(port: u16, expected_release: &str, timeout: Duration) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    let mut last_error;
    loop {
        match http_get(port, "/api/deployment", Duration::from_secs(3)) {
            Ok(response) => match serde_json::from_slice::<Value>(&response.body) {
                Ok(value) => {
                    if response.status == 200 && value["release_id"] == json!(expected_release) {
                        return Ok(value);
                    }
                    last_error = format!(
                        "status={} body={}",
                        response.status,
                        String::from_utf8_lossy(&response.body)
                    );
                }
                Err(error) => last_error = format!("invalid JSON: {error}"),
            },
            Err(error) => last_error = format!("{error:#}"),
        }
        if Instant::now() >= deadline {
            bail!(
                "Panel did not serve {expected_release} within {:?}: {last_error}",
                timeout
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn assert_deployment_status(
    value: &Value,
    instance: &str,
    release: &str,
    protocol: &str,
    maintenance: bool,
) -> Result<()> {
    ensure!(
        value["configured"] == json!(true),
        "/api/deployment does not report a configured gate: {value}"
    );
    ensure!(
        value["state"]
            == json!(if maintenance {
                "maintenance"
            } else {
                "serving"
            }),
        "/api/deployment state mismatch: {value}"
    );
    ensure!(
        value["ready"] == json!(!maintenance),
        "/api/deployment ready mismatch: {value}"
    );
    ensure!(
        value["fail_closed"] == json!(maintenance),
        "/api/deployment fail_closed mismatch: {value}"
    );
    ensure!(
        value["maintenance"] == json!(maintenance),
        "/api/deployment maintenance mismatch: {value}"
    );
    ensure!(
        value["identity_source"] == json!("deployment_file"),
        "/api/deployment identity_source mismatch: {value}"
    );
    ensure!(
        value["identity_available"] == json!(true),
        "/api/deployment identity is not available: {value}"
    );
    ensure!(
        value["identity_is_binary_build_id"] == json!(false),
        "/api/deployment must not claim a binary build id: {value}"
    );
    ensure!(
        value["schema_version"] == json!(1),
        "/api/deployment schema mismatch: {value}"
    );
    ensure!(
        value["instance_id"] == json!(instance),
        "/api/deployment instance mismatch: {value}"
    );
    ensure!(
        value["release_id"] == json!(release),
        "/api/deployment release mismatch: {value}"
    );
    ensure!(
        value["management_protocol"] == json!(protocol),
        "/api/deployment protocol mismatch: {value}"
    );
    ensure!(
        value["gate_error"].is_null(),
        "/api/deployment reports a gate error: {value}"
    );
    ensure!(
        value["new_websocket_connections"]
            == json!(if maintenance { "rejected" } else { "accepted" }),
        "/api/deployment new_websocket_connections mismatch: {value}"
    );
    // The Panel status document still hard-codes false; the proxy now actively
    // closes existing sockets on maintenance. Record the declared field but do
    // not use it as the close proof (the WebSocket test does that).
    ensure!(
        value["existing_websockets_disconnected"].is_boolean(),
        "/api/deployment close declaration missing: {value}"
    );
    Ok(())
}

fn assert_index_bytes(port: u16, path: &str, expected: &[u8], label: &str) -> Result<Value> {
    let response = http_get(port, path, Duration::from_secs(5))?;
    ensure!(
        response.status == 200,
        "{label}: GET {path} returned {} {}: {}",
        response.status,
        response.reason,
        response.text()
    );
    let content_type = response.header("content-type").unwrap_or("");
    ensure!(
        content_type.starts_with("text/html"),
        "{label}: GET {path} content-type is {content_type:?}"
    );
    let cache = response.header("cache-control").unwrap_or("");
    ensure!(
        cache.contains("no-store"),
        "{label}: GET {path} cache-control is {cache:?}"
    );
    ensure!(
        response.body == expected,
        "{label}: GET {path} bytes differ from the fixture (got {} bytes, expected {})",
        response.body.len(),
        expected.len()
    );
    Ok(json!({
        "path": path,
        "status": response.status,
        "bytes": response.body.len(),
        "sha256": sha256(&response.body),
        "content_type": content_type,
        "cache_control": cache,
        "byte_acceptance_only": true,
        "browser_frontend_acceptance": false,
    }))
}

fn probe_health(port: u16, expected_index: &[u8]) -> Result<Value> {
    let response = http_get(port, "/health", Duration::from_secs(5))?;
    let content_type = response.header("content-type").unwrap_or("").to_string();
    let classification = if response.status == 404 {
        "not_implemented"
    } else if response.status == 200 && response.body == expected_index {
        "spa_index_fallback_not_a_health_endpoint"
    } else if response.status == 503 {
        "deployment_gate_maintenance_page"
    } else if content_type.contains("application/json") {
        "json_endpoint_candidate"
    } else {
        "other"
    };
    Ok(json!({
        "path": "/health",
        "status": response.status,
        "content_type": content_type,
        "bytes": response.body.len(),
        "body_sha256": sha256(&response.body),
        "classification": classification,
        "claim": "recorded as observed; this test does not claim a real health endpoint unless classification is json_endpoint_candidate",
    }))
}

// --- WebSocket handshake / close ------------------------------------------

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    let mut index = 0;
    while index < bytes.len() {
        let first = bytes[index];
        let second = bytes.get(index + 1).copied().unwrap_or(0);
        let third = bytes.get(index + 2).copied().unwrap_or(0);
        let triple = ((first as u32) << 16) | ((second as u32) << 8) | third as u32;
        output.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        output.push(if index + 1 < bytes.len() {
            TABLE[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        output.push(if index + 2 < bytes.len() {
            TABLE[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
        index += 3;
    }
    output
}

fn random_ws_key() -> Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .context("read /dev/urandom for Sec-WebSocket-Key")?;
    Ok(base64(&bytes))
}

#[allow(dead_code)] // retained in the structured live-test evidence model
struct WsHandshake {
    status: u16,
    reason: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    socket: Option<WsSocket>,
}

struct WsSocket {
    stream: TcpStream,
    buffered: Vec<u8>,
}

fn ws_handshake(port: u16, timeout: Duration) -> Result<WsHandshake> {
    let mut stream = connect_loopback(port, timeout)?;
    let key = random_ws_key()?;
    let request = format!(
        "GET /ws HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Origin: http://127.0.0.1:{port}\r\n\
         \r\n"
    );
    stream
        .write_all(request.as_bytes())
        .context("write WS handshake")?;
    let (headers, mut buffered) = read_until_headers(&mut stream, 64 * 1024)?;
    let text = std::str::from_utf8(&headers).context("WS headers UTF-8")?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().context("missing WS status line")?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().context("missing WS version")?;
    ensure!(version.starts_with("HTTP/"), "bad WS status line");
    let status = parts
        .next()
        .context("missing WS status")?
        .parse::<u16>()
        .context("bad WS status")?;
    let reason = parts.next().unwrap_or("").to_string();
    let mut map = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            map.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    if status == 101 {
        ensure!(
            map.get("upgrade")
                .map(|value| value.eq_ignore_ascii_case("websocket"))
                .unwrap_or(false),
            "101 response is missing Upgrade: websocket"
        );
        return Ok(WsHandshake {
            status,
            reason,
            headers: map,
            body: Vec::new(),
            socket: Some(WsSocket { stream, buffered }),
        });
    }
    let mut body = buffered.split_off(0);
    // A gate rejection is a normal HTTP response with Content-Length; do not
    // wait for EOF, because an upgrade request may leave the connection open
    // after the rejection body (this was observed on the real Panel gate).
    let content_length = map
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok());
    if let Some(length) = content_length {
        ensure!(
            length <= MAX_HTTP_BYTES,
            "WS rejection Content-Length too large"
        );
        while body.len() < length {
            let mut buffer = [0_u8; 8192];
            let read = stream.read(&mut buffer).context("read WS rejection body")?;
            ensure!(
                read > 0,
                "connection closed before WS rejection body completed"
            );
            body.extend_from_slice(&buffer[..read]);
        }
        body.truncate(length);
    } else {
        let mut buffer = [0_u8; 8192];
        loop {
            let read = stream.read(&mut buffer).context("read WS rejection body")?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buffer[..read]);
            ensure!(body.len() <= MAX_HTTP_BYTES, "WS rejection body too large");
        }
    }
    let parsed = parse_http_response(&headers, body)?;
    Ok(WsHandshake {
        status: parsed.status,
        reason: parsed.reason,
        headers: parsed.headers,
        body: parsed.body,
        socket: None,
    })
}

impl WsSocket {
    fn read_exact_buffered(&mut self, output: &mut [u8]) -> Result<()> {
        let take = output.len().min(self.buffered.len());
        output[..take].copy_from_slice(&self.buffered[..take]);
        self.buffered.drain(..take);
        if take < output.len() {
            self.stream
                .read_exact(&mut output[take..])
                .context("read WebSocket frame bytes")?;
        }
        Ok(())
    }

    fn read_frame(&mut self) -> Result<(bool, u8, Vec<u8>)> {
        let mut first = [0_u8; 2];
        self.read_exact_buffered(&mut first)?;
        let fin = first[0] & 0x80 != 0;
        let opcode = first[0] & 0x0f;
        let masked = first[1] & 0x80 != 0;
        let length = (first[1] & 0x7f) as usize;
        let payload_len = match length {
            126 => {
                let mut buffer = [0_u8; 2];
                self.read_exact_buffered(&mut buffer)?;
                u16::from_be_bytes(buffer) as usize
            }
            127 => {
                let mut buffer = [0_u8; 8];
                self.read_exact_buffered(&mut buffer)?;
                let value = u64::from_be_bytes(buffer);
                usize::try_from(value).context("WebSocket frame length overflow")?
            }
            value => value,
        };
        ensure!(
            payload_len <= 32 * 1024 * 1024,
            "WebSocket frame too large: {payload_len}"
        );
        let mask = if masked {
            let mut mask = [0_u8; 4];
            self.read_exact_buffered(&mut mask)?;
            Some(mask)
        } else {
            None
        };
        let mut payload = vec![0_u8; payload_len];
        self.read_exact_buffered(&mut payload)?;
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        Ok((fin, opcode, payload))
    }

    fn wait_close(&mut self, timeout: Duration) -> Result<WsClose> {
        self.stream
            .set_read_timeout(Some(timeout))
            .context("set WS read timeout")?;
        loop {
            match self.read_frame() {
                Ok((_fin, 0x8, payload)) => {
                    let (code, reason) = if payload.len() >= 2 {
                        (
                            Some(u16::from_be_bytes([payload[0], payload[1]])),
                            String::from_utf8_lossy(&payload[2..]).to_string(),
                        )
                    } else {
                        (None, String::new())
                    };
                    return Ok(WsClose::Frame { code, reason });
                }
                Ok((_fin, 0x9, _payload)) => {
                    // Ping: the close frame follows shortly; no pong is needed
                    // for this unidirectional verification.
                    continue;
                }
                Ok((_fin, 0xA, _payload)) => continue,
                Ok((_fin, _opcode, _payload)) => continue,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .map(|io| {
                            matches!(
                                io.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            )
                        })
                        .unwrap_or(false) =>
                {
                    return Ok(WsClose::StillOpen);
                }
                Err(error) => {
                    let text = format!("{error:#}");
                    if text.contains("failed to fill whole buffer")
                        || text.contains("connection reset")
                        || text.contains("Broken pipe")
                        || text.contains("UnexpectedEof")
                    {
                        return Ok(WsClose::Eof);
                    }
                    return Err(error).context("read WebSocket close");
                }
            }
        }
    }
}

enum WsClose {
    Frame { code: Option<u16>, reason: String },
    Eof,
    StillOpen,
}

impl WsClose {
    fn closed(&self) -> bool {
        !matches!(self, WsClose::StillOpen)
    }
    fn describe(&self) -> Value {
        match self {
            WsClose::Frame { code, reason } => json!({
                "kind": "close_frame",
                "code": code,
                "reason": reason,
            }),
            WsClose::Eof => json!({"kind": "tcp_eof"}),
            WsClose::StillOpen => json!({"kind": "still_open"}),
        }
    }
}

// ---------------------------------------------------------------------------
// journal evidence
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct JournalEvent {
    ts_us: u64,
    unit: Option<String>,
    cgroup: Option<String>,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleKind {
    Start,
    Stop,
}

fn journal_value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(values) => {
            let mut bytes = Vec::with_capacity(values.len());
            for value in values {
                bytes.push(value.as_u64()? as u8);
            }
            Some(String::from_utf8_lossy(&bytes).to_string())
        }
        _ => None,
    }
}

fn read_journal(units: &[&str], since_epoch: i64, until_epoch: i64) -> Result<Vec<JournalEvent>> {
    let mut command = Command::new("/usr/bin/journalctl");
    command.args([
        "--user",
        "--no-pager",
        "-o",
        "json",
        "--since",
        &format!("@{since_epoch}"),
        "--until",
        &format!("@{until_epoch}"),
    ]);
    for unit in units {
        command.arg("-u").arg(unit);
    }
    let result = run_bounded(command, Duration::from_secs(30), "journalctl --user")?;
    ensure!(result.success(), "journalctl failed:\n{}", result.text());
    let mut events = Vec::new();
    for line in String::from_utf8_lossy(&result.stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts_us = value["__REALTIME_TIMESTAMP"]
            .as_str()
            .and_then(|text| text.parse::<u64>().ok())
            .or_else(|| value["__REALTIME_TIMESTAMP"].as_u64())
            .unwrap_or(0);
        let unit = value["USER_UNIT"]
            .as_str()
            .or_else(|| value["_SYSTEMD_USER_UNIT"].as_str())
            .or_else(|| value["_SYSTEMD_UNIT"].as_str())
            .map(str::to_string);
        // USER_UNIT is manager-reported lifecycle metadata. Its emitter is
        // init.scope, not the service process; preserve it for ordering but
        // do not attach the manager cgroup as process-membership evidence.
        let cgroup = if value["USER_UNIT"].as_str().is_some() {
            None
        } else {
            value["_SYSTEMD_CGROUP"].as_str().map(str::to_string)
        };
        let message = value
            .get("MESSAGE")
            .and_then(journal_value_text)
            .unwrap_or_default();
        events.push(JournalEvent {
            ts_us,
            unit,
            cgroup,
            message,
        });
    }
    events.sort_by_key(|event| event.ts_us);
    Ok(events)
}

fn lifecycle_kind(message: &str) -> Option<LifecycleKind> {
    let message = message.trim();
    if message.starts_with("Starting ") || message.starts_with("Started ") {
        Some(LifecycleKind::Start)
    } else if message.starts_with("Stopping ") || message.starts_with("Stopped ") {
        Some(LifecycleKind::Stop)
    } else {
        None
    }
}

fn verify_lifecycle_order(
    events: &[JournalEvent],
    unit: &str,
    kind: LifecycleKind,
) -> Vec<(u64, String)> {
    events
        .iter()
        .filter(|event| event.unit.as_deref() == Some(unit))
        .filter(|event| lifecycle_kind(&event.message) == Some(kind))
        .map(|event| (event.ts_us, event.message.clone()))
        .collect()
}

fn order_evidence(
    events: &[JournalEvent],
    core_unit: &str,
    panel_unit: &str,
    label: &str,
    stop: bool,
) -> Result<Value> {
    let kind = if stop {
        LifecycleKind::Stop
    } else {
        LifecycleKind::Start
    };
    let core_events = verify_lifecycle_order(events, core_unit, kind);
    let panel_events = verify_lifecycle_order(events, panel_unit, kind);
    ensure!(
        !core_events.is_empty() && !panel_events.is_empty(),
        "{label}: journal has no lifecycle evidence for both units (core={} panel={})",
        core_events.len(),
        panel_events.len()
    );
    let core_min = core_events.iter().map(|event| event.0).min().unwrap();
    let core_max = core_events.iter().map(|event| event.0).max().unwrap();
    let panel_min = panel_events.iter().map(|event| event.0).min().unwrap();
    let panel_max = panel_events.iter().map(|event| event.0).max().unwrap();
    // A window can contain more than one stop/start cycle (a failed bad-Panel
    // attempt followed by its rollback). The *last* complete cycle is the one
    // whose order is contractual: use the last event of each unit and require
    // strict ordering. This still rejects a Core stop before Panel or a Panel
    // start before Core in the final cycle.
    if stop {
        ensure!(
            panel_max < core_max,
            "{label}: expected Panel stop before Core, got panel=[{panel_min},{panel_max}] core=[{core_min},{core_max}]"
        );
    } else {
        ensure!(
            core_max < panel_max,
            "{label}: expected Core start before Panel, got core=[{core_min},{core_max}] panel=[{panel_min},{panel_max}]"
        );
    }
    Ok(json!({
        "label": label,
        "stop": stop,
        "core_timestamps_us": core_events,
        "panel_timestamps_us": panel_events,
        "core_range_us": [core_min, core_max],
        "panel_range_us": [panel_min, panel_max],
        "ordering_rule": "last event of each unit in window",
    }))
}

fn updater_journal_evidence(events: &[JournalEvent], updater_unit: &str) -> Result<Value> {
    let entries: Vec<&JournalEvent> = events
        .iter()
        .filter(|event| event.unit.as_deref() == Some(updater_unit))
        // Manager lifecycle messages identify USER_UNIT but are emitted from
        // init.scope. Only process-origin messages prove updater membership.
        .filter(|event| lifecycle_kind(&event.message).is_none())
        .collect();
    ensure!(
        !entries.is_empty(),
        "no journal entries for independent updater unit {updater_unit}"
    );
    let mut cgroups = Vec::new();
    for entry in &entries {
        if let Some(cgroup) = &entry.cgroup {
            cgroups.push(cgroup.clone());
        }
    }
    ensure!(
        !cgroups.is_empty(),
        "updater journal entries have no cgroup evidence"
    );
    for cgroup in &cgroups {
        ensure!(
            cgroup.contains(updater_unit),
            "updater journal entry is not in its own cgroup: {cgroup}"
        );
        ensure!(
            !cgroup.contains("echo-agent-core.service")
                && !cgroup.contains("echo-agent-panel.service"),
            "updater journal entry overlaps a Core/Panel cgroup: {cgroup}"
        );
    }
    Ok(json!({
        "unit": updater_unit,
        "entries": entries.len(),
        "cgroups": cgroups,
        "independent": true,
    }))
}

// ---------------------------------------------------------------------------
// main exercise
// ---------------------------------------------------------------------------

fn assert_transaction_phase(record: &Value, expected: &str, expected_target: &str) -> Result<()> {
    ensure!(
        record["phase"] == json!(expected),
        "transaction phase is {:?}, expected {expected}: {record}",
        record["phase"]
    );
    ensure!(
        record["target_release"] == json!(expected_target),
        "transaction target release mismatch: {record}"
    );
    Ok(())
}

fn exercise(live: &mut Live, manifest: &Manifest) -> Result<Value> {
    let total_started = Instant::now();
    let mut evidence = Map::new();
    evidence.insert(
        "boundary".into(),
        json!({
            "core_binary": live.core_binary,
            "panel_binary": live.panel_binary,
            "core_root": manifest.core_root,
            "panel_root": manifest.panel_root,
            "instance": live.instance,
            "management_port": live.management_port(),
            "panel_port": live.panel_port(),
            "web": "r1/r2 index.html stubs only; byte-level acceptance, not browser frontend acceptance",
            "health": "Panel has no dedicated /health route in this build; /health is classified, never claimed as a health endpoint",
            "proc": "independent /proc/<pid>/exe hash; permission failure is a failure, never PR_SET_PTRACER",
        }),
    );

    let core_help = check_binary_help(
        &live.core_binary,
        "real Core",
        &live.root,
        manifest.help_timeout(),
    )?;
    let panel_help = check_binary_help(
        &live.panel_binary,
        "real Panel",
        &live.root,
        manifest.help_timeout(),
    )?;
    evidence.insert(
        "help".into(),
        json!({
            "core_help_bytes": core_help.len(),
            "panel_help_bytes": panel_help.len(),
        }),
    );

    let bundles = build_bundles(live, manifest)?;
    stage_releases(live, manifest, &bundles)?;
    let r1 = release_info(
        live,
        &manifest.release_r1,
        &fixtures_dir().join(&manifest.r1_index),
    )?;
    let r2 = release_info(
        live,
        &manifest.release_r2,
        &fixtures_dir().join(&manifest.r2_index),
    )?;
    let bad = release_info(
        live,
        &manifest.release_r2_bad,
        &fixtures_dir().join(&manifest.r2_index),
    )?;
    evidence.insert(
        "staged".into(),
        json!({
            "r1": {"manifest_sha256": r1.manifest_sha256, "core_sha256": r1.core_sha256, "panel_sha256": r1.panel_sha256},
            "r2": {"manifest_sha256": r2.manifest_sha256, "core_sha256": r2.core_sha256, "panel_sha256": r2.panel_sha256},
            "r2_bad": {"manifest_sha256": bad.manifest_sha256, "core_sha256": bad.core_sha256, "panel_sha256": bad.panel_sha256},
        }),
    );

    // ---- Phase 1: first install via systemd-apply -------------------------
    let first_started = Instant::now();
    ensure!(
        live.selector_target()?.is_none(),
        "first install must start without a current selector"
    );
    let (first_output, first_inflight) = run_first_install_with_probe(live, manifest)?;
    if !first_output.success() {
        let stdout_text = String::from_utf8_lossy(&first_output.stdout);
        if stdout_text.contains("/proc/<pid>/exe")
            && (stdout_text.contains("Permission denied")
                || stdout_text.contains("permission denied"))
        {
            let record: Value = serde_json::from_slice(&first_output.stdout).unwrap_or(Value::Null);
            let phase = record["phase"].as_str().unwrap_or("<missing>");
            eprintln!(
                "[standalone-live] BOUNDARY: /proc/<pid>/exe for a systemd-managed Core/Panel \
                 is not readable from the separate runtime adapter on this host. The real \
                 transaction cannot verify the running image and ended in phase {phase}. \
                 This is an environment failure, not a pass; no PR_SET_PTRACER or host \
                 ptrace policy change was attempted."
            );
            bail!(
                "PROC_PERMISSION_BOUNDARY: real systemd-managed Core/Panel /proc/<pid>/exe \
                 is not readable (Yama ptrace_scope / non-ancestor adapter). Required real \
                 running-image verification cannot pass on this host. transaction_phase={phase} \
                 transaction_error={} inflight_panel_probe={} systemd-apply output:\n{}\n{}",
                record["error"],
                first_inflight,
                first_output.text(),
                live.all_tails(120)
            );
        }
        bail!(
            "first install systemd-apply failed (inflight_panel_probe={}):\n{}\n{}",
            first_inflight,
            first_output.text(),
            live.all_tails(120)
        );
    }
    let first_record: Value = serde_json::from_slice(&first_output.stdout)
        .context("first install stdout is not the transaction JSON")?;
    assert_transaction_phase(&first_record, "committed", &manifest.release_r1)?;
    ensure!(
        first_record["previous_release"].is_null(),
        "first install must have no previous release: {first_record}"
    );
    live.assert_selector(&manifest.release_r1)?;
    let first_core_pid = unit_check_active(&live.core_unit)?;
    let first_panel_pid = unit_check_active(&live.panel_unit)?;
    ensure!(
        first_core_pid != first_panel_pid,
        "Core and Panel must be distinct processes"
    );
    let first_core_probe = probe_process_image(&live.core_unit, &r1.core_sha256)?;
    let first_panel_probe = probe_process_image(&live.panel_unit, &r1.panel_sha256)?;
    let first_status = wait_for_http_serving(
        live.panel_port(),
        &manifest.release_r1,
        manifest.startup_timeout(),
    )?;
    assert_deployment_status(
        &first_status,
        &live.instance,
        &manifest.release_r1,
        &r1.management_protocol,
        false,
    )?;
    let first_index = assert_index_bytes(
        live.panel_port(),
        "/index.html",
        &r1.index_bytes,
        "first-install r1",
    )?;
    let first_index_no_extension = assert_index_bytes(
        live.panel_port(),
        "/index",
        &r1.index_bytes,
        "first-install r1 SPA route",
    )?;
    let first_health = probe_health(live.panel_port(), &r1.index_bytes)?;
    let first_gate = live.gate_value()?;
    ensure!(
        first_gate["maintenance"] == json!(false)
            && first_gate["release_id"] == json!(&manifest.release_r1),
        "gate file after first install is wrong: {first_gate}"
    );
    let first_evidence_path = live.state_dir.join("first-install-evidence.json");
    ensure!(
        first_evidence_path.is_file(),
        "first-install evidence was not persisted"
    );
    let first_evidence = read_json(&first_evidence_path)?;

    evidence.insert(
        "first_install".into(),
        json!({
            "elapsed_seconds": first_started.elapsed().as_secs_f64(),
            "transaction": first_record,
            "selector": live.selector_target()?,
            "core": {"pid": first_core_probe.pid, "exe": first_core_probe.exe_path, "sha256": first_core_probe.sha256, "start_time": first_core_probe.start_time},
            "panel": {"pid": first_panel_probe.pid, "exe": first_panel_probe.exe_path, "sha256": first_panel_probe.sha256, "start_time": first_panel_probe.start_time},
            "deployment_status": first_status,
            "index_html": first_index,
            "index_no_extension": first_index_no_extension,
            "health": first_health,
            "gate": first_gate,
            "first_install_evidence": first_evidence,
            "inflight_panel_probe": first_inflight,
        }),
    );
    eprintln!(
        "[standalone-live] first install committed r1 (core pid {}, panel pid {}) in {:.1}s",
        first_core_pid,
        first_panel_pid,
        first_started.elapsed().as_secs_f64()
    );

    // ---- Phase 2: maintenance gate + existing WebSocket close -------------
    let ws_before = ws_handshake(live.panel_port(), Duration::from_secs(10))?;
    ensure!(
        ws_before.status == 101,
        "WS handshake before maintenance did not upgrade: {} {}",
        ws_before.status,
        String::from_utf8_lossy(&ws_before.body)
    );
    let mut ws_before_socket = ws_before.socket.context("101 without a socket")?;
    live.set_gate_maintenance(true)?;
    let maintenance_status =
        wait_until_deployment_state(live.panel_port(), "maintenance", Duration::from_secs(15))?;
    assert_deployment_status(
        &maintenance_status,
        &live.instance,
        &manifest.release_r1,
        &r1.management_protocol,
        true,
    )?;
    let ws_during = ws_handshake(live.panel_port(), Duration::from_secs(5))?;
    ensure!(
        ws_during.status == 503,
        "new WS handshake during maintenance must be rejected, got {}",
        ws_during.status
    );
    let maintenance_index = http_get(live.panel_port(), "/index.html", Duration::from_secs(5))?;
    ensure!(
        maintenance_index.status == 503,
        "static index during maintenance must be 503, got {}",
        maintenance_index.status
    );
    let ws_close = ws_before_socket.wait_close(Duration::from_secs(10))?;
    ensure!(
        ws_close.closed(),
        "existing WebSocket was not closed after maintenance flip: {}",
        ws_close.describe()
    );
    live.set_gate_maintenance(false)?;
    let serving_again =
        wait_until_deployment_state(live.panel_port(), "serving", Duration::from_secs(15))?;
    assert_deployment_status(
        &serving_again,
        &live.instance,
        &manifest.release_r1,
        &r1.management_protocol,
        false,
    )?;
    let ws_after_flip = ws_handshake(live.panel_port(), Duration::from_secs(10))?;
    ensure!(
        ws_after_flip.status == 101,
        "WS handshake after maintenance release did not upgrade: {}",
        ws_after_flip.status
    );
    evidence.insert(
        "maintenance_gate".into(),
        json!({
            "maintenance_status": maintenance_status,
            "new_ws_status": ws_during.status,
            "new_ws_body": String::from_utf8_lossy(&ws_during.body).to_string(),
            "static_index_status": maintenance_index.status,
            "existing_ws_close": ws_close.describe(),
            "existing_ws_closed": ws_close.closed(),
            "serving_again": serving_again,
            "ws_after_release": ws_after_flip.status,
        }),
    );
    eprintln!(
        "[standalone-live] maintenance gate observed; existing WS close = {}",
        ws_close.describe()
    );

    // ---- Phase 3: upgrade r2 through the independent updater unit ---------
    let upgrade_started_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let request_path = live.write_update_request(&manifest.release_r2, "standalone-upgrade-r2")?;
    ensure!(
        request_path == live.state_dir.join("update-request.json"),
        "update request path does not match the rendered updater unit contract"
    );
    let ws_upgrade = ws_handshake(live.panel_port(), Duration::from_secs(10))?;
    ensure!(
        ws_upgrade.status == 101,
        "WS handshake before upgrade did not upgrade: {}",
        ws_upgrade.status
    );
    let mut ws_upgrade_socket = ws_upgrade.socket.context("101 without a socket")?;
    let observer = start_http_observer(
        live.panel_port(),
        &manifest.release_r1,
        &manifest.release_r2,
    );
    let upgrade_command = live.start_updater(manifest.transaction_timeout())?;
    let observations = observer.stop();
    ensure!(
        upgrade_command.success(),
        "upgrade apply-request unit failed:\n{}\n{}\n{}",
        upgrade_command.text(),
        live.all_tails(160),
        serde_json::to_string_pretty(&live.read_transaction().unwrap_or(Value::Null))?
    );

    ensure!(
        observations["saw_maintenance"] == json!(true),
        "upgrade observer did not observe Panel maintenance: {observations}"
    );
    ensure!(
        observations["saw_after"] == json!(true),
        "upgrade observer did not observe r2 serving after maintenance: {observations}"
    );
    ensure!(
        observations["maintenance_ws_probed"] == json!(true),
        "upgrade observer did not probe a new WS during maintenance: {observations}"
    );
    let maintenance_ws_rejected = observations["observations"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .any(|entry| entry["maintenance_ws_status"] == json!(503))
        })
        .unwrap_or(false);
    ensure!(
        maintenance_ws_rejected,
        "upgrade observer never saw a 503 WS handshake during maintenance: {observations}"
    );
    let upgrade_record = live.read_transaction()?;
    assert_transaction_phase(&upgrade_record, "committed", &manifest.release_r2)?;
    ensure!(
        upgrade_record["previous_release"] == json!(&manifest.release_r1),
        "upgrade previous release mismatch: {upgrade_record}"
    );
    live.assert_selector(&manifest.release_r2)?;
    let second_core_pid = unit_check_active(&live.core_unit)?;
    let second_panel_pid = unit_check_active(&live.panel_unit)?;
    ensure!(
        second_core_pid != first_core_pid && second_panel_pid != first_panel_pid,
        "upgrade did not replace the Core/Panel processes"
    );
    let second_core_probe = probe_process_image(&live.core_unit, &r2.core_sha256)?;
    let second_panel_probe = probe_process_image(&live.panel_unit, &r2.panel_sha256)?;
    let second_status = wait_for_http_serving(
        live.panel_port(),
        &manifest.release_r2,
        manifest.startup_timeout(),
    )?;
    assert_deployment_status(
        &second_status,
        &live.instance,
        &manifest.release_r2,
        &r2.management_protocol,
        false,
    )?;
    let second_index = assert_index_bytes(
        live.panel_port(),
        "/index.html",
        &r2.index_bytes,
        "upgrade r2",
    )?;
    let second_index_no_extension = assert_index_bytes(
        live.panel_port(),
        "/index",
        &r2.index_bytes,
        "upgrade r2 SPA route",
    )?;
    let second_health = probe_health(live.panel_port(), &r2.index_bytes)?;
    let second_gate = live.gate_value()?;
    ensure!(
        second_gate["maintenance"] == json!(false)
            && second_gate["release_id"] == json!(&manifest.release_r2),
        "gate file after upgrade is wrong: {second_gate}"
    );
    let ws_after_upgrade = ws_handshake(live.panel_port(), Duration::from_secs(10))?;
    ensure!(
        ws_after_upgrade.status == 101,
        "WS handshake after upgrade did not upgrade: {}",
        ws_after_upgrade.status
    );
    let upgrade_ws_close = ws_upgrade_socket.wait_close(Duration::from_secs(10))?;
    ensure!(
        upgrade_ws_close.closed(),
        "pre-upgrade WebSocket was not closed during maintenance: {}",
        upgrade_ws_close.describe()
    );
    let upgrade_until_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let journal = read_journal(
        &[
            live.core_unit.as_str(),
            live.panel_unit.as_str(),
            live.updater_unit.as_str(),
        ],
        upgrade_started_epoch - 2,
        upgrade_until_epoch + 2,
    )?;
    let stop_order = order_evidence(
        &journal,
        &live.core_unit,
        &live.panel_unit,
        "upgrade stop order",
        true,
    )?;
    let start_order = order_evidence(
        &journal,
        &live.core_unit,
        &live.panel_unit,
        "upgrade start order",
        false,
    )?;
    let updater_evidence = updater_journal_evidence(&journal, &live.updater_unit)?;
    let updater_result = systemctl_show(
        &live.updater_unit,
        &["ActiveState", "Result", "ExecMainStatus", "MainPID"],
    )?;
    ensure!(
        updater_result.get("Result").map(String::as_str) == Some("success"),
        "updater unit did not finish successfully: {updater_result:?}"
    );
    evidence.insert(
        "upgrade".into(),
        json!({
            "elapsed_seconds": upgrade_started_epoch,
            "transaction": upgrade_record,
            "selector": live.selector_target()?,
            "core": {"pid": second_core_probe.pid, "exe": second_core_probe.exe_path, "sha256": second_core_probe.sha256, "start_time": second_core_probe.start_time},
            "panel": {"pid": second_panel_probe.pid, "exe": second_panel_probe.exe_path, "sha256": second_panel_probe.sha256, "start_time": second_panel_probe.start_time},
            "deployment_status": second_status,
            "index_html": second_index,
            "index_no_extension": second_index_no_extension,
            "health": second_health,
            "gate": second_gate,
            "ws_after_upgrade_status": ws_after_upgrade.status,
            "pre_upgrade_ws_close": upgrade_ws_close.describe(),
            "updater_unit": updater_result,
            "updater_journal": updater_evidence,
            "stop_order": stop_order,
            "start_order": start_order,
            "observer": observations,
        }),
    );
    eprintln!(
        "[standalone-live] upgrade committed r2 (core pid {}, panel pid {}); stop/start order evidenced from journal",
        second_core_pid, second_panel_pid
    );

    // ---- Phase 4: bad Panel image must be rejected and rolled back --------
    let bad_started_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    live.write_update_request(&manifest.release_r2_bad, "standalone-bad-panel")?;
    let bad_command = live.start_updater(manifest.transaction_timeout())?;
    ensure!(
        !bad_command.success(),
        "bad Panel updater unexpectedly succeeded:\n{}\n{}",
        bad_command.text(),
        live.all_tails(160)
    );
    let bad_record = live.read_transaction()?;
    assert_transaction_phase(&bad_record, "rolled_back", &manifest.release_r2_bad)?;
    ensure!(
        bad_record["previous_release"] == json!(&manifest.release_r2),
        "bad Panel transaction previous release mismatch: {bad_record}"
    );
    let bad_error = bad_record
        .get("error")
        .context("rolled_back transaction must record an error")?;
    let bad_step = bad_error["step"].as_str().unwrap_or("");
    ensure!(
        matches!(
            bad_step,
            "resume" | "verify_readiness" | "switch" | "maintenance" | "drain" | "prepare"
        ),
        "bad Panel rollback has an unexpected error step {bad_step:?}: {bad_record}"
    );
    live.assert_selector(&manifest.release_r2)?;
    let rollback_core_pid = unit_check_active(&live.core_unit)?;
    let rollback_panel_pid = unit_check_active(&live.panel_unit)?;
    let rollback_core_probe = probe_process_image(&live.core_unit, &r2.core_sha256)?;
    let rollback_panel_probe = probe_process_image(&live.panel_unit, &r2.panel_sha256)?;
    let rollback_status = wait_for_http_serving(
        live.panel_port(),
        &manifest.release_r2,
        manifest.startup_timeout(),
    )?;
    assert_deployment_status(
        &rollback_status,
        &live.instance,
        &manifest.release_r2,
        &r2.management_protocol,
        false,
    )?;
    let rollback_index = assert_index_bytes(
        live.panel_port(),
        "/index.html",
        &r2.index_bytes,
        "bad-panel rollback r2",
    )?;
    let bad_until_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let bad_journal = read_journal(
        &[live.core_unit.as_str(), live.panel_unit.as_str()],
        bad_started_epoch - 2,
        bad_until_epoch + 2,
    )?;
    let bad_start_order = order_evidence(
        &bad_journal,
        &live.core_unit,
        &live.panel_unit,
        "bad-panel rollback start order",
        false,
    )?;
    let updater_result = systemctl_show(
        &live.updater_unit,
        &["ActiveState", "Result", "ExecMainStatus", "MainPID"],
    )?;
    ensure!(
        updater_result.get("Result").map(String::as_str) != Some("success"),
        "bad Panel updater unit must fail, got {updater_result:?}"
    );
    evidence.insert(
        "bad_panel_rollback".into(),
        json!({
            "elapsed_seconds": bad_started_epoch,
            "updater_command": bad_command.text(),
            "updater_unit": updater_result,
            "transaction": bad_record,
            "error_step": bad_step,
            "error_detail": bad_error,
            "selector": live.selector_target()?,
            "core": {"pid": rollback_core_probe.pid, "sha256": rollback_core_probe.sha256},
            "panel": {"pid": rollback_panel_probe.pid, "sha256": rollback_panel_probe.sha256},
            "deployment_status": rollback_status,
            "index_html": rollback_index,
            "start_order": bad_start_order,
        }),
    );
    eprintln!(
        "[standalone-live] bad Panel rejected at step {bad_step} and rolled back to r2 (core pid {}, panel pid {})",
        rollback_core_pid, rollback_panel_pid
    );

    evidence.insert(
        "total_elapsed_seconds".into(),
        json!(total_started.elapsed().as_secs_f64()),
    );
    ensure!(
        total_started.elapsed() <= manifest.total_timeout(),
        "standalone live test exceeded its total budget of {:?}",
        manifest.total_timeout()
    );
    Ok(Value::Object(evidence))
}

fn wait_until_deployment_state(port: u16, state: &str, timeout: Duration) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    let mut last;
    loop {
        match http_get(port, "/api/deployment", Duration::from_secs(3)) {
            Ok(response) => match serde_json::from_slice::<Value>(&response.body) {
                Ok(value) => {
                    if value["state"] == json!(state) {
                        return Ok(value);
                    }
                    last = format!(
                        "status={} state={} body={}",
                        response.status,
                        value["state"],
                        String::from_utf8_lossy(&response.body)
                    );
                }
                Err(error) => last = format!("invalid JSON: {error}"),
            },
            Err(error) => last = format!("{error:#}"),
        }
        if Instant::now() >= deadline {
            bail!("Panel never reached deployment state {state:?}: {last}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// HTTP maintenance observer for the upgrade window
// ---------------------------------------------------------------------------

struct HttpObserver {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<Value>>,
}

impl HttpObserver {
    fn stop(mut self) -> Value {
        self.stop.store(true, Ordering::SeqCst);
        match self.handle.take() {
            Some(handle) => handle
                .join()
                .unwrap_or_else(|_| json!({"thread": "panicked"})),
            None => Value::Null,
        }
    }
}

fn start_http_observer(port: u16, release_before: &str, release_after: &str) -> HttpObserver {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let release_before = release_before.to_string();
    let release_after = release_after.to_string();
    let observations = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observations_writer = observations.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut saw_maintenance = false;
        let mut maintenance_ws_probed = false;
        let mut saw_after = false;
        while !stop_flag.load(Ordering::SeqCst) && Instant::now() < deadline {
            let mut record = json!({"at_ms": 0});
            match http_get(port, "/api/deployment", Duration::from_secs(2)) {
                Ok(response) => {
                    let value: Value = serde_json::from_slice(&response.body)
                        .unwrap_or_else(|_| json!({"unparsed": String::from_utf8_lossy(&response.body).to_string()}));
                    record["status"] = json!(response.status);
                    record["state"] = value["state"].clone();
                    record["maintenance"] = value["maintenance"].clone();
                    record["release_id"] = value["release_id"].clone();
                    if response.status == 503 && value["state"] == json!("maintenance") {
                        saw_maintenance = true;
                        if !maintenance_ws_probed {
                            maintenance_ws_probed = true;
                            if let Ok(handshake) = ws_handshake(port, Duration::from_secs(3)) {
                                record["maintenance_ws_status"] = json!(handshake.status);
                            }
                            if let Ok(index) = http_get(port, "/index.html", Duration::from_secs(3))
                            {
                                record["maintenance_index_status"] = json!(index.status);
                            }
                        }
                    }
                    if response.status == 200
                        && value["state"] == json!("serving")
                        && value["release_id"] == json!(release_after)
                        && saw_maintenance
                    {
                        saw_after = true;
                    }
                }
                Err(error) => {
                    record["error"] = json!(format!("{error:#}"));
                }
            }
            record["at_ms"] = json!(SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis());
            observations_writer
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(record);
            if saw_after {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let values = observations_writer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        json!({
            "observations": values,
            "saw_maintenance": saw_maintenance,
            "saw_after": saw_after,
            "maintenance_ws_probed": maintenance_ws_probed,
            "release_before": release_before,
            "release_after": release_after,
        })
    });
    HttpObserver {
        stop,
        handle: Some(handle),
    }
}

// ---------------------------------------------------------------------------
// test entry point
// ---------------------------------------------------------------------------

#[test]
#[ignore = "opt-in live acceptance (real Core + real Panel + real user systemd)"]
fn real_standalone_core_panel_user_systemd_live() -> Result<()> {
    let (manifest, _manifest_path) = load_manifest()?;
    if !opted_in(&manifest)? {
        eprintln!(
            "SKIP: {} is not set to 1; refusing to mutate the real user systemd manager",
            manifest.opt_in_env
        );
        return Ok(());
    }
    if !user_systemd_available() {
        eprintln!("SKIP: no reachable user systemd manager");
        return Ok(());
    }
    ensure_not_in_protected_cgroup()?;
    let core_binary = selected_binary(&manifest, "core")?;
    let panel_binary = selected_binary(&manifest, "panel")?;
    eprintln!(
        "[standalone-live] core={} sha256={}",
        core_binary.display(),
        sha256_file(&core_binary)?
    );
    eprintln!(
        "[standalone-live] panel={} sha256={}",
        panel_binary.display(),
        sha256_file(&panel_binary)?
    );

    let mut live = Live::new(&manifest, core_binary, panel_binary)?;
    let body = exercise(&mut live, &manifest);
    let cleanup = live.finish();
    let evidence = match (body, cleanup) {
        (Ok(evidence), Ok(cleanup)) => {
            let mut map = evidence.as_object().cloned().unwrap_or_default();
            map.insert("cleanup".into(), cleanup);
            Value::Object(map)
        }
        (Err(error), Ok(cleanup)) => {
            eprintln!("[standalone-live] FAILURE: {error:#}");
            eprintln!(
                "[standalone-live] cleanup after failure: {}",
                serde_json::to_string_pretty(&cleanup)?
            );
            return Err(error);
        }
        (Ok(_), Err(error)) => {
            eprintln!("[standalone-live] CLEANUP FAILURE: {error:#}");
            return Err(error);
        }
        (Err(error), Err(cleanup_error)) => {
            eprintln!("[standalone-live] FAILURE: {error:#}");
            eprintln!("[standalone-live] CLEANUP FAILURE: {cleanup_error:#}");
            return Err(error.context(format!("cleanup also failed: {cleanup_error:#}")));
        }
    };
    eprintln!(
        "[standalone-live] EVIDENCE\n{}",
        serde_json::to_string_pretty(&evidence)?
    );
    Ok(())
}
