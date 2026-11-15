//! **Opt-in, live** acceptance that a *separate* `systemd` updater unit can
//! verify the real `systemd`-managed Core process, not merely a Core that the
//! test itself spawned as a child.
//!
//! `tests/core_live.rs` starts Core as a child of the test process.  That proves
//! the control protocol and process isolation, but it does **not** prove the
//! deployment permission we actually depend on: the shipping
//! `echo-deploy runtime-adapter`, running in its own transient unit, must be able
//! to open/hash `/proc/<core-pid>/exe` and compare it with the active release
//! manifest.  This file tests exactly that boundary:
//!
//! * the real Core binary (`target/debug/echo-agent-core` under the exact Core
//!   root) is started as a transient user unit
//!   `echo-agent-echo-deploy-test-<random>-core.service` with an isolated
//!   config rendered from `tests/fixtures/core_live.toml`, `env -i`, temp
//!   HOME/XDG/TMPDIR, empty personas, no credentials, QQ disabled, federation
//!   `listen = ""`, and a random loopback management port;
//! * a **different** transient unit
//!   `echo-agent-echo-deploy-test-<random>-<label>.service` runs the real
//!   `echo-deploy runtime-adapter` with a generated `release.json` whose core
//!   artifact is a hardlink to the exact running binary and whose SHA-256/size
//!   are the real values.  The selector points at that release;
//! * the updater unit first runs an independent probe in its own cgroup: it
//!   reads/hashes `/proc/<core-pid>/exe`, records the readlink path, stat
//!   identity, the Core environment it can observe, `ss` listeners and
//!   `kernel.yama.ptrace_scope`; then it `exec`s the real adapter for
//!   `Observe` and `Ready`;
//! * the test twice proves the hash is real: (1) the probe hash must equal the
//!   configured binary SHA-256; (2) a negative-control `Observe` against a
//!   tampered manifest digest must fail with `running executable SHA ... does
//!   not match`.  A success from the adapter alone is not accepted as proof
//!   that it hashed anything;
//! * cleanup stops every transient unit, resets failed state, reloads systemd
//!   and asserts the production Core/Panel `MainPID`, invocation and start time
//!   did not change.
//!
//! The test records the actual permission outcome instead of assuming either
//! side of the long-standing production question.  If the independent updater
//! is denied, the failure is reported with the exact errno (normally EACCES 13
//! from `/proc/<pid>/exe`), the cgroups/pids involved, the Yama scope value and
//! the probe's own errno.  It never calls `PR_SET_PTRACER`, never relaxes Yama,
//! and never accepts the adapter's declared `ECHO_RELEASE_ID` as process proof.
//!
//! Run it as a transient **service** unit (not as a `--scope` child of an
//! already-confined agent shell): a service unit runs in the host user manager's
//! user namespace/AppArmor profile, which is the same execution boundary as the
//! updater units it creates.  `--scope` from a sandboxed shell can inherit the
//! shell's confinement and produce misleading `/proc` denials.
//!
//! ```bash
//! cd /home/evence/.local/share/echo-agent-core/source/tools/echo-deploy
//! unit="echo-deploy-core-systemd-live-runner-$(date +%s)-$$"
//! systemd-run --user --unit="$unit" --collect \
//!   --property=Type=exec \
//!   --property=WorkingDirectory="$PWD" \
//!   --setenv=ECHO_DEPLOY_RUN_CORE_SYSTEMD_LIVE=1 \
//!   --setenv=ECHO_DEPLOY_CORE_BINARY=/home/evence/.local/share/echo-agent-core/source/target/debug/echo-agent-core \
//!   /bin/bash -lc 'cargo test --locked --test core_systemd_live -- --ignored --nocapture --test-threads=1'
//! ```
//!
//! Requirements: `ECHO_DEPLOY_RUN_CORE_SYSTEMD_LIVE=1`, a working
//! `systemd --user` manager, the real Core binary, and permission to read the
//! Core source root for an evidence-only `PR_SET_PTRACER`/`prctl` scan.  Without
//! the opt-in the test prints `SKIP` and does nothing (it is `#[ignore]`d).
//!
//! What this does **not** claim: it is not a published/signed release, not a
//! system-scope or standalone topology test, not a full upgrade/rollback
//! transaction, and it does not touch `echo-agent-<instance>-update.service`.
//! It proves the live `/proc` hash + `Observe`/`Ready` permission path for the
//! temporary user units it creates, on this host, with the exact binary it
//! hashed.  Production readiness still requires the canonical units, release
//! provenance and the other items in `ACCEPTANCE.md`.

use anyhow::{bail, ensure, Context, Result};
use echo_deploy::control_client::{current_euid, query_core, CoreOperation};
use echo_deploy::model::Release;
use echo_deploy::systemd::{ControlOperation, ControlReply, CONTROL_PROTOCOL};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{symlink, MetadataExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const FIXTURE_MANIFEST: &str = "core_systemd_live.manifest.json";
const EXPECTED_CORE_ROOT: &str = "/home/evence/.local/share/echo-agent-core/source";
const FIXED_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const PRIVATE_DIR_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const MIN_REAL_CORE_BYTES: u64 = 10 * 1024 * 1024;
const PROTECTED_UNITS: [&str; 3] = [
    "echo-agent-core.service",
    "echo-agent-panel.service",
    "echo-agent-core-update.service",
];
const SOURCE_SCAN_ROOTS: [&str; 2] = [
    "source/modules/core/src",
    "source/modules/agent-module-self-update/src",
];

// ---------------------------------------------------------------------------
// fixture manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    description: String,
    opt_in_env: String,
    core_binary_env: String,
    core_root: PathBuf,
    core_binary: PathBuf,
    config_template: String,
    release_id: String,
    instance_id: String,
    unit_prefix: String,
    management_host: String,
    provider_url: String,
    required_modules: Vec<String>,
    forbidden_ports: Vec<u16>,
    max_help_seconds: u64,
    max_startup_seconds: u64,
    max_control_seconds: u64,
    max_updater_seconds: u64,
    max_shutdown_seconds: u64,
    max_total_seconds: u64,
    require_systemd_user: bool,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load_manifest() -> Result<(Manifest, PathBuf, PathBuf)> {
    let dir = fixtures_dir();
    let path = dir.join(FIXTURE_MANIFEST);
    ensure!(
        path.is_absolute(),
        "fixture manifest path must be absolute: {}",
        path.display()
    );
    let meta = fs::symlink_metadata(&path)
        .with_context(|| format!("stat fixture manifest {}", path.display()))?;
    ensure!(
        meta.file_type().is_file() && !meta.file_type().is_symlink(),
        "fixture manifest must be a regular non-symlink file: {}",
        path.display()
    );
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    let template = dir.join(&manifest.config_template);
    validate_manifest(&manifest, &path, &template)?;
    Ok((manifest, path, template))
}

fn validate_manifest(manifest: &Manifest, manifest_path: &Path, template: &Path) -> Result<()> {
    ensure!(
        manifest.schema_version == 1,
        "unsupported fixture manifest schema_version {}",
        manifest.schema_version
    );
    ensure!(
        !manifest.description.trim().is_empty(),
        "fixture manifest description must not be empty"
    );
    ensure!(
        manifest.opt_in_env == "ECHO_DEPLOY_RUN_CORE_SYSTEMD_LIVE",
        "opt_in_env must be ECHO_DEPLOY_RUN_CORE_SYSTEMD_LIVE"
    );
    ensure!(
        manifest.core_binary_env == "ECHO_DEPLOY_CORE_BINARY",
        "core_binary_env must be ECHO_DEPLOY_CORE_BINARY"
    );
    ensure!(
        manifest.core_root == Path::new(EXPECTED_CORE_ROOT),
        "core_root must be the exact Core root {EXPECTED_CORE_ROOT}, got {}",
        manifest.core_root.display()
    );
    ensure!(
        manifest.core_root.is_absolute() && manifest.core_binary.is_absolute(),
        "core_root and core_binary must be absolute"
    );
    ensure!(
        manifest.core_binary.starts_with(&manifest.core_root),
        "core_binary {} must be under core_root {}",
        manifest.core_binary.display(),
        manifest.core_root.display()
    );
    ensure!(
        manifest
            .core_binary
            .file_name()
            .and_then(|name| name.to_str())
            == Some("echo-agent-core"),
        "core_binary must be named echo-agent-core: {}",
        manifest.core_binary.display()
    );
    ensure!(
        Path::new(&manifest.config_template).components().count() == 1
            && Path::new(&manifest.config_template)
                .extension()
                .and_then(|ext| ext.to_str())
                == Some("toml"),
        "config_template must be a single .toml file name, got {:?}",
        manifest.config_template
    );
    ensure!(
        manifest.release_id == "core-systemd-live" && manifest.instance_id == manifest.release_id,
        "release_id/instance_id must be core-systemd-live"
    );
    ensure!(
        safe_systemd_id(&manifest.release_id) && safe_systemd_id(&manifest.instance_id),
        "release_id/instance_id must be safe identifiers"
    );
    ensure!(
        manifest.unit_prefix == "echo-agent-echo-deploy-test",
        "unit_prefix must be echo-agent-echo-deploy-test"
    );
    ensure!(
        manifest.management_host == "127.0.0.1",
        "management_host must be 127.0.0.1"
    );
    ensure!(
        manifest.provider_url == "http://127.0.0.1:9",
        "provider_url must be http://127.0.0.1:9"
    );
    ensure!(
        !manifest.required_modules.is_empty()
            && manifest
                .required_modules
                .iter()
                .all(|module| safe_systemd_id(module)),
        "required_modules must be a non-empty list of safe identifiers"
    );
    ensure!(
        manifest.forbidden_ports.contains(&3132) && manifest.forbidden_ports.contains(&3133),
        "forbidden_ports must contain 3132 and 3133"
    );
    for (label, seconds) in [
        ("max_help_seconds", manifest.max_help_seconds),
        ("max_startup_seconds", manifest.max_startup_seconds),
        ("max_control_seconds", manifest.max_control_seconds),
        ("max_updater_seconds", manifest.max_updater_seconds),
        ("max_shutdown_seconds", manifest.max_shutdown_seconds),
        ("max_total_seconds", manifest.max_total_seconds),
    ] {
        ensure!(
            (1..=600).contains(&seconds),
            "{label} must be in 1..=600 seconds"
        );
    }
    ensure!(
        manifest.max_total_seconds >= manifest.max_startup_seconds + manifest.max_updater_seconds,
        "max_total_seconds must cover startup plus one updater run"
    );
    ensure!(
        manifest.require_systemd_user,
        "require_systemd_user must be true"
    );
    for path in [manifest_path, template] {
        ensure!(
            path.is_absolute(),
            "fixture path must be absolute: {}",
            path.display()
        );
        let meta = fs::symlink_metadata(path)
            .with_context(|| format!("stat fixture path {}", path.display()))?;
        ensure!(
            meta.file_type().is_file() && !meta.file_type().is_symlink(),
            "fixture path must be a regular non-symlink file: {}",
            path.display()
        );
    }
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
        Err(env::VarError::NotPresent) => {
            eprintln!(
                "SKIP core_systemd_live: {} is not set (this test starts the real Core and a real updater unit)",
                manifest.opt_in_env
            );
            Ok(false)
        }
        Err(error) => Err(error).with_context(|| format!("read {}", manifest.opt_in_env)),
    }
}

fn selected_core_binary(manifest: &Manifest) -> Result<PathBuf> {
    let requested = match env::var_os(&manifest.core_binary_env) {
        Some(value) => PathBuf::from(value),
        None => manifest.core_binary.clone(),
    };
    ensure!(
        requested.is_absolute(),
        "{} must be an absolute path, got {}",
        manifest.core_binary_env,
        requested.display()
    );
    let binary = fs::canonicalize(&requested)
        .with_context(|| format!("canonicalize Core binary {}", requested.display()))?;
    let core_root = fs::canonicalize(&manifest.core_root)
        .with_context(|| format!("canonicalize Core root {}", manifest.core_root.display()))?;
    ensure!(
        binary.starts_with(&core_root),
        "Core binary {} is outside the exact Core root {}",
        binary.display(),
        core_root.display()
    );
    ensure!(
        binary.file_name().and_then(|name| name.to_str()) == Some("echo-agent-core"),
        "Core binary must be named echo-agent-core, got {}",
        binary.display()
    );
    let fixtures = fs::canonicalize(fixtures_dir())?;
    ensure!(
        !binary.starts_with(&fixtures),
        "refusing to execute a fixture binary as the real Core: {}",
        binary.display()
    );
    let meta = fs::metadata(&binary)?;
    ensure!(meta.is_file(), "Core binary is not a file");
    ensure!(
        meta.len() >= MIN_REAL_CORE_BYTES,
        "Core binary is only {} bytes; refusing a fixture-sized binary",
        meta.len()
    );
    Ok(binary)
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0_u8; bytes];
    if let Ok(mut file) = File::open("/dev/urandom") {
        if file.read_exact(&mut buffer).is_ok() {
            return buffer.iter().map(|byte| format!("{byte:02x}")).collect();
        }
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{nanos:016x}").chars().take(bytes * 2).collect()
}

fn unique_token() -> String {
    format!("{}-{}", std::process::id(), random_hex(8))
}

// ---------------------------------------------------------------------------
// small process / systemd helpers
// ---------------------------------------------------------------------------

fn run_bounded(mut command: Command, timeout: Duration, label: &str) -> Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().with_context(|| format!("spawn {label}"))?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .with_context(|| format!("{label} timeout overflow"))?;
    loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("wait for {label}"))?
        {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut pipe) = child.stdout.take() {
                pipe.read_to_end(&mut stdout)?;
            }
            if let Some(mut pipe) = child.stderr.take() {
                pipe.read_to_end(&mut stderr)?;
            }
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{label} timed out after {timeout:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn render(output: &Output) -> String {
    format!(
        "status={} stdout={:?} stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn systemctl(args: &[&str]) -> Result<Output> {
    let mut command = Command::new("systemctl");
    command.arg("--user").args(args);
    run_bounded(command, Duration::from_secs(30), "systemctl --user")
}

fn systemd_run(args: &[String], timeout: Duration) -> Result<Output> {
    let mut command = Command::new("systemd-run");
    command.arg("--user").args(args);
    run_bounded(command, timeout, "systemd-run --user")
}

fn show_unit(unit: &str, properties: &[&str]) -> Result<BTreeMap<String, String>> {
    let mut args: Vec<String> = vec!["show".into(), "--no-pager".into()];
    for property in properties {
        args.push(format!("--property={property}"));
    }
    args.push(unit.to_string());
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = systemctl(&borrowed)?;
    ensure!(
        output.status.success(),
        "systemctl show {unit} failed: {}",
        render(&output)
    );
    let mut map = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((key, value)) = line.split_once('=') {
            map.entry(key.to_string())
                .or_insert_with(|| value.to_string());
        }
    }
    Ok(map)
}

fn unit_properties(unit: &str) -> Result<BTreeMap<String, String>> {
    show_unit(
        unit,
        &[
            "Id",
            "LoadState",
            "ActiveState",
            "SubState",
            "MainPID",
            "Result",
            "ExecMainStatus",
            "InvocationID",
            "ControlGroup",
            "FragmentPath",
            "Transient",
            "Type",
            "ExecStart",
            "Environment",
            "UMask",
            "WorkingDirectory",
            "Description",
            "ExecMainStartTimestampMonotonic",
        ],
    )
}

fn property_u32(map: &BTreeMap<String, String>, key: &str) -> u32 {
    map.get(key)
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

fn property_i32(map: &BTreeMap<String, String>, key: &str) -> Option<i32> {
    map.get(key).and_then(|value| value.trim().parse().ok())
}

fn property<'a>(map: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    map.get(key).map(String::as_str).unwrap_or("")
}

// ---------------------------------------------------------------------------
// /proc helpers
// ---------------------------------------------------------------------------

fn read_proc_cgroup(pid: u32) -> Result<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .with_context(|| format!("read /proc/{pid}/cgroup"))?;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            return Ok(path.to_string());
        }
    }
    bail!("no unified cgroup entry for pid {pid}: {text:?}")
}

fn read_proc_starttime(pid: u32) -> Result<u64> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat"))
        .with_context(|| format!("read /proc/{pid}/stat"))?;
    let close = text
        .rfind(')')
        .with_context(|| format!("/proc/{pid}/stat has no comm terminator"))?;
    let after_comm = &text[close + 1..];
    let field = after_comm
        .split_whitespace()
        .nth(19)
        .with_context(|| format!("/proc/{pid}/stat has no starttime field"))?;
    field
        .parse()
        .with_context(|| format!("/proc/{pid}/stat starttime is not u64: {field:?}"))
}

fn proc_status_field(pid: u32, key: &str) -> Result<Option<String>> {
    let text = fs::read_to_string(format!("/proc/{pid}/status"))
        .with_context(|| format!("read /proc/{pid}/status"))?;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix(&format!("{key}:")) {
            return Ok(Some(value.trim().to_string()));
        }
    }
    Ok(None)
}

fn read_proc_cmdline(pid: u32) -> Result<Vec<String>> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline"))
        .with_context(|| format!("read /proc/{pid}/cmdline"))?;
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect())
}

fn peer_credentials(socket: &Path) -> Result<(u32, u32)> {
    let stream =
        UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the fd is owned by `stream`; the pointer and length are valid.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    ensure!(result == 0, "getsockopt(SO_PEERCRED) failed");
    ensure!(
        length as usize >= std::mem::size_of::<libc::ucred>(),
        "short SO_PEERCRED"
    );
    ensure!(credentials.pid > 0, "SO_PEERCRED returned no pid");
    Ok((credentials.uid, credentials.pid as u32))
}

fn listening_ports() -> Result<BTreeSet<u16>> {
    let mut ports = BTreeSet::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {path}")),
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() > 3 && fields[3] == "0A" {
                if let Some((_, port)) = fields[1].rsplit_once(':') {
                    if let Ok(port) = u16::from_str_radix(port, 16) {
                        ports.insert(port);
                    }
                }
            }
        }
    }
    Ok(ports)
}

fn pids_in_cgroup_containing(token: &str) -> Result<Vec<u32>> {
    let mut pids = Vec::new();
    let entries = fs::read_dir("/proc").context("read /proc")?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(cgroup) = fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
            continue;
        };
        if cgroup.contains(token) {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// ---------------------------------------------------------------------------
// private paths and config
// ---------------------------------------------------------------------------

struct LivePaths {
    root: PathBuf,
    config_dir: PathBuf,
    config_file: PathBuf,
    work: PathBuf,
    skills: PathBuf,
    modules: PathBuf,
    home: PathBuf,
    xdg_config: PathBuf,
    xdg_data: PathBuf,
    xdg_cache: PathBuf,
    xdg_state: PathBuf,
    xdg_runtime: PathBuf,
    tmp: PathBuf,
    control_dir: PathBuf,
    socket: PathBuf,
    core_log: PathBuf,
    state: PathBuf,
    store: PathBuf,
    run: PathBuf,
    wrapper: PathBuf,
    probe: PathBuf,
    evidence: PathBuf,
}

impl LivePaths {
    fn create(root: &Path) -> Result<Self> {
        set_private_dir(root)?;
        let paths = Self {
            root: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/echo-agent-core.toml"),
            work: root.join("work"),
            skills: root.join("skills"),
            modules: root.join("modules"),
            home: root.join("home"),
            xdg_config: root.join("xdg/config"),
            xdg_data: root.join("xdg/data"),
            xdg_cache: root.join("xdg/cache"),
            xdg_state: root.join("xdg/state"),
            xdg_runtime: root.join("xdg/runtime"),
            tmp: root.join("tmp"),
            control_dir: root.join("control"),
            socket: root.join("control/core.sock"),
            core_log: root.join("core.log"),
            state: root.join("state"),
            store: root.join("store"),
            run: root.join("run"),
            wrapper: root.join("run/wrapper.sh"),
            probe: root.join("run/probe.py"),
            evidence: root.join("evidence"),
        };
        for directory in [
            &paths.config_dir,
            &paths.work,
            &paths.skills,
            &paths.modules,
            &paths.home,
            &paths.xdg_config,
            &paths.xdg_data,
            &paths.xdg_cache,
            &paths.xdg_state,
            &paths.xdg_runtime,
            &paths.tmp,
            &paths.control_dir,
            &paths.state,
            &paths.run,
            &paths.evidence,
        ] {
            fs::create_dir_all(directory)
                .with_context(|| format!("create {}", directory.display()))?;
            set_private_dir(directory)?;
        }
        fs::create_dir_all(&paths.store)
            .with_context(|| format!("create {}", paths.store.display()))?;
        set_dir_mode(&paths.store, 0o755)?;
        ensure!(
            paths.socket.as_os_str().as_encoded_bytes().len() < 108,
            "control socket path is too long for sockaddr_un: {}",
            paths.socket.display()
        );
        ensure!(
            paths.control_dir.parent() == Some(root),
            "control directory must be directly under the private root"
        );
        ensure!(
            fs::read_dir(&paths.skills)?.next().is_none(),
            "skills directory must start empty"
        );
        ensure!(
            fs::read_dir(&paths.modules)?.next().is_none(),
            "modules directory must start empty"
        );
        Ok(paths)
    }

    fn updater_file(&self, label: &str, suffix: &str) -> PathBuf {
        self.run.join(format!("{label}.{suffix}"))
    }
}

fn set_private_dir(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIR_MODE))
        .with_context(|| format!("chmod 0700 {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{} must be a real directory",
        path.display()
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "{} must be private (no group/other bits), mode {:o}",
        path.display(),
        metadata.permissions().mode() & 0o777
    );
    Ok(())
}

fn set_dir_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))?;
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

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write {}", path.display()))?;
    file.flush()?;
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
        .with_context(|| format!("chmod 0600 {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file(),
        "{} must be a regular file",
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

fn minimal_environment(paths: &LivePaths) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("PATH".to_string(), FIXED_PATH.to_string()),
        ("HOME".to_string(), paths.home.display().to_string()),
        ("TMPDIR".to_string(), paths.tmp.display().to_string()),
        (
            "XDG_CONFIG_HOME".to_string(),
            paths.xdg_config.display().to_string(),
        ),
        (
            "XDG_DATA_HOME".to_string(),
            paths.xdg_data.display().to_string(),
        ),
        (
            "XDG_CACHE_HOME".to_string(),
            paths.xdg_cache.display().to_string(),
        ),
        (
            "XDG_STATE_HOME".to_string(),
            paths.xdg_state.display().to_string(),
        ),
        (
            "XDG_RUNTIME_DIR".to_string(),
            paths.xdg_runtime.display().to_string(),
        ),
    ])
}

fn core_environment(paths: &LivePaths, release_id: &str) -> BTreeMap<String, String> {
    let mut environment = minimal_environment(paths);
    environment.insert(
        "ECHO_DEPLOY_CONTROL_SOCKET".to_string(),
        paths.socket.display().to_string(),
    );
    environment.insert("ECHO_RELEASE_ID".to_string(), release_id.to_string());
    environment
}

fn choose_loopback_port(forbidden: &[u16]) -> Result<u16> {
    let in_use = listening_ports().unwrap_or_default();
    for _ in 0..64 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("bind ephemeral loopback listener")?;
        let port = listener
            .local_addr()
            .context("read ephemeral loopback port")?
            .port();
        drop(listener);
        if port != 0 && !forbidden.contains(&port) && !in_use.contains(&port) {
            return Ok(port);
        }
    }
    bail!("could not allocate a random loopback port outside {forbidden:?}");
}

fn render_config(template_path: &Path, paths: &LivePaths, port: u16) -> Result<String> {
    let template = fs::read_to_string(template_path)
        .with_context(|| format!("read config template {}", template_path.display()))?;
    ensure!(
        template.matches("@TMP_ROOT@").count() >= 1,
        "config template must contain @TMP_ROOT@"
    );
    ensure!(
        template.matches("@MANAGEMENT_PORT@").count() >= 1,
        "config template must contain @MANAGEMENT_PORT@"
    );
    let rendered = template
        .replace("@TMP_ROOT@", &paths.root.display().to_string())
        .replace("@MANAGEMENT_PORT@", &port.to_string());
    ensure!(
        !rendered.contains("@TMP_ROOT@") && !rendered.contains("@MANAGEMENT_PORT@"),
        "config template placeholders were not fully rendered"
    );
    for (label, needle) in [
        (
            "management loopback bind",
            format!("management_address = \"127.0.0.1:{port}\""),
        ),
        (
            "provider discard endpoint",
            "base_url = \"http://127.0.0.1:9\"".to_string(),
        ),
        ("empty API key", "api_key = \"\"".to_string()),
        (
            "empty management token",
            "management_access_token = \"\"".to_string(),
        ),
        (
            "explicit empty federation listen",
            "listen = \"\"".to_string(),
        ),
        (
            "temp skills directory",
            format!("skills_dir = \"{}/skills\"", paths.root.display()),
        ),
        (
            "temp modules directory",
            format!("modules_dir = \"{}/modules\"", paths.root.display()),
        ),
    ] {
        ensure!(
            rendered.contains(&needle),
            "rendered config is missing {label}: expected {needle:?}"
        );
    }
    ensure!(
        rendered.contains("[adapters.qq]") && rendered.contains("enabled = false"),
        "rendered config must disable the QQ adapter"
    );
    ensure!(
        !rendered
            .lines()
            .any(|line| line.trim_start().starts_with("[agent.teams")),
        "rendered config must not define agent personas"
    );
    ensure!(
        !rendered.contains("/home/evence/.config")
            && !rendered.contains("/home/evence/.local/share/echo-agent-core/config"),
        "rendered config must not reference production config paths"
    );
    Ok(rendered)
}

// ---------------------------------------------------------------------------
// snapshots / report types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct UnitSnapshot {
    unit: String,
    load_state: String,
    active_state: String,
    main_pid: u32,
    invocation_id: String,
    control_group: String,
    start_time: u64,
}

fn snapshot_unit(unit: &str) -> Result<UnitSnapshot> {
    let map = show_unit(
        unit,
        &[
            "LoadState",
            "ActiveState",
            "MainPID",
            "InvocationID",
            "ControlGroup",
        ],
    )?;
    let main_pid = property_u32(&map, "MainPID");
    let start_time = if main_pid > 0 {
        read_proc_starttime(main_pid).unwrap_or(0)
    } else {
        0
    };
    Ok(UnitSnapshot {
        unit: unit.to_string(),
        load_state: property(&map, "LoadState").to_string(),
        active_state: property(&map, "ActiveState").to_string(),
        main_pid,
        invocation_id: property(&map, "InvocationID").to_string(),
        control_group: property(&map, "ControlGroup").to_string(),
        start_time,
    })
}

fn snapshot_protected() -> Result<BTreeMap<String, UnitSnapshot>> {
    let mut snapshots = BTreeMap::new();
    for unit in PROTECTED_UNITS {
        snapshots.insert(unit.to_string(), snapshot_unit(unit)?);
    }
    Ok(snapshots)
}

fn assert_protected_unchanged(
    before: &BTreeMap<String, UnitSnapshot>,
    after: &BTreeMap<String, UnitSnapshot>,
) -> Result<()> {
    for unit in PROTECTED_UNITS {
        let before = before
            .get(unit)
            .with_context(|| format!("missing before snapshot for {unit}"))?;
        let after = after
            .get(unit)
            .with_context(|| format!("missing after snapshot for {unit}"))?;
        ensure!(
            before.main_pid == after.main_pid
                && before.invocation_id == after.invocation_id
                && before.start_time == after.start_time
                && before.active_state == after.active_state
                && before.control_group == after.control_group,
            "production unit {unit} changed: before={before:?} after={after:?}"
        );
        if before.main_pid > 0 {
            ensure!(
                Path::new(&format!("/proc/{}", before.main_pid)).exists(),
                "production pid {} of {unit} is gone",
                before.main_pid
            );
            ensure!(
                read_proc_starttime(before.main_pid)? == before.start_time,
                "production pid {} of {unit} was replaced",
                before.main_pid
            );
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct CleanupEvidence {
    units: Vec<UnitSnapshot>,
    residual_pids: Vec<u32>,
    socket_error: Option<String>,
    daemon_reload: String,
    production_after: BTreeMap<String, UnitSnapshot>,
}

#[derive(Debug, Serialize)]
struct AdapterVerdict {
    ok: bool,
    failure_kind: Option<String>,
    errno: Option<i32>,
    errno_name: Option<String>,
    proc_path: Option<String>,
    mechanism: Option<String>,
    detail: String,
}

#[derive(Debug, Serialize)]
struct UpdaterRun {
    label: String,
    operation: String,
    unit: String,
    request: Value,
    pid: u32,
    main_pid: u32,
    unit_cgroup: String,
    proc_cgroup: String,
    active_state: String,
    sub_state: String,
    result: String,
    exec_main_status: Option<i32>,
    reply: Option<Value>,
    reply_error: Option<String>,
    stderr: String,
    systemd_run_stdout: String,
    systemd_run_stderr: String,
    probe: Option<Value>,
    probe_error: Option<String>,
    ss: String,
    yama_ptrace_scope: String,
    elapsed_ms: u128,
    success: bool,
}

impl UpdaterRun {
    fn stderr_tail(&self, max: usize) -> String {
        tail_text(&self.stderr, max)
    }
}

fn tail_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let start = text.len() - max;
    let start = text[start..]
        .char_indices()
        .next()
        .map(|(offset, _)| start + offset)
        .unwrap_or(start);
    format!("...{}", &text[start..])
}

fn read_text_lossy(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// live fixture
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct LiveRelease {
    release_id: String,
    manifest_sha256: String,
    core_sha256: String,
    core_size: u64,
    release_dir: PathBuf,
    manifest_path: PathBuf,
    selector: PathBuf,
    artifact: PathBuf,
    adapter_config: PathBuf,
}

struct Live {
    _temp: tempfile::TempDir,
    manifest: Manifest,
    manifest_path: PathBuf,
    template_path: PathBuf,
    paths: LivePaths,
    binary: PathBuf,
    binary_sha256: String,
    binary_size: u64,
    core_version: String,
    port: u16,
    token: String,
    core_unit: String,
    core_props: BTreeMap<String, String>,
    core_pid: u32,
    core_cgroup: String,
    core_starttime: u64,
    core_cmdline: Vec<String>,
    core_proc_exe_from_test: Value,
    core_proc_environ_from_test: Value,
    core_status: Value,
    release: Option<LiveRelease>,
    updater_units: Vec<String>,
    production_before: BTreeMap<String, UnitSnapshot>,
    report_dir: PathBuf,
    cleaned: bool,
}

impl Live {
    fn start(
        manifest: Manifest,
        manifest_path: PathBuf,
        template_path: PathBuf,
        production_before: BTreeMap<String, UnitSnapshot>,
    ) -> Result<Self> {
        let temp = tempfile::Builder::new()
            .prefix("echo-deploy-core-systemd-live-")
            .tempdir()
            .context("create private live tempdir")?;
        let paths = LivePaths::create(temp.path())?;
        let binary = selected_core_binary(&manifest)?;
        let binary_sha256 = sha256_file(&binary)?;
        let binary_size = fs::metadata(&binary)?.len();
        let core_version = check_core_help(
            &binary,
            &paths,
            Duration::from_secs(manifest.max_help_seconds),
        )?;
        let port = choose_loopback_port(&manifest.forbidden_ports)?;
        let rendered = render_config(&template_path, &paths, port)?;
        write_private_file(&paths.config_file, rendered.as_bytes())?;
        let token = unique_token();
        let core_unit = format!("{}-{}-core.service", manifest.unit_prefix, token);
        ensure!(
            core_unit.len() <= 255 && safe_systemd_id(core_unit.trim_end_matches(".service")),
            "generated core unit name is not safe/length-bounded: {core_unit}"
        );
        let report_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("core-systemd-live")
            .join(&token);
        fs::create_dir_all(&report_dir)
            .with_context(|| format!("create report directory {}", report_dir.display()))?;
        set_private_dir(&report_dir)?;

        let mut live = Self {
            _temp: temp,
            manifest,
            manifest_path,
            template_path,
            paths,
            binary,
            binary_sha256,
            binary_size,
            core_version,
            port,
            token,
            core_unit,
            core_props: BTreeMap::new(),
            core_pid: 0,
            core_cgroup: String::new(),
            core_starttime: 0,
            core_cmdline: Vec::new(),
            core_proc_exe_from_test: Value::Null,
            core_proc_environ_from_test: Value::Null,
            core_status: Value::Null,
            release: None,
            updater_units: Vec::new(),
            production_before,
            report_dir,
            cleaned: false,
        };
        live.write_scripts()?;
        live.start_core_unit()?;
        live.wait_core_ready()?;
        live.inspect_core()?;
        live.release = Some(live.publish_release()?);
        Ok(live)
    }

    fn write_scripts(&self) -> Result<()> {
        let wrapper = r#"#!/bin/sh
set -u
printf '%s\n' "$$" > "$ECHO_DEPLOY_TEST_PIDFILE"
cat /proc/self/cgroup > "$ECHO_DEPLOY_TEST_CGROUPFILE" 2>/dev/null || true
cat /proc/sys/kernel/yama/ptrace_scope > "$ECHO_DEPLOY_TEST_YAMAFILE" 2>/dev/null || true
/usr/bin/ss -ltnp > "$ECHO_DEPLOY_TEST_SSFILE" 2>&1 || true
/usr/bin/ss -lunp >> "$ECHO_DEPLOY_TEST_SSFILE" 2>&1 || true
if [ -x /usr/bin/python3 ]; then
  /usr/bin/python3 "$ECHO_DEPLOY_TEST_PROBE" "$ECHO_DEPLOY_TEST_CORE_PID" "$ECHO_DEPLOY_TEST_PROBEFILE" >/dev/null 2>&1 || true
else
  printf '%s\n' '{"schema_version":1,"available":false,"reason":"python3 missing"}' > "$ECHO_DEPLOY_TEST_PROBEFILE"
fi
printf '%s\n' done > "$ECHO_DEPLOY_TEST_PROBE_DONE"
while [ ! -e "$ECHO_DEPLOY_TEST_GATE" ]; do sleep 0.02; done
exec "$ECHO_DEPLOY_TEST_ADAPTER" runtime-adapter --config "$ECHO_DEPLOY_TEST_CONFIG"
"#;
        write_private_file(&self.paths.wrapper, wrapper.as_bytes())?;
        fs::set_permissions(&self.paths.wrapper, fs::Permissions::from_mode(0o700))?;

        let probe = r#"import hashlib
import json
import os
import sys


def read_text(path):
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            return handle.read()
    except OSError as error:
        return {"error": error.strerror, "errno": error.errno}


def read_cgroup():
    value = read_text("/proc/self/cgroup")
    if not isinstance(value, str):
        return value
    for line in value.splitlines():
        if line.startswith("0::"):
            return line[3:]
    return value.strip()


def read_environ(pid):
    try:
        data = open("/proc/%d/environ" % pid, "rb").read()
    except OSError as error:
        return {"ok": False, "errno": error.errno, "error": error.strerror}
    values = {}
    for item in data.split(b"\x00"):
        if not item:
            continue
        key, separator, value = item.partition(b"=")
        values[key.decode("utf-8", "replace")] = value.decode("utf-8", "replace")
    return {"ok": True, "values": values}


def read_exe(pid):
    path = "/proc/%d/exe" % pid
    result = {"path": path}
    try:
        result["readlink"] = os.readlink(path)
    except OSError as error:
        result["readlink_error"] = {"errno": error.errno, "error": error.strerror}
    try:
        stat = os.stat(path)
        result["stat"] = {
            "dev": stat.st_dev,
            "ino": stat.st_ino,
            "size": stat.st_size,
            "mode": stat.st_mode,
        }
    except OSError as error:
        result["stat_error"] = {"errno": error.errno, "error": error.strerror}
    try:
        hasher = hashlib.sha256()
        total = 0
        with open(path, "rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                hasher.update(chunk)
                total += len(chunk)
        result["sha256"] = hasher.hexdigest()
        result["bytes"] = total
    except OSError as error:
        result["hash_error"] = {"errno": error.errno, "error": error.strerror}
    return result


def main():
    core_pid = int(sys.argv[1])
    output = sys.argv[2]
    result = {
        "schema_version": 1,
        "core_pid": core_pid,
        "python_pid": os.getpid(),
        "parent_pid": os.getppid(),
        "self_cgroup": read_cgroup(),
        "yama_ptrace_scope": read_text("/proc/sys/kernel/yama/ptrace_scope").strip()
        if isinstance(read_text("/proc/sys/kernel/yama/ptrace_scope"), str)
        else read_text("/proc/sys/kernel/yama/ptrace_scope"),
        "exe": read_exe(core_pid),
        "environ": read_environ(core_pid),
        "cmdline": read_text("/proc/%d/cmdline" % core_pid),
    }
    with open(output, "w", encoding="utf-8") as handle:
        json.dump(result, handle, indent=2, sort_keys=True)


if __name__ == "__main__":
    main()
"#;
        write_private_file(&self.paths.probe, probe.as_bytes())?;
        Ok(())
    }

    fn start_core_unit(&mut self) -> Result<()> {
        let environment = core_environment(&self.paths, &self.manifest.release_id);
        let mut args: Vec<String> = vec![
            format!("--unit={}", self.core_unit),
            "--no-block".to_string(),
            format!(
                "--description=echo-deploy core-systemd-live real Core ({})",
                self.token
            ),
            "--property=Type=exec".to_string(),
            "--property=Restart=no".to_string(),
            format!(
                "--property=TimeoutStartSec={}",
                self.manifest.max_startup_seconds
            ),
            format!(
                "--property=TimeoutStopSec={}",
                self.manifest.max_shutdown_seconds
            ),
            format!(
                "--property=RuntimeMaxSec={}",
                self.manifest.max_total_seconds + 120
            ),
            "--property=KillMode=control-group".to_string(),
            format!("--property=WorkingDirectory={}", self.paths.work.display()),
            format!(
                "--property=StandardOutput=append:{}",
                self.paths.core_log.display()
            ),
            format!(
                "--property=StandardError=append:{}",
                self.paths.core_log.display()
            ),
            "--property=UMask=0077".to_string(),
            "/usr/bin/env".to_string(),
            "-i".to_string(),
        ];
        for (key, value) in &environment {
            args.push(format!("{key}={value}"));
        }
        args.push(self.binary.display().to_string());
        args.push("-c".to_string());
        args.push(self.paths.config_file.display().to_string());
        let output = systemd_run(
            &args,
            Duration::from_secs(self.manifest.max_startup_seconds + 20),
        )?;
        ensure!(
            output.status.success(),
            "systemd-run could not start the Core unit: {}",
            render(&output)
        );
        eprintln!(
            "[core-systemd-live] core unit={} systemd-run={}",
            self.core_unit,
            String::from_utf8_lossy(&output.stdout).trim()
        );
        Ok(())
    }

    fn wait_core_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(self.manifest.max_startup_seconds);
        let mut last;
        loop {
            let props = unit_properties(&self.core_unit)?;
            let active = property(&props, "ActiveState").to_string();
            let load = property(&props, "LoadState").to_string();
            let result = property(&props, "Result").to_string();
            if active == "failed"
                || (active == "inactive" && load != "not-found" && result != "success")
            {
                bail!(
                    "Core unit {} failed before readiness: unit={props:?}\nlog tail:\n{}",
                    self.core_unit,
                    tail_text(&read_text_lossy(&self.paths.core_log), 6000)
                );
            }
            let pid = property_u32(&props, "MainPID");
            if pid > 0 && self.paths.socket.exists() {
                match query_core(
                    &self.paths.socket,
                    CoreOperation::Status,
                    &format!("status-{}", self.token),
                    None,
                    Duration::from_secs(self.manifest.max_control_seconds),
                ) {
                    Ok(status) => {
                        if core_status_is_ready(&status, &self.manifest) {
                            self.core_props = props;
                            self.core_pid = pid;
                            self.core_status = status;
                            return Ok(());
                        }
                        last = format!("status not ready yet: {status}");
                    }
                    Err(error) => last = format!("{error:#}"),
                }
            } else {
                last = format!(
                    "unit active={active} main_pid={pid} socket_exists={}",
                    self.paths.socket.exists()
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out after {:?} waiting for real Core readiness: {last}\nunit={props:?}\nlog tail:\n{}",
                    self.manifest.max_startup_seconds,
                    tail_text(&read_text_lossy(&self.paths.core_log), 6000)
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn inspect_core(&mut self) -> Result<()> {
        let props = unit_properties(&self.core_unit)?;
        ensure!(
            property(&props, "ActiveState") == "active",
            "Core unit is not active: {props:?}"
        );
        ensure!(
            property(&props, "SubState") == "running",
            "Core unit substate is not running: {props:?}"
        );
        ensure!(
            property(&props, "Result") == "success",
            "Core unit result is not success: {props:?}"
        );
        ensure!(
            property(&props, "Type") == "exec",
            "Core unit is not Type=exec: {props:?}"
        );
        ensure!(
            property(&props, "Transient") == "yes",
            "Core unit is not transient: {props:?}"
        );
        ensure!(
            property(&props, "FragmentPath").contains("/run/user/")
                && property(&props, "FragmentPath").contains("/transient/"),
            "Core unit has an unexpected FragmentPath: {}",
            property(&props, "FragmentPath")
        );
        let main_pid = property_u32(&props, "MainPID");
        ensure!(main_pid > 0, "Core unit MainPID is zero: {props:?}");
        ensure!(
            self.core_pid == main_pid,
            "readiness pid {} != systemd MainPID {}",
            self.core_pid,
            main_pid
        );
        let unit_cgroup = property(&props, "ControlGroup").to_string();
        ensure!(
            !unit_cgroup.is_empty() && unit_cgroup.contains(&self.core_unit),
            "Core unit ControlGroup is missing/does not contain the unit: {props:?}"
        );
        let proc_cgroup = read_proc_cgroup(main_pid)?;
        ensure!(
            proc_cgroup == unit_cgroup,
            "Core /proc cgroup {proc_cgroup:?} != systemd ControlGroup {unit_cgroup:?}"
        );
        let (peer_uid, peer_pid) = peer_credentials(&self.paths.socket)?;
        ensure!(
            peer_uid == current_euid(),
            "control socket peer uid {peer_uid} != test euid {}",
            current_euid()
        );
        ensure!(
            peer_pid == main_pid,
            "control socket peer pid {peer_pid} != systemd MainPID {main_pid}"
        );
        let uid_line = proc_status_field(main_pid, "Uid")?.unwrap_or_default();
        ensure!(
            uid_line
                .split_whitespace()
                .all(|value| value == current_euid().to_string()),
            "Core /proc/{main_pid}/status Uid={uid_line:?} does not match euid {}",
            current_euid()
        );
        let name = proc_status_field(main_pid, "Name")?.unwrap_or_default();
        ensure!(
            name == "echo-agent-core",
            "Core process name is {name:?}, expected echo-agent-core"
        );
        let cmdline = read_proc_cmdline(main_pid)?;
        ensure!(
            cmdline.iter().any(|arg| arg == "-c"),
            "Core cmdline has no -c: {cmdline:?}"
        );
        ensure!(
            cmdline
                .iter()
                .any(|arg| arg == &self.paths.config_file.display().to_string()),
            "Core cmdline does not name the isolated config {}: {cmdline:?}",
            self.paths.config_file.display()
        );
        ensure!(
            cmdline
                .iter()
                .any(|arg| arg == &self.binary.display().to_string()),
            "Core cmdline does not name the real binary {}: {cmdline:?}",
            self.binary.display()
        );

        let exec_start = property(&props, "ExecStart");
        ensure!(
            exec_start.contains("path=/usr/bin/env")
                && exec_start.contains("argv[]=/usr/bin/env -i"),
            "Core ExecStart is not an env -i launch: {exec_start}"
        );
        for (key, value) in core_environment(&self.paths, &self.manifest.release_id) {
            ensure!(
                exec_start.contains(&format!("{key}={value}")),
                "Core ExecStart is missing isolated environment {key}={value}: {exec_start}"
            );
        }
        ensure!(
            property(&props, "Environment").trim().is_empty(),
            "Core unit has an unexpected Environment= property: {}",
            property(&props, "Environment")
        );
        ensure!(
            property(&props, "UMask") == "0077",
            "Core unit UMask is not 0077: {}",
            property(&props, "UMask")
        );
        ensure!(
            property(&props, "WorkingDirectory") == self.paths.work.display().to_string(),
            "Core unit WorkingDirectory is not the isolated work dir: {}",
            property(&props, "WorkingDirectory")
        );

        let status = &self.core_status;
        ensure!(
            status["ok"] == json!(true),
            "Core status is not ok: {status}"
        );
        ensure!(
            status["operation"] == json!("status"),
            "Core status operation mismatch: {status}"
        );
        ensure!(
            status["phase"] == json!("idle"),
            "Core status phase is not idle: {status}"
        );
        ensure!(
            status["prepared"] == json!(false) && status["maintenance_held"] == json!(false),
            "Core is unexpectedly prepared/in maintenance: {status}"
        );
        ensure!(
            status["release"]["declared"] == json!(self.manifest.release_id),
            "Core declared release does not match ECHO_RELEASE_ID: {status}"
        );
        ensure!(
            status["release"]["identity_verified"] == json!(false),
            "Core unexpectedly claims identity_verified=true: {status}"
        );
        ensure!(
            status["modules_verified"] == json!(true)
                && status["module_state"]["verified"] == json!(true),
            "Core did not provide verified live module evidence: {status}"
        );
        ensure!(
            status["module_state"]["missing_required"] == json!([]),
            "Core reports missing required modules: {status}"
        );
        let mounted = status["module_state"]["mounted"]
            .as_array()
            .context("Core module_state.mounted is not an array")?;
        let mounted: BTreeSet<&str> = mounted.iter().filter_map(Value::as_str).collect();
        for module in &self.manifest.required_modules {
            ensure!(
                mounted.contains(module.as_str()),
                "Core has not mounted required module {module:?}: {status}"
            );
        }

        let log = read_text_lossy(&self.paths.core_log);
        ensure!(!log.trim().is_empty(), "Core log is empty");
        ensure!(
            log.contains(&format!(
                "path={}/config/echo-node.json",
                self.paths.root.display()
            )),
            "Core log does not show the isolated config directory: tail={}",
            tail_text(&log, 4000)
        );
        ensure!(
            !log.contains("/home/evence/.config")
                && !log.contains("/home/evence/.local/share/echo-agent-core/config"),
            "Core log references production config paths: {}",
            tail_text(&log, 4000)
        );

        self.core_props = props;
        self.core_pid = main_pid;
        self.core_cgroup = proc_cgroup;
        self.core_starttime = read_proc_starttime(main_pid)?;
        self.core_cmdline = cmdline;
        self.core_proc_exe_from_test = proc_readlink_result(main_pid, "exe");
        self.core_proc_environ_from_test = proc_read_result(main_pid, "environ");
        eprintln!(
            "[core-systemd-live] core pid={} cgroup={} status=idle declared={} modules_verified=true",
            self.core_pid, self.core_cgroup, self.manifest.release_id
        );
        Ok(())
    }

    fn publish_release(&self) -> Result<LiveRelease> {
        let release_id = self.manifest.release_id.clone();
        let release_dir = self.paths.store.join(&release_id);
        let bin_dir = release_dir.join("bin");
        fs::create_dir_all(&bin_dir).with_context(|| format!("create {}", bin_dir.display()))?;
        set_dir_mode(&release_dir, 0o755)?;
        set_dir_mode(&bin_dir, 0o755)?;
        let artifact = bin_dir.join("echo-agent-core");
        let mut hardlinked = false;
        match fs::hard_link(&self.binary, &artifact) {
            Ok(()) => hardlinked = true,
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EXDEV) | Some(libc::EPERM) | Some(libc::EACCES)
                ) =>
            {
                fs::copy(&self.binary, &artifact).with_context(|| {
                    format!(
                        "copy Core binary {} -> {}",
                        self.binary.display(),
                        artifact.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "hardlink Core binary {} -> {}",
                        self.binary.display(),
                        artifact.display()
                    )
                })
            }
        }
        fs::set_permissions(&artifact, fs::Permissions::from_mode(0o755))?;
        let artifact_meta = fs::metadata(&artifact)?;
        ensure!(
            artifact_meta.len() == self.binary_size,
            "release artifact size {} != real Core size {}",
            artifact_meta.len(),
            self.binary_size
        );
        if hardlinked {
            let binary_meta = fs::metadata(&self.binary)?;
            ensure!(
                artifact_meta.dev() == binary_meta.dev()
                    && artifact_meta.ino() == binary_meta.ino(),
                "release artifact is not the same inode as the real Core binary"
            );
        } else {
            ensure!(
                sha256_file(&artifact)? == self.binary_sha256,
                "copied release artifact digest differs from the real Core binary"
            );
        }

        let document = json!({
            "schema_version": 1,
            "release_id": release_id,
            "target_os": "linux",
            "target_arch": env::consts::ARCH,
            "management_protocol": "management-contract-1",
            "development": true,
            "sources": [{
                "id": "src",
                "revision": "core-systemd-live-test",
                "snapshot_sha256": "a".repeat(64),
                "lock_sha256": "b".repeat(64)
            }],
            "artifacts": [{
                "id": "core",
                "kind": "core",
                "path": "bin/echo-agent-core",
                "sha256": self.binary_sha256,
                "size": self.binary_size,
                "executable": true,
                "source_id": "src"
            }]
        });
        let bytes = serde_json::to_vec_pretty(&document)?;
        let parsed: Release = serde_json::from_slice(&bytes)
            .context("parse generated release manifest as model::Release")?;
        parsed
            .validate()
            .context("generated release manifest is invalid")?;
        ensure!(
            parsed.release_id == release_id,
            "generated release id mismatch"
        );
        let manifest_sha256 = format!("{:x}", Sha256::digest(&bytes));
        let manifest_path = release_dir.join("release.json");
        write_private_file(&manifest_path, &bytes)?;
        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o444))?;

        let selector = self.paths.state.join("current");
        match fs::remove_file(&selector) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove old selector"),
        }
        symlink(&release_dir, &selector).context("create current selector")?;

        let adapter_config = self.paths.run.join("adapter-config.json");
        let config = json!({
            "instance_id": self.manifest.instance_id,
            "core_socket": self.paths.socket,
            "panel_gate_file": null,
            "instance_state": self.paths.state,
            "current_selector": selector,
            "release_store": self.paths.store,
            "required_modules": [],
            "acknowledge_external_writers_stopped": true,
            "first_install": null
        });
        write_private_file(&adapter_config, &serde_json::to_vec_pretty(&config)?)?;

        eprintln!(
            "[core-systemd-live] release={} manifest_sha256={} core_sha256={} core_size={} artifact_hardlink={}",
            release_id, manifest_sha256, self.binary_sha256, self.binary_size, hardlinked
        );
        Ok(LiveRelease {
            release_id,
            manifest_sha256,
            core_sha256: self.binary_sha256.clone(),
            core_size: self.binary_size,
            release_dir,
            manifest_path,
            selector,
            artifact,
            adapter_config,
        })
    }

    fn run_updater(
        &mut self,
        label: &str,
        operation: ControlOperation,
        release_id: Option<&str>,
        manifest_sha256: Option<&str>,
    ) -> Result<UpdaterRun> {
        let unit = format!(
            "{}-{}-{}.service",
            self.manifest.unit_prefix, self.token, label
        );
        ensure!(
            unit.len() <= 255 && safe_systemd_id(unit.trim_end_matches(".service")),
            "generated updater unit name is not safe/length-bounded: {unit}"
        );
        self.updater_units.push(unit.clone());

        let request = json!({
            "protocol": CONTROL_PROTOCOL,
            "operation": operation,
            "instance": self.manifest.instance_id,
            "release": release_id,
            "manifest_sha256": manifest_sha256
        });
        let request_path = self.paths.updater_file(label, "request.json");
        let reply_path = self.paths.updater_file(label, "reply.json");
        let stderr_path = self.paths.updater_file(label, "stderr");
        let pid_path = self.paths.updater_file(label, "pid");
        let cgroup_path = self.paths.updater_file(label, "cgroup");
        let yama_path = self.paths.updater_file(label, "yama");
        let ss_path = self.paths.updater_file(label, "ss");
        let probe_path = self.paths.updater_file(label, "probe.json");
        let probe_done_path = self.paths.updater_file(label, "probe.done");
        let gate_path = self.paths.updater_file(label, "gate");
        for path in [
            &reply_path,
            &stderr_path,
            &pid_path,
            &cgroup_path,
            &yama_path,
            &ss_path,
            &probe_path,
            &probe_done_path,
            &gate_path,
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("remove {}", path.display()))
                }
            }
        }
        write_private_file(&request_path, &serde_json::to_vec(&request)?)?;

        let mut environment = minimal_environment(&self.paths);
        environment.insert(
            "ECHO_DEPLOY_TEST_PIDFILE".into(),
            pid_path.display().to_string(),
        );
        environment.insert(
            "ECHO_DEPLOY_TEST_CGROUPFILE".into(),
            cgroup_path.display().to_string(),
        );
        environment.insert(
            "ECHO_DEPLOY_TEST_YAMAFILE".into(),
            yama_path.display().to_string(),
        );
        environment.insert(
            "ECHO_DEPLOY_TEST_SSFILE".into(),
            ss_path.display().to_string(),
        );
        environment.insert(
            "ECHO_DEPLOY_TEST_PROBE".into(),
            self.paths.probe.display().to_string(),
        );
        environment.insert(
            "ECHO_DEPLOY_TEST_PROBEFILE".into(),
            probe_path.display().to_string(),
        );
        environment.insert(
            "ECHO_DEPLOY_TEST_PROBE_DONE".into(),
            probe_done_path.display().to_string(),
        );
        environment.insert(
            "ECHO_DEPLOY_TEST_CORE_PID".into(),
            self.core_pid.to_string(),
        );
        environment.insert(
            "ECHO_DEPLOY_TEST_GATE".into(),
            gate_path.display().to_string(),
        );
        environment.insert(
            "ECHO_DEPLOY_TEST_CONFIG".into(),
            self.release
                .as_ref()
                .context("release not published before updater run")?
                .adapter_config
                .display()
                .to_string(),
        );
        environment.insert(
            "ECHO_DEPLOY_TEST_ADAPTER".into(),
            env!("CARGO_BIN_EXE_echo-deploy").to_string(),
        );

        let mut args: Vec<String> = vec![
            format!("--unit={unit}"),
            "--no-block".to_string(),
            format!(
                "--description=echo-deploy core-systemd-live updater {} ({})",
                label, self.token
            ),
            "--property=Type=oneshot".to_string(),
            "--property=RemainAfterExit=yes".to_string(),
            format!(
                "--property=TimeoutStartSec={}",
                self.manifest.max_updater_seconds
            ),
            format!(
                "--property=RuntimeMaxSec={}",
                self.manifest.max_updater_seconds
            ),
            "--property=KillMode=control-group".to_string(),
            format!("--property=WorkingDirectory={}", self.paths.work.display()),
            format!("--property=StandardInput=file:{}", request_path.display()),
            format!("--property=StandardOutput=file:{}", reply_path.display()),
            format!("--property=StandardError=file:{}", stderr_path.display()),
            "--property=UMask=0077".to_string(),
            "/usr/bin/env".to_string(),
            "-i".to_string(),
        ];
        for (key, value) in &environment {
            args.push(format!("{key}={value}"));
        }
        args.push("/bin/sh".to_string());
        args.push(self.paths.wrapper.display().to_string());

        let started = Instant::now();
        let systemd_run_output = systemd_run(
            &args,
            Duration::from_secs(self.manifest.max_updater_seconds + 20),
        )?;
        ensure!(
            systemd_run_output.status.success(),
            "systemd-run could not start updater unit {unit}: {}",
            render(&systemd_run_output)
        );

        let deadline = started + Duration::from_secs(self.manifest.max_updater_seconds);
        let mut pid = 0_u32;
        let mut main_pid = 0_u32;
        let mut unit_cgroup = String::new();
        let mut gate_ready = false;
        let mut props;
        loop {
            props = unit_properties(&unit)?;
            let active = property(&props, "ActiveState").to_string();
            let load = property(&props, "LoadState").to_string();
            if pid == 0 {
                if let Ok(text) = fs::read_to_string(&pid_path) {
                    pid = text.trim().parse().unwrap_or(0);
                }
            }
            if main_pid == 0 {
                main_pid = property_u32(&props, "MainPID");
            }
            if active == "failed" || (active == "inactive" && load != "not-found") {
                break;
            }
            let running = matches!(active.as_str(), "activating" | "active" | "reloading");
            if pid > 0 && main_pid == pid && running && probe_done_path.exists() {
                unit_cgroup = property(&props, "ControlGroup").to_string();
                ensure!(
                    !unit_cgroup.is_empty(),
                    "updater unit {unit} has an empty ControlGroup: {props:?}"
                );
                gate_ready = true;
                break;
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for updater unit {unit} to reach its gate: pid={pid} main_pid={main_pid} unit={props:?}\nprobe_done={}\nstderr tail:\n{}",
                    probe_done_path.exists(),
                    tail_text(&read_text_lossy(&stderr_path), 4000)
                );
            }
            thread::sleep(Duration::from_millis(20));
        }

        let proc_cgroup = if pid > 0 {
            read_proc_cgroup(pid).unwrap_or_default()
        } else {
            String::new()
        };
        if gate_ready {
            ensure!(
                proc_cgroup == unit_cgroup,
                "updater /proc cgroup {proc_cgroup:?} != unit ControlGroup {unit_cgroup:?}"
            );
            ensure!(
                proc_cgroup.contains(&unit),
                "updater cgroup {proc_cgroup:?} does not contain unit {unit}"
            );
            write_private_file(&gate_path, b"go\n")?;
            let finish_deadline =
                Instant::now() + Duration::from_secs(self.manifest.max_updater_seconds);
            loop {
                props = unit_properties(&unit)?;
                let active = property(&props, "ActiveState");
                let sub = property(&props, "SubState");
                if active == "active" && sub == "exited" {
                    break;
                }
                if active == "failed" || active == "inactive" {
                    break;
                }
                if Instant::now() >= finish_deadline {
                    bail!(
                        "updater unit {unit} did not finish within {:?}: unit={props:?}\nstderr tail:\n{}",
                        self.manifest.max_updater_seconds,
                        tail_text(&read_text_lossy(&stderr_path), 4000)
                    );
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        let reply_source = read_text_lossy(&reply_path);
        let (reply, reply_error) = if reply_source.trim().is_empty() {
            (None, None)
        } else {
            match serde_json::from_str::<Value>(&reply_source) {
                Ok(value) => (Some(value), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            }
        };
        let probe_source = read_text_lossy(&probe_path);
        let (probe, probe_error) = if probe_source.trim().is_empty() {
            (None, Some("probe file is empty or missing".to_string()))
        } else {
            match serde_json::from_str::<Value>(&probe_source) {
                Ok(value) => (Some(value), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            }
        };
        let stderr = read_text_lossy(&stderr_path);
        let ss = read_text_lossy(&ss_path);
        let yama_ptrace_scope = read_text_lossy(&yama_path).trim().to_string();
        let active_state = property(&props, "ActiveState").to_string();
        let sub_state = property(&props, "SubState").to_string();
        let result = property(&props, "Result").to_string();
        let exec_main_status = property_i32(&props, "ExecMainStatus");
        let success = active_state == "active"
            && sub_state == "exited"
            && result == "success"
            && reply.is_some();

        let run = UpdaterRun {
            label: label.to_string(),
            operation: serde_json::to_value(operation)?
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            unit: unit.clone(),
            request,
            pid,
            main_pid,
            unit_cgroup,
            proc_cgroup,
            active_state,
            sub_state,
            result,
            exec_main_status,
            reply,
            reply_error,
            stderr,
            systemd_run_stdout: String::from_utf8_lossy(&systemd_run_output.stdout).into_owned(),
            systemd_run_stderr: String::from_utf8_lossy(&systemd_run_output.stderr).into_owned(),
            probe,
            probe_error,
            ss,
            yama_ptrace_scope,
            elapsed_ms: started.elapsed().as_millis(),
            success,
        };
        eprintln!(
            "[core-systemd-live] updater {} unit={} pid={} cgroup={} active={} sub={} result={} exit={:?} success={} elapsed_ms={}",
            label, unit, run.pid, run.proc_cgroup, run.active_state, run.sub_state, run.result, run.exec_main_status, run.success, run.elapsed_ms
        );
        if !run.stderr.is_empty() {
            eprintln!(
                "[core-systemd-live] updater {} stderr tail:\n{}",
                label,
                run.stderr_tail(4000)
            );
        }
        let _ = systemctl(&["stop", &unit]);
        let _ = systemctl(&["reset-failed", &unit]);
        Ok(run)
    }

    fn run_negative_control(&mut self, release: &LiveRelease) -> Result<UpdaterRun> {
        let original = fs::read(&release.manifest_path)
            .with_context(|| format!("read {}", release.manifest_path.display()))?;
        let original_sha = format!("{:x}", Sha256::digest(&original));
        ensure!(
            original_sha == release.manifest_sha256,
            "active release manifest changed before negative control"
        );
        let mut document: Value = serde_json::from_slice(&original)?;
        document["artifacts"][0]["sha256"] = json!("0".repeat(64));
        let tampered = serde_json::to_vec_pretty(&document)?;
        fs::set_permissions(&release.manifest_path, fs::Permissions::from_mode(0o644))?;
        fs::write(&release.manifest_path, &tampered)?;
        fs::set_permissions(&release.manifest_path, fs::Permissions::from_mode(0o444))?;

        let result = self.run_updater("negative-observe", ControlOperation::Observe, None, None);

        fs::set_permissions(&release.manifest_path, fs::Permissions::from_mode(0o644))?;
        fs::write(&release.manifest_path, &original)?;
        fs::set_permissions(&release.manifest_path, fs::Permissions::from_mode(0o444))?;
        let restored = fs::read(&release.manifest_path)?;
        ensure!(
            format!("{:x}", Sha256::digest(&restored)) == release.manifest_sha256,
            "restored release manifest digest differs"
        );
        result
    }

    fn cleanup(&mut self, before: &BTreeMap<String, UnitSnapshot>) -> Result<CleanupEvidence> {
        let units = self.cleanup_units_best_effort();
        let production_after = snapshot_protected()?;
        assert_protected_unchanged(before, &production_after)?;
        let residual_pids =
            pids_in_cgroup_containing(&format!("{}-{}", self.manifest.unit_prefix, self.token))
                .unwrap_or_default();
        let socket_error = match UnixStream::connect(&self.paths.socket) {
            Ok(_) => Some("control socket still accepts connections after cleanup".to_string()),
            Err(error) => Some(format!("{error}")),
        };
        self.cleaned = true;
        Ok(CleanupEvidence {
            units,
            residual_pids,
            socket_error,
            daemon_reload: "requested".to_string(),
            production_after,
        })
    }

    fn cleanup_units_best_effort(&mut self) -> Vec<UnitSnapshot> {
        let mut units: Vec<String> = self.updater_units.clone();
        units.push(self.core_unit.clone());
        for unit in &units {
            let _ = systemctl(&["stop", unit]);
            let _ = systemctl(&["reset-failed", unit]);
        }
        let _ = systemctl(&["daemon-reload"]);
        let mut snapshots = Vec::new();
        for unit in &units {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match unit_properties(unit) {
                    Ok(props) => {
                        let active = property(&props, "ActiveState").to_string();
                        let main_pid = property_u32(&props, "MainPID");
                        if active != "active" || main_pid == 0 {
                            snapshots.push(UnitSnapshot {
                                unit: unit.clone(),
                                load_state: property(&props, "LoadState").to_string(),
                                active_state: active,
                                main_pid,
                                invocation_id: property(&props, "InvocationID").to_string(),
                                control_group: property(&props, "ControlGroup").to_string(),
                                start_time: if main_pid > 0 {
                                    read_proc_starttime(main_pid).unwrap_or(0)
                                } else {
                                    0
                                },
                            });
                            break;
                        }
                    }
                    Err(error) => {
                        snapshots.push(UnitSnapshot {
                            unit: unit.clone(),
                            load_state: "unknown".into(),
                            active_state: format!("show failed: {error:#}"),
                            main_pid: 0,
                            invocation_id: String::new(),
                            control_group: String::new(),
                            start_time: 0,
                        });
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    snapshots.push(snapshot_unit(unit).unwrap_or(UnitSnapshot {
                        unit: unit.clone(),
                        load_state: "unknown".into(),
                        active_state: "timeout".into(),
                        main_pid: 0,
                        invocation_id: String::new(),
                        control_group: String::new(),
                        start_time: 0,
                    }));
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
        eprintln!("[core-systemd-live] cleanup units={snapshots:?}");
        snapshots
    }

    #[allow(clippy::too_many_arguments)] // assembles one explicit live acceptance report
    fn write_report(
        &self,
        outcome: &str,
        observe: &UpdaterRun,
        ready: &UpdaterRun,
        negative: Option<&UpdaterRun>,
        observe_verdict: &AdapterVerdict,
        ready_verdict: &AdapterVerdict,
        negative_verdict: Option<&AdapterVerdict>,
        probe_verdict: Option<&Value>,
        failures: &[String],
        cleanup: &CleanupEvidence,
    ) -> Result<PathBuf> {
        let report_dir = &self.report_dir;
        fs::create_dir_all(report_dir)?;
        let mut copied = BTreeMap::new();
        for (name, source) in [
            ("config.toml", self.paths.config_file.clone()),
            ("core.log", self.paths.core_log.clone()),
            ("wrapper.sh", self.paths.wrapper.clone()),
            ("probe.py", self.paths.probe.clone()),
            ("source-scan.json", PathBuf::new()),
        ] {
            if source.as_os_str().is_empty() {
                continue;
            }
            if source.exists() {
                let destination = report_dir.join(name);
                if name == "core.log" {
                    write_private_file(
                        &destination,
                        tail_text(&read_text_lossy(&source), 65536).as_bytes(),
                    )?;
                } else {
                    fs::copy(&source, &destination).with_context(|| {
                        format!("copy {} -> {}", source.display(), destination.display())
                    })?;
                }
                copied.insert(name.to_string(), destination.display().to_string());
            }
        }
        for run in [Some(observe), Some(ready), negative].into_iter().flatten() {
            for (suffix, name) in [
                ("request.json", "request.json"),
                ("reply.json", "reply.json"),
                ("stderr", "stderr"),
                ("pid", "pid"),
                ("cgroup", "cgroup"),
                ("probe.json", "probe.json"),
                ("ss", "ss"),
                ("yama", "yama"),
            ] {
                let source = self.paths.updater_file(&run.label, suffix);
                if source.exists() {
                    let destination = report_dir.join(format!("{}.{}", run.label, name));
                    fs::copy(&source, &destination).with_context(|| {
                        format!("copy {} -> {}", source.display(), destination.display())
                    })?;
                }
            }
        }
        let source_scan = scan_core_source(&self.manifest.core_root)?;
        let report = json!({
            "schema_version": 1,
            "outcome": outcome,
            "failures": failures,
            "token": self.token,
            "core_root": self.manifest.core_root,
            "core_binary": self.binary,
            "core_binary_sha256": self.binary_sha256,
            "core_binary_size": self.binary_size,
            "core_version_help": self.core_version,
            "release_id": self.manifest.release_id,
            "release_manifest": self.release.as_ref().map(|release| json!({
                "manifest_path": release.manifest_path,
                "manifest_sha256": release.manifest_sha256,
                "core_sha256": release.core_sha256,
                "core_size": release.core_size,
                "artifact": release.artifact,
                "selector": release.selector,
                "adapter_config": release.adapter_config,
                "release_dir": release.release_dir,
            })),
            "core_unit": {
                "unit": self.core_unit,
                "pid": self.core_pid,
                "cgroup": self.core_cgroup,
                "start_time": self.core_starttime,
                "properties": self.core_props,
                "status": self.core_status,
                "cmdline": self.core_cmdline,
                "proc_exe_from_test": self.core_proc_exe_from_test,
                "proc_environ_from_test": self.core_proc_environ_from_test,
                "log_tail": tail_text(&read_text_lossy(&self.paths.core_log), 8192),
            },
            "isolation": {
                "test_cgroup": read_proc_cgroup(std::process::id()).unwrap_or_default(),
                "config_sha256": sha256_file(&self.paths.config_file)?,
                "management_port": self.port,
                "socket": self.paths.socket,
                "config_template": self.template_path,
                "manifest_path": self.manifest_path,
                "unit_prefix": self.manifest.unit_prefix,
                "core_environment_expected": core_environment(&self.paths, &self.manifest.release_id),
                "yama_ptrace_scope": read_text_lossy(Path::new("/proc/sys/kernel/yama/ptrace_scope")).trim().to_string(),
                "source_scan": source_scan,
            },
            "observe": observe,
            "observe_verdict": observe_verdict,
            "ready": ready,
            "ready_verdict": ready_verdict,
            "negative_control": negative,
            "negative_control_verdict": negative_verdict,
            "probe_verdict": probe_verdict,
            "production_before": self.production_before,
            "cleanup": cleanup,
            "copied_evidence": copied,
        });
        let report_path = report_dir.join("report.json");
        write_private_file(&report_path, &serde_json::to_vec_pretty(&report)?)?;
        eprintln!("[core-systemd-live] report={}", report_path.display());
        Ok(report_path)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup_units_best_effort();
        }
    }
}

fn check_core_help(binary: &Path, paths: &LivePaths, timeout: Duration) -> Result<String> {
    let mut command = Command::new(binary);
    command
        .arg("--help")
        .current_dir(&paths.work)
        .env_clear()
        .envs(minimal_environment(paths));
    let output = run_bounded(command, timeout, "real Core --help")?;
    ensure!(
        output.status.success(),
        "real Core --help failed: {}",
        render(&output)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    ensure!(
        !text.trim().is_empty(),
        "real Core --help produced no stdout"
    );
    Ok(text.lines().next().unwrap_or("").to_string())
}

fn core_status_is_ready(status: &Value, manifest: &Manifest) -> bool {
    status["ok"] == json!(true)
        && status["operation"] == json!("status")
        && status["phase"] == json!("idle")
        && status["prepared"] == json!(false)
        && status["maintenance_held"] == json!(false)
        && status["release"]["declared"] == json!(manifest.release_id)
        && status["modules_verified"] == json!(true)
        && status["module_state"]["verified"] == json!(true)
}

fn check_reply(run: &UpdaterRun, operation: ControlOperation, live: &Live) -> Result<()> {
    let value = run
        .reply
        .as_ref()
        .with_context(|| format!("updater {} produced no reply", run.label))?;
    let reply: ControlReply = serde_json::from_value(value.clone())
        .with_context(|| format!("parse updater {} reply as ControlReply", run.label))?;
    ensure!(
        reply.protocol == CONTROL_PROTOCOL,
        "updater reply protocol is {:?}, expected {:?}",
        reply.protocol,
        CONTROL_PROTOCOL
    );
    ensure!(
        reply.operation == operation,
        "updater reply operation is {:?}, expected {:?}",
        reply.operation,
        operation
    );
    ensure!(
        reply.instance == live.manifest.instance_id,
        "updater reply instance {:?} != {:?}",
        reply.instance,
        live.manifest.instance_id
    );
    ensure!(
        reply.success,
        "updater reply reports success=false: {value}"
    );
    ensure!(
        reply.error.is_none(),
        "updater reply has error={:?}",
        reply.error
    );
    match operation {
        ControlOperation::Observe => {
            ensure!(
                reply.current_release.as_deref() == Some(&live.manifest.release_id),
                "Observe reply current_release={:?}, expected {:?}",
                reply.current_release,
                live.manifest.release_id
            );
            ensure!(
                reply.maintenance == Some(false),
                "Observe reply maintenance={:?}, expected false",
                reply.maintenance
            );
            ensure!(
                reply.ready == Some(true),
                "Observe reply ready={:?}, expected true",
                reply.ready
            );
            ensure!(
                reply.active_work == Some(0),
                "Observe reply active_work={:?}, expected 0",
                reply.active_work
            );
        }
        ControlOperation::Ready => {
            ensure!(
                reply.ready == Some(true),
                "Ready reply ready={:?}, expected true",
                reply.ready
            );
            ensure!(
                reply.current_release.as_deref() == Some(&live.manifest.release_id),
                "Ready reply current_release={:?}, expected {:?}",
                reply.current_release,
                live.manifest.release_id
            );
        }
        other => bail!("unexpected operation in check_reply: {other:?}"),
    }
    Ok(())
}

fn parse_os_error(text: &str) -> Option<i32> {
    let index = text.find("os error ")?;
    let digits: String = text[index + "os error ".len()..]
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn extract_proc_exe(text: &str) -> Option<String> {
    let start = text.find("/proc/")?;
    let rest = &text[start..];
    let end = rest.find("/exe")?;
    let path = &rest[..end + 4];
    if path
        .trim_start_matches("/proc/")
        .trim_end_matches("/exe")
        .chars()
        .all(|character| character.is_ascii_digit())
    {
        Some(path.to_string())
    } else {
        None
    }
}

fn errno_name(errno: i32) -> String {
    let name = match errno {
        1 => "EPERM",
        2 => "ENOENT",
        13 => "EACCES",
        _ => "UNKNOWN",
    };
    format!("{name} ({errno})")
}

fn evaluate_run(run: &UpdaterRun, operation: ControlOperation, live: &Live) -> AdapterVerdict {
    if run.success {
        return match check_reply(run, operation, live) {
            Ok(()) => AdapterVerdict {
                ok: true,
                failure_kind: None,
                errno: None,
                errno_name: None,
                proc_path: None,
                mechanism: None,
                detail: "adapter reply is a successful, schema-valid acknowledgement".to_string(),
            },
            Err(error) => AdapterVerdict {
                ok: false,
                failure_kind: Some("invalid_reply".to_string()),
                errno: None,
                errno_name: None,
                proc_path: None,
                mechanism: None,
                detail: format!("{error:#}"),
            },
        };
    }
    let stderr = &run.stderr;
    let errno = parse_os_error(stderr);
    let proc_path = extract_proc_exe(stderr);
    let digest_mismatch =
        stderr.contains("running executable SHA") && stderr.contains("does not match");
    let permission = errno == Some(13) || stderr.contains("Permission denied");
    let kind = if digest_mismatch {
        "running_image_digest_mismatch"
    } else if permission && proc_path.is_some() {
        "proc_exe_access_denied"
    } else if run.result == "timeout" {
        "updater_timeout"
    } else {
        "adapter_error"
    };
    let probe_text = run
        .probe
        .as_ref()
        .and_then(|probe| probe["exe"].as_object())
        .and_then(|exe| exe.get("readlink_error"))
        .map(Value::to_string)
        .unwrap_or_else(|| "<no probe>".to_string());
    let mechanism = match kind {
        "proc_exe_access_denied" => Some(format!(
            "The independent updater unit could not open/read {} ({}). The updater is a separate systemd unit, not an ancestor of the Core unit's process, and Core does not authorize it with PR_SET_PTRACER. procfs reports EACCES (13) rather than EPERM. kernel.yama.ptrace_scope={:?}; updater probe readlink_error={}; updater cgroup={:?}; core cgroup={:?}",
            proc_path.clone().unwrap_or_else(|| "/proc/<pid>/exe".into()),
            errno.map(errno_name).unwrap_or_else(|| "unknown errno".into()),
            run.yama_ptrace_scope,
            probe_text,
            run.proc_cgroup,
            live.core_cgroup
        )),
        "running_image_digest_mismatch" => Some(
            "The adapter did read and hash the running image, but the digest did not match the active release manifest. This is the expected negative-control result."
                .to_string(),
        ),
        "updater_timeout" => Some(format!(
            "The updater unit {} did not finish before its deadline (Result={}).",
            run.unit, run.result
        )),
        _ => None,
    };
    AdapterVerdict {
        ok: false,
        failure_kind: Some(kind.to_string()),
        errno,
        errno_name: errno.map(errno_name),
        proc_path,
        mechanism,
        detail: if stderr.trim().is_empty() {
            format!(
                "updater unit {} did not succeed: active={} sub={} result={} exit={:?}",
                run.unit, run.active_state, run.sub_state, run.result, run.exec_main_status
            )
        } else {
            tail_text(stderr, 4000)
        },
    }
}

fn verify_probe(run: &UpdaterRun, live: &Live, release: &LiveRelease) -> Result<Value> {
    let Some(probe) = &run.probe else {
        return Ok(json!({
            "available": false,
            "error": run.probe_error,
            "note": "no independent probe result; the adapter reply and negative control remain the acceptance evidence"
        }));
    };
    let mut facts = Map::new();
    facts.insert("available".into(), json!(true));
    facts.insert("python_pid".into(), probe["python_pid"].clone());
    facts.insert("parent_pid".into(), probe["parent_pid"].clone());
    facts.insert("core_pid".into(), probe["core_pid"].clone());
    facts.insert("self_cgroup".into(), probe["self_cgroup"].clone());
    facts.insert(
        "yama_ptrace_scope".into(),
        probe["yama_ptrace_scope"].clone(),
    );
    facts.insert("exe".into(), probe["exe"].clone());
    facts.insert("environ".into(), probe["environ"].clone());

    ensure!(
        probe["parent_pid"] == json!(run.pid),
        "probe parent_pid {:?} != updater wrapper pid {}",
        probe["parent_pid"],
        run.pid
    );
    ensure!(
        probe["core_pid"] == json!(live.core_pid),
        "probe core_pid {:?} != systemd Core MainPID {}",
        probe["core_pid"],
        live.core_pid
    );
    if let Some(cgroup) = probe["self_cgroup"].as_str() {
        let cgroup = cgroup.strip_prefix("0::").unwrap_or(cgroup).trim();
        ensure!(
            cgroup == run.proc_cgroup,
            "probe self_cgroup {:?} != updater /proc cgroup {:?}",
            cgroup,
            run.proc_cgroup
        );
    }

    let exe = &probe["exe"];
    if let Some(sha) = exe["sha256"].as_str() {
        ensure!(
            sha == release.core_sha256,
            "independent updater probe hashed /proc/{}/exe as {sha}, but the real binary is {}",
            live.core_pid,
            release.core_sha256
        );
        ensure!(
            exe["bytes"].as_u64() == Some(release.core_size),
            "independent updater probe read {:?} bytes, expected {}",
            exe["bytes"],
            release.core_size
        );
        facts.insert("sha256".into(), json!(sha));
        facts.insert("bytes".into(), exe["bytes"].clone());
        facts.insert("hash_match".into(), json!(true));
    } else {
        facts.insert("hash_match".into(), json!(false));
        let readlink_error = &exe["readlink_error"];
        let hash_error = &exe["hash_error"];
        return Err(anyhow::anyhow!(
            "independent updater probe could not hash /proc/{}/exe: readlink_error={} hash_error={}",
            live.core_pid,
            readlink_error,
            hash_error
        ));
    }
    if let Some(readlink) = exe["readlink"].as_str() {
        ensure!(
            Path::new(readlink) == live.binary,
            "probe /proc/{}/exe readlink={readlink:?} != real binary {:?}",
            live.core_pid,
            live.binary
        );
        facts.insert("readlink".into(), json!(readlink));
    }
    if let Some(stat) = exe["stat"].as_object() {
        facts.insert("stat".into(), json!(stat));
    }

    if probe["environ"]["ok"] == json!(true) {
        let observed: BTreeMap<String, String> = probe["environ"]["values"]
            .as_object()
            .context("probe environ values is not an object")?
            .iter()
            .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
            .collect();
        let expected = core_environment(&live.paths, &live.manifest.release_id);
        for (key, value) in &expected {
            ensure!(
                observed.get(key) == Some(value),
                "Core environment observed by the updater has {key}={:?}, expected {value:?}",
                observed.get(key)
            );
        }
        let forbidden: Vec<&String> = observed
            .keys()
            .filter(|key| {
                let upper = key.to_ascii_uppercase();
                ["TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "API_KEY"]
                    .iter()
                    .any(|needle| upper.contains(needle))
            })
            .collect();
        ensure!(
            forbidden.is_empty(),
            "Core environment observed by the updater contains credential-like keys: {forbidden:?}"
        );
        for (key, value) in &observed {
            if key == "PATH" {
                ensure!(value == FIXED_PATH, "Core PATH {value:?} != {FIXED_PATH:?}");
            }
            if [
                "HOME",
                "TMPDIR",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_CACHE_HOME",
                "XDG_STATE_HOME",
                "XDG_RUNTIME_DIR",
            ]
            .contains(&key.as_str())
            {
                ensure!(
                    Path::new(value).starts_with(&live.paths.root),
                    "Core environment {key}={value:?} is outside the private temp root {}",
                    live.paths.root.display()
                );
            }
        }
        facts.insert("environ_observed".into(), json!(observed));
        facts.insert("environ_match".into(), json!(true));
    } else {
        facts.insert("environ_error".into(), probe["environ"].clone());
    }

    let ss = &run.ss;
    let mut listeners = Vec::new();
    for line in ss.lines() {
        let line = line.trim_start();
        if !(line.starts_with("LISTEN") || line.starts_with("UNCONN")) {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        let Some((_, port)) = fields[3].rsplit_once(':') else {
            continue;
        };
        let Ok(port) = port.parse::<u16>() else {
            continue;
        };
        let pid = line.find("pid=").and_then(|index| {
            line[index + 4..]
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect::<String>()
                .parse::<u32>()
                .ok()
        });
        listeners.push(json!({
            "protocol": fields[0],
            "port": port,
            "pid": pid,
        }));
    }
    let core_ports: BTreeSet<u16> = listeners
        .iter()
        .filter(|listener| listener["pid"] == json!(live.core_pid))
        .filter_map(|listener| listener["port"].as_u64().map(|port| port as u16))
        .collect();
    if !core_ports.is_empty() {
        ensure!(
            core_ports.contains(&live.port),
            "ss did not show Core pid {} listening on the configured management port {}: {:?}",
            live.core_pid,
            live.port,
            core_ports
        );
        ensure!(
            !core_ports.contains(&3132) && !core_ports.contains(&3133),
            "Core pid {} unexpectedly listens on production management/federation ports: {:?}",
            live.core_pid,
            core_ports
        );
    }
    facts.insert("ss_listeners".into(), json!(listeners));
    facts.insert("core_owned_ports".into(), json!(core_ports));
    Ok(Value::Object(facts))
}

fn evaluate_negative(run: &UpdaterRun, live: &Live) -> AdapterVerdict {
    if run.success {
        return AdapterVerdict {
            ok: false,
            failure_kind: Some("negative_control_unexpected_success".to_string()),
            errno: None,
            errno_name: None,
            proc_path: None,
            mechanism: Some(
                "A tampered manifest digest was accepted by Observe; the adapter did not compare the running image hash, or the negative control was not actually applied."
                    .to_string(),
            ),
            detail: format!("negative Observe reply={:?}", run.reply),
        };
    }
    let stderr = &run.stderr;
    if stderr.contains("running executable SHA") && stderr.contains("does not match") {
        AdapterVerdict {
            ok: false,
            failure_kind: Some("negative_control_detected".to_string()),
            errno: parse_os_error(stderr),
            errno_name: parse_os_error(stderr).map(errno_name),
            proc_path: extract_proc_exe(stderr),
            mechanism: Some(
                "Negative control passed: the adapter hashed /proc/<pid>/exe and rejected the tampered digest."
                    .to_string(),
            ),
            detail: tail_text(stderr, 4000),
        }
    } else {
        let mut verdict = evaluate_run(run, ControlOperation::Observe, live);
        verdict.mechanism = Some(format!(
            "Negative control did not produce a digest mismatch: kind={:?}; detail={}",
            verdict.failure_kind, verdict.detail
        ));
        verdict
    }
}

fn proc_readlink_result(pid: u32, name: &str) -> Value {
    match fs::read_link(format!("/proc/{pid}/{name}")) {
        Ok(path) => json!({"ok": true, "path": path}),
        Err(error) => json!({
            "ok": false,
            "errno": error.raw_os_error(),
            "error": error.to_string(),
        }),
    }
}

fn proc_read_result(pid: u32, name: &str) -> Value {
    match fs::read(format!("/proc/{pid}/{name}")) {
        Ok(bytes) => json!({"ok": true, "bytes": bytes.len()}),
        Err(error) => json!({
            "ok": false,
            "errno": error.raw_os_error(),
            "error": error.to_string(),
        }),
    }
}

fn scan_core_source(core_root: &Path) -> Result<Value> {
    let mut roots = Vec::new();
    let mut matches = Vec::new();
    for relative in SOURCE_SCAN_ROOTS {
        let root = core_root.join(relative);
        roots.push(root.display().to_string());
        if !root.is_dir() {
            continue;
        }
        let mut stack = vec![root];
        while let Some(directory) = stack.pop() {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                if metadata.is_dir() {
                    stack.push(path);
                } else if metadata.is_file()
                    && path.extension().and_then(|extension| extension.to_str()) == Some("rs")
                {
                    let Ok(text) = fs::read_to_string(&path) else {
                        continue;
                    };
                    for (number, line) in text.lines().enumerate() {
                        if line.contains("PR_SET_PTRACER")
                            || line.contains("PR_SET_DUMPABLE")
                            || line.contains("prctl(")
                        {
                            matches.push(json!({
                                "file": path,
                                "line": number + 1,
                                "text": line.trim(),
                            }));
                            if matches.len() >= 200 {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    let has_pr_set_ptracer = matches.iter().any(|entry| {
        entry["text"]
            .as_str()
            .map(|text| text.contains("PR_SET_PTRACER"))
            .unwrap_or(false)
    });
    Ok(json!({
        "roots": roots,
        "matches": matches,
        "has_pr_set_ptracer": has_pr_set_ptracer,
        "note": "read-only evidence scan; the test does not modify Core source",
    }))
}

fn ensure_user_systemd(manifest: &Manifest) -> Result<()> {
    if !manifest.require_systemd_user {
        return Ok(());
    }
    let output = systemctl(&["is-system-running"])?;
    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    ensure!(
        state == "running" || state == "degraded",
        "user systemd manager is not usable: state={state:?} {}",
        render(&output)
    );
    let output = run_bounded(
        {
            let mut command = Command::new("systemd-run");
            command.arg("--user").arg("--version");
            command
        },
        Duration::from_secs(10),
        "systemd-run --version",
    )?;
    ensure!(
        output.status.success(),
        "systemd-run --user is not usable: {}",
        render(&output)
    );
    Ok(())
}

fn ensure_test_not_in_protected_cgroup() -> Result<()> {
    let cgroup = read_proc_cgroup(std::process::id()).unwrap_or_default();
    for unit in PROTECTED_UNITS {
        ensure!(
            !cgroup.contains(unit),
            "refusing to run the live systemd acceptance inside protected unit {unit}: test cgroup {cgroup:?}. \
             Start the test as a transient service unit, not as a child of the agent shell."
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the test
// ---------------------------------------------------------------------------

#[test]
#[ignore = "opt-in live acceptance: real systemd Core + independent transient updater unit"]
fn independent_updater_unit_observes_and_readies_real_systemd_core() -> Result<()> {
    let started = Instant::now();
    let (manifest, manifest_path, template_path) = load_manifest()?;
    if !opted_in(&manifest)? {
        return Ok(());
    }
    ensure_user_systemd(&manifest)?;
    ensure_test_not_in_protected_cgroup()?;

    let production_before = snapshot_protected()?;
    let core_before = production_before
        .get("echo-agent-core.service")
        .context("missing production Core snapshot")?;
    let panel_before = production_before
        .get("echo-agent-panel.service")
        .context("missing production Panel snapshot")?;
    ensure!(
        core_before.active_state == "active" && core_before.main_pid > 0,
        "production echo-agent-core.service is not active; refusing to claim non-interference: {core_before:?}"
    );
    ensure!(
        panel_before.active_state == "active" && panel_before.main_pid > 0,
        "production echo-agent-panel.service is not active; refusing to claim non-interference: {panel_before:?}"
    );
    eprintln!(
        "[core-systemd-live] production before: core={} panel={}",
        core_before.main_pid, panel_before.main_pid
    );

    let mut live = Live::start(
        manifest,
        manifest_path,
        template_path,
        production_before.clone(),
    )
    .context("start real systemd Core fixture")?;
    let release = live.release.clone().context("release not published")?;

    let observe = live.run_updater("observe", ControlOperation::Observe, None, None)?;
    let observe_verdict = evaluate_run(&observe, ControlOperation::Observe, &live);
    let observe_probe = if observe.probe.is_some() {
        match verify_probe(&observe, &live, &release) {
            Ok(facts) => Some(facts),
            Err(error) => {
                live.write_report(
                    "failed",
                    &observe,
                    &observe,
                    None,
                    &AdapterVerdict {
                        ok: false,
                        failure_kind: Some("probe_failed".into()),
                        errno: None,
                        errno_name: None,
                        proc_path: None,
                        mechanism: Some(format!("{error:#}")),
                        detail: format!("{error:#}"),
                    },
                    &AdapterVerdict {
                        ok: false,
                        failure_kind: Some("not_run".into()),
                        errno: None,
                        errno_name: None,
                        proc_path: None,
                        mechanism: None,
                        detail: "skipped because the independent probe failed".into(),
                    },
                    None,
                    None,
                    &[format!("{error:#}")],
                    &CleanupEvidence {
                        units: Vec::new(),
                        residual_pids: Vec::new(),
                        socket_error: None,
                        daemon_reload: "not_attempted".into(),
                        production_after: production_before.clone(),
                    },
                )?;
                return Err(error).context("independent updater probe failed");
            }
        }
    } else {
        None
    };

    let negative = if observe_verdict.ok {
        Some(live.run_negative_control(&release)?)
    } else {
        None
    };
    let negative_verdict = negative.as_ref().map(|run| evaluate_negative(run, &live));

    let ready = live.run_updater(
        "ready",
        ControlOperation::Ready,
        Some(&release.release_id),
        Some(&release.manifest_sha256),
    )?;
    let ready_verdict = evaluate_run(&ready, ControlOperation::Ready, &live);

    let mut failures = Vec::new();
    if !observe_verdict.ok {
        failures.push(format!(
            "Observe failed: kind={:?} errno={:?} detail={}",
            observe_verdict.failure_kind, observe_verdict.errno_name, observe_verdict.detail
        ));
    }
    if negative.is_some() {
        let verdict = negative_verdict.as_ref().expect("negative verdict");
        if verdict.failure_kind.as_deref() != Some("negative_control_detected") {
            failures.push(format!(
                "negative control did not detect the tampered digest: kind={:?} detail={}",
                verdict.failure_kind, verdict.detail
            ));
        }
    } else if observe_verdict.ok {
        failures.push("negative control was not run after a successful Observe".to_string());
    }
    if !ready_verdict.ok {
        failures.push(format!(
            "Ready failed: kind={:?} errno={:?} detail={}",
            ready_verdict.failure_kind, ready_verdict.errno_name, ready_verdict.detail
        ));
    }
    if let Some(probe) = &observe_probe {
        if probe["hash_match"] != json!(true) {
            failures.push(format!(
                "independent updater probe did not prove the hash: {probe}"
            ));
        }
    }

    let cleanup = live.cleanup(&production_before)?;
    ensure!(
        cleanup.residual_pids.is_empty(),
        "transient test units left processes behind: {:?}",
        cleanup.residual_pids
    );
    ensure!(
        cleanup.socket_error.as_deref()
            != Some("control socket still accepts connections after cleanup"),
        "Core control socket still accepts connections after cleanup"
    );

    let elapsed = started.elapsed();
    ensure!(
        elapsed <= Duration::from_secs(live.manifest.max_total_seconds),
        "live acceptance exceeded max_total_seconds: {elapsed:?} > {}s",
        live.manifest.max_total_seconds
    );
    let outcome = if failures.is_empty() {
        "passed"
    } else {
        "failed"
    };
    let report = live.write_report(
        outcome,
        &observe,
        &ready,
        negative.as_ref(),
        &observe_verdict,
        &ready_verdict,
        negative_verdict.as_ref(),
        observe_probe.as_ref(),
        &failures,
        &cleanup,
    )?;

    eprintln!(
        "[core-systemd-live] outcome={outcome} elapsed={:?} observe={} ready={} negative={}",
        elapsed,
        observe_verdict.ok,
        ready_verdict.ok,
        negative_verdict
            .as_ref()
            .map(|verdict| verdict.failure_kind.clone().unwrap_or_default())
            .unwrap_or_else(|| "not_run".into())
    );
    if !failures.is_empty() {
        let blocked = [&observe_verdict, &ready_verdict]
            .into_iter()
            .any(|verdict| verdict.failure_kind.as_deref() == Some("proc_exe_access_denied"));
        let prefix = if blocked {
            "LIVE ACCEPTANCE BLOCKED"
        } else {
            "LIVE ACCEPTANCE FAILED"
        };
        bail!(
            "{prefix}: {}\nreport: {}\nobserve={observe_verdict:?}\nready={ready_verdict:?}\nnegative={negative_verdict:?}",
            failures.join("; "),
            report.display()
        );
    }
    eprintln!(
        "[core-systemd-live] PASS: independent updater unit hashed /proc/{}/exe and Observe/Ready matched the real manifest; report={}",
        live.core_pid,
        report.display()
    );
    Ok(())
}
