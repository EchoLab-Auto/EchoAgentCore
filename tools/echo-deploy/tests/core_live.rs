//! **Opt-in, live** smoke test for the real Core deployment control socket.
//!
//! This test is deliberately *not* a fixture-Core test. It executes an already
//! built `echo-agent-core` binary from the exact Core source root, with a fresh
//! private temp tree and an otherwise empty environment.  It proves the parts
//! the deployment adapter depends on but a JSON fixture cannot prove:
//!
//! * Core really binds `ECHO_DEPLOY_CONTROL_SOCKET` and answers `status`,
//!   `prepare`, `ready` and `cancel` over that Unix socket;
//! * `prepare` really pauses controlled writers, drains turns, performs a
//!   checked flush, holds maintenance and returns a generation in its ACK;
//! * `cancel` requires the *same* transaction id and generation, rejects a
//!   mismatched pair without releasing maintenance, and then releases the
//!   writers and returns to `idle`;
//! * the peer process is the configured binary: `/proc/<pid>/exe` is hashed and
//!   compared with the configured file, and the live peer pid is read from
//!   `SO_PEERCRED`;
//! * no real config, credentials or user data are read: the config is rendered
//!   from `tests/fixtures/core_live.toml` into the temp tree, `env_clear()` is
//!   used, HOME/XDG/TMPDIR all point inside the temp tree, personas are empty,
//!   QQ is disabled, federation is explicit `listen = ""` and the management
//!   listener is a random free `127.0.0.1` port;
//! * the only TCP listener the process owns is that random loopback port, there
//!   are no outbound TCP/UDP sockets (the provider points at `127.0.0.1:9` and
//!   no turn is ever started), and Core has no child processes.
//!
//! Run it explicitly, outside the production `echo-agent-core.service` cgroup:
//!
//! ```bash
//! cd tools/echo-deploy
//! systemd-run --user --scope \
//!   --setenv=ECHO_DEPLOY_RUN_CORE_LIVE=1 \
//!   --setenv=ECHO_DEPLOY_CORE_BINARY=/home/evence/.local/share/echo-agent-core/source/target/debug/echo-agent-core \
//!   cargo test --locked --test core_live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! A transient service unit (`systemd-run --user --unit=... --collect ...`) is
//! equally valid: the invariant is only that the test process and the Core it
//! spawns are not inside `echo-agent-core.service` (or another protected
//! production unit).  A service unit also gives the run a hard
//! `RuntimeMaxSec` in addition to the test's own deadlines.
//!
//! Requirements:
//!
//! * `ECHO_DEPLOY_RUN_CORE_LIVE=1` (without it the test prints `SKIP` and does
//!   nothing; the test is `#[ignore]`d anyway).
//! * `ECHO_DEPLOY_CORE_BINARY` must be an absolute path if set; when omitted the
//!   exact default in `tests/fixtures/core_live.manifest.json` is used.  Either
//!   way the resolved file must live under the manifest's exact Core root.
//! * The process must not be in a protected production unit cgroup.  The test
//!   fails closed with the `systemd-run --user --scope` instruction above when
//!   it detects `echo-agent-core.service` / `echo-agent-panel.service` /
//!   `echo-agent-core-update.service` in its own cgroup.
//!
//! The real Core binary may be older than the current module-evidence work.
//! When it has no `modules_verified` / `module_state` fields the test records
//! `module_evidence: "absent_legacy_binary"` and still validates the control
//! protocol; it never claims that an old binary is the new one.  The observed
//! binary SHA-256 is always printed, but it is evidence about the *running
//! image only*, not build provenance.

use anyhow::{bail, ensure, Context, Result};
use echo_deploy::control_client::{
    core_identity, current_euid, map_to_systemd_reply,
    map_to_systemd_reply_with_process_verification, query_core, verify_required_modules,
    CoreOperation,
};
use echo_deploy::systemd::{ControlOperation, ControlRequest, CONTROL_PROTOCOL};
use serde::Deserialize;
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
            fs::{MetadataExt, PermissionsExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

const FIXTURE_MANIFEST: &str = "core_live.manifest.json";
const PRIVATE_DIR_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const MIN_REAL_CORE_BYTES: u64 = 10 * 1024 * 1024;
const PROTECTED_UNITS: [&str; 3] = [
    "echo-agent-core.service",
    "echo-agent-panel.service",
    "echo-agent-core-update.service",
];
const FIXED_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

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
    management_host: String,
    provider_url: String,
    transaction_prefix: String,
    expected_phases: ExpectedPhases,
    writer_scope_covered: Vec<String>,
    writer_scope_not_covered: Vec<String>,
    prepare_requires: Vec<String>,
    ready_requires: Vec<String>,
    required_modules: Vec<String>,
    forbidden_ports: Vec<u16>,
    max_help_seconds: u64,
    max_startup_seconds: u64,
    max_control_seconds: u64,
    max_shutdown_seconds: u64,
    max_total_seconds: u64,
    require_ready: bool,
    require_systemd_scope: bool,
    module_evidence: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedPhases {
    status_initial: String,
    prepare: String,
    status_prepared: String,
    cancel: String,
    status_cancelled: String,
    status_final: String,
}

impl Manifest {
    fn control_timeout(&self) -> Duration {
        Duration::from_secs(self.max_control_seconds)
    }

    fn startup_timeout(&self) -> Duration {
        Duration::from_secs(self.max_startup_seconds)
    }

    fn shutdown_timeout(&self) -> Duration {
        Duration::from_secs(self.max_shutdown_seconds)
    }

    fn total_timeout(&self) -> Duration {
        Duration::from_secs(self.max_total_seconds)
    }
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load_manifest() -> Result<(Manifest, PathBuf, PathBuf)> {
    let fixtures = fs::canonicalize(fixtures_dir()).context("canonicalize tests/fixtures")?;
    let manifest_path = fixtures.join(FIXTURE_MANIFEST);
    let manifest_meta = fs::symlink_metadata(&manifest_path)
        .with_context(|| format!("stat fixture manifest {}", manifest_path.display()))?;
    ensure!(
        manifest_meta.file_type().is_file(),
        "fixture manifest {} is not a regular file",
        manifest_path.display()
    );
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("read fixture manifest {}", manifest_path.display()))?;
    let manifest: Manifest = serde_json::from_str(&raw)
        .with_context(|| format!("parse fixture manifest {}", manifest_path.display()))?;

    ensure!(
        Path::new(&manifest.config_template).components().count() == 1
            && Path::new(&manifest.config_template)
                .extension()
                .and_then(|ext| ext.to_str())
                == Some("toml"),
        "config_template must be a single .toml file name, got {:?}",
        manifest.config_template
    );
    let template_path = fixtures.join(&manifest.config_template);
    let template_meta = fs::symlink_metadata(&template_path)
        .with_context(|| format!("stat config template {}", template_path.display()))?;
    ensure!(
        template_meta.file_type().is_file(),
        "config template {} is not a regular file",
        template_path.display()
    );

    validate_manifest(&manifest, &manifest_path, &template_path)?;
    Ok((manifest, manifest_path, template_path))
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
        manifest.opt_in_env == "ECHO_DEPLOY_RUN_CORE_LIVE",
        "fixture manifest opt_in_env must be ECHO_DEPLOY_RUN_CORE_LIVE"
    );
    ensure!(
        manifest.core_binary_env == "ECHO_DEPLOY_CORE_BINARY",
        "fixture manifest core_binary_env must be ECHO_DEPLOY_CORE_BINARY"
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
        manifest.management_host == "127.0.0.1",
        "management_host must be loopback 127.0.0.1"
    );
    ensure!(
        manifest.provider_url == "http://127.0.0.1:9",
        "provider_url must be the discard endpoint http://127.0.0.1:9"
    );
    ensure!(
        !manifest.transaction_prefix.is_empty() && manifest.transaction_prefix.len() <= 200,
        "transaction_prefix must be 1..=200 bytes"
    );
    ensure!(
        safe_systemd_id(&manifest.release_id),
        "release_id {:?} is not a safe systemd identifier",
        manifest.release_id
    );
    for (label, values) in [
        ("writer_scope_covered", &manifest.writer_scope_covered),
        (
            "writer_scope_not_covered",
            &manifest.writer_scope_not_covered,
        ),
        ("prepare_requires", &manifest.prepare_requires),
        ("ready_requires", &manifest.ready_requires),
        ("required_modules", &manifest.required_modules),
    ] {
        ensure!(!values.is_empty(), "{label} must not be empty");
        ensure!(
            values.iter().all(|value| !value.is_empty()),
            "{label} must not contain empty entries"
        );
    }
    ensure!(
        manifest
            .writer_scope_not_covered
            .iter()
            .any(|value| value == "user_arbitrary_subprocesses"),
        "writer_scope_not_covered must name user_arbitrary_subprocesses"
    );
    ensure!(
        manifest.forbidden_ports.contains(&3132) && manifest.forbidden_ports.contains(&3133),
        "forbidden_ports must contain the production management/federation ports 3132 and 3133"
    );
    ensure!(
        manifest.module_evidence == "if_present",
        "module_evidence policy must be if_present"
    );
    ensure!(manifest.require_ready, "require_ready must be true");
    ensure!(
        manifest.require_systemd_scope,
        "require_systemd_scope must be true"
    );

    for (label, seconds) in [
        ("max_help_seconds", manifest.max_help_seconds),
        ("max_startup_seconds", manifest.max_startup_seconds),
        ("max_control_seconds", manifest.max_control_seconds),
        ("max_shutdown_seconds", manifest.max_shutdown_seconds),
        ("max_total_seconds", manifest.max_total_seconds),
    ] {
        ensure!(
            (1..=300).contains(&seconds),
            "{label} must be in 1..=300 seconds"
        );
    }

    for path in [manifest_path, template] {
        ensure!(
            path.is_absolute(),
            "fixture path must be absolute: {}",
            path.display()
        );
        let meta = fs::symlink_metadata(path)?;
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
                "SKIP core_live: {} is not set (this test runs the real Core binary)",
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
        "refusing a binary from tests/fixtures as the live Core: {}",
        binary.display()
    );

    let metadata =
        fs::metadata(&binary).with_context(|| format!("stat Core binary {}", binary.display()))?;
    ensure!(
        metadata.is_file(),
        "Core binary {} is not a regular file",
        binary.display()
    );
    ensure!(
        metadata.len() >= MIN_REAL_CORE_BYTES,
        "Core binary {} is only {} bytes; this looks like a fixture, not the real Core",
        binary.display(),
        metadata.len()
    );
    let mut magic = [0_u8; 4];
    File::open(&binary)
        .and_then(|mut file| file.read_exact(&mut magic))
        .with_context(|| format!("read ELF header of {}", binary.display()))?;
    ensure!(
        magic == *b"\x7fELF",
        "Core binary {} is not an ELF executable",
        binary.display()
    );
    Ok(binary)
}

// ---------------------------------------------------------------------------
// private paths and environment
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
    log: PathBuf,
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
            log: root.join("core.log"),
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
        ] {
            fs::create_dir_all(directory)
                .with_context(|| format!("create {}", directory.display()))?;
            set_private_dir(directory)?;
        }
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

fn core_environment(paths: &LivePaths, socket: &Path, release: &str) -> BTreeMap<String, String> {
    let mut environment = minimal_environment(paths);
    environment.insert(
        "ECHO_DEPLOY_CONTROL_SOCKET".to_string(),
        socket.display().to_string(),
    );
    environment.insert("ECHO_RELEASE_ID".to_string(), release.to_string());
    environment
}

fn choose_loopback_port(forbidden: &[u16]) -> Result<u16> {
    for _ in 0..32 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("bind ephemeral loopback listener")?;
        let port = listener
            .local_addr()
            .context("read ephemeral loopback port")?
            .port();
        drop(listener);
        if port != 0 && !forbidden.contains(&port) {
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
        "config template must contain at least one @TMP_ROOT@ placeholder"
    );
    ensure!(
        template.matches("@MANAGEMENT_PORT@").count() >= 1,
        "config template must contain at least one @MANAGEMENT_PORT@ placeholder"
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
        ("disabled QQ", "enabled = false".to_string()),
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
        !rendered
            .lines()
            .any(|line| line.trim_start().starts_with("[agent.teams")),
        "rendered config must not define agent personas"
    );
    ensure!(
        !rendered.contains("/home/evence/.config")
            && !rendered.contains("/home/evence/.local/share/echo-agent-core/config"),
        "rendered config must not point at a real user config"
    );
    ensure!(
        !rendered.contains("DEEPSEEK_API_KEY")
            && !rendered.contains("ANTHROPIC_API_KEY")
            && !rendered.contains("OPENAI_API_KEY"),
        "rendered config must not contain credential variable names"
    );
    Ok(rendered)
}

// ---------------------------------------------------------------------------
// live process
// ---------------------------------------------------------------------------

struct LiveCore {
    temp: Option<TempDir>,
    paths: LivePaths,
    child: Option<Child>,
    binary: PathBuf,
    binary_sha256: String,
    core_version: String,
    port: u16,
    environment: BTreeMap<String, String>,
    started: Instant,
    control_timeout: Duration,
    startup_timeout: Duration,
    shutdown_timeout: Duration,
    total_timeout: Duration,
}

#[derive(Debug, serde::Serialize)]
struct ShutdownEvidence {
    exit_status: String,
    process_gone: bool,
    socket_removed: bool,
    port_rebindable: bool,
    tempdir_removed: bool,
    elapsed_seconds: f64,
}

impl LiveCore {
    fn start(manifest: &Manifest, binary: PathBuf, binary_sha256: String) -> Result<Self> {
        let temp = tempfile::Builder::new()
            .prefix("echo-deploy-core-live-")
            .tempdir()
            .context("create private live-Core tempdir")?;
        let paths = LivePaths::create(temp.path())?;
        let port = choose_loopback_port(&manifest.forbidden_ports)?;
        let rendered = render_config(
            &fixtures_dir().join(&manifest.config_template),
            &paths,
            port,
        )?;
        write_private_file(&paths.config_file, rendered.as_bytes())?;
        let environment = core_environment(&paths, &paths.socket, &manifest.release_id);

        let core_version = check_core_help(
            &binary,
            Duration::from_secs(manifest.max_help_seconds),
            &paths,
        )?;

        let log = File::create(&paths.log)
            .with_context(|| format!("create Core log {}", paths.log.display()))?;
        fs::set_permissions(&paths.log, fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
        let log_stderr = log
            .try_clone()
            .with_context(|| format!("clone Core log {}", paths.log.display()))?;

        let child = Command::new(&binary)
            .arg("-c")
            .arg(&paths.config_file)
            .current_dir(&paths.work)
            .env_clear()
            .envs(&environment)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_stderr))
            .spawn()
            .with_context(|| format!("spawn real Core {}", binary.display()))?;
        let started = Instant::now();
        let mut live = Self {
            temp: Some(temp),
            paths,
            child: Some(child),
            binary,
            binary_sha256,
            core_version,
            port,
            environment,
            started,
            control_timeout: manifest.control_timeout(),
            startup_timeout: manifest.startup_timeout(),
            shutdown_timeout: manifest.shutdown_timeout(),
            total_timeout: manifest.total_timeout(),
        };
        live.wait_for_socket()?;
        Ok(live)
    }

    fn pid(&self) -> Result<u32> {
        self.child
            .as_ref()
            .map(Child::id)
            .context("live Core child is not running")
    }

    fn wait_for_socket(&mut self) -> Result<()> {
        let deadline = self.started + self.startup_timeout;
        loop {
            if let Some(status) = self.child.as_mut().context("child missing")?.try_wait()? {
                bail!(
                    "real Core exited before the control socket was ready: {status}\n{}",
                    self.log_tail()
                );
            }
            if self.paths.socket.exists() {
                match UnixStream::connect(&self.paths.socket) {
                    Ok(_) => {
                        let elapsed = self.started.elapsed();
                        ensure!(
                            elapsed <= self.total_timeout,
                            "Core startup exceeded the total test budget"
                        );
                        return Ok(());
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound
                                | std::io::ErrorKind::ConnectionRefused
                                | std::io::ErrorKind::PermissionDenied
                        ) => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "connect to Core control socket {}",
                                self.paths.socket.display()
                            )
                        })
                    }
                }
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out after {:?} waiting for Core control socket {}\n{}",
                    self.startup_timeout,
                    self.paths.socket.display(),
                    self.log_tail()
                );
            }
            ensure!(
                self.started.elapsed() <= self.total_timeout,
                "Core startup exceeded the total test budget"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn query(
        &self,
        operation: CoreOperation,
        transaction_id: &str,
        expected_release: Option<&str>,
    ) -> Result<Value> {
        ensure!(
            self.started.elapsed() <= self.total_timeout,
            "live Core test exceeded its {:?} budget",
            self.total_timeout
        );
        query_core(
            &self.paths.socket,
            operation,
            transaction_id,
            expected_release,
            self.control_timeout,
        )
        .with_context(|| {
            format!(
                "Core {operation:?} transaction {transaction_id:?} failed\n{}",
                self.log_tail()
            )
        })
    }

    fn log_tail(&self) -> String {
        match fs::read_to_string(&self.paths.log) {
            Ok(text) => {
                let tail: Vec<&str> = text.lines().rev().take(120).collect();
                let mut tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
                if tail.len() > 24 * 1024 {
                    tail = tail[tail.len() - 24 * 1024..].to_string();
                }
                format!("--- Core log tail (last 120 lines) ---\n{tail}")
            }
            Err(error) => format!(
                "(could not read Core log {}: {error})",
                self.paths.log.display()
            ),
        }
    }

    fn shutdown(&mut self) -> Result<ShutdownEvidence> {
        let started = Instant::now();
        let pid = self.pid()?;
        let mut child = self.child.take().context("live Core child already taken")?;
        let mut exited = child.try_wait()?.is_some();
        if !exited {
            signal_process_group(pid, libc::SIGTERM)?;
            let deadline = Instant::now() + self.shutdown_timeout;
            while Instant::now() < deadline {
                if child.try_wait()?.is_some() {
                    exited = true;
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        if !exited {
            signal_process_group(pid, libc::SIGKILL)?;
        }
        let status = child.wait().context("reap Core child")?;
        ensure!(
            status.success(),
            "Core exited with {status}; expected clean shutdown after SIGTERM\n{}",
            self.log_tail()
        );
        let process_gone = wait_until_gone(pid, Duration::from_secs(5))?;
        ensure!(process_gone, "Core process {pid} is still visible in /proc");
        let socket_removed = wait_until(|| !self.paths.socket.exists(), Duration::from_secs(5))?;
        ensure!(
            socket_removed,
            "Core control socket {} was not removed on shutdown",
            self.paths.socket.display()
        );
        let port_rebindable = TcpListener::bind((Ipv4Addr::LOCALHOST, self.port)).is_ok();
        ensure!(
            port_rebindable,
            "management port {} is still bound after Core exit",
            self.port
        );
        let root = self.paths.root.clone();
        let tempdir_removed = match self.temp.take() {
            Some(temp) => {
                temp.close()
                    .with_context(|| format!("remove private tempdir {}", root.display()))?;
                !root.exists()
            }
            None => false,
        };
        ensure!(
            tempdir_removed,
            "private tempdir {} was not removed",
            root.display()
        );
        Ok(ShutdownEvidence {
            exit_status: format!("{status}"),
            process_gone,
            socket_removed,
            port_rebindable,
            tempdir_removed,
            elapsed_seconds: started.elapsed().as_secs_f64(),
        })
    }
}

impl Drop for LiveCore {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            if child.try_wait().ok().flatten().is_none() {
                let _ = signal_process_group(pid, libc::SIGTERM);
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => thread::sleep(Duration::from_millis(20)),
                        Err(_) => break,
                    }
                }
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                }
            }
            let _ = child.wait();
        }
        // `temp` drops here and removes the private tree; no production path is
        // ever recorded in it.
    }
}

fn signal_process_group(pid: u32, signal: libc::c_int) -> Result<()> {
    // The child was started with process_group(0), so its pgid == pid.  Kill the
    // group so no module/adaptor descendant could outlive the Core process.
    let result = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).with_context(|| format!("send signal {signal} to process group {pid}"))
}

fn wait_until_gone(pid: u32, timeout: Duration) -> Result<bool> {
    wait_until(|| !Path::new(&format!("/proc/{pid}")).exists(), timeout)
}

fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// CLI and process-image checks
// ---------------------------------------------------------------------------

fn check_core_help(binary: &Path, timeout: Duration, paths: &LivePaths) -> Result<String> {
    let environment = minimal_environment(paths);
    let help = run_bounded(
        binary,
        &["--help"],
        timeout,
        &paths.work,
        &environment,
        "Core --help",
    )?;
    ensure!(
        help.status.success(),
        "Core --help failed with {}\nstdout:\n{}\nstderr:\n{}",
        help.status,
        String::from_utf8_lossy(&help.stdout),
        String::from_utf8_lossy(&help.stderr)
    );
    let stdout = String::from_utf8_lossy(&help.stdout);
    for needle in ["EchoAgentCore", "dump-config", "module-upgrade"] {
        ensure!(
            stdout.contains(needle),
            "Core --help output does not look like the real Core (missing {needle:?}):\n{stdout}"
        );
    }
    let version = run_bounded(
        binary,
        &["--version"],
        timeout,
        &paths.work,
        &environment,
        "Core --version",
    )?;
    ensure!(
        version.status.success(),
        "Core --version failed with {}",
        version.status
    );
    let version_stdout = String::from_utf8_lossy(&version.stdout);
    ensure!(
        version_stdout.contains("echo-agent-core"),
        "Core --version output does not name echo-agent-core: {version_stdout}"
    );
    Ok(version_stdout.trim().to_string())
}

fn run_bounded(
    binary: &Path,
    arguments: &[&str],
    timeout: Duration,
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    label: &str,
) -> Result<Output> {
    let mut child = Command::new(binary)
        .args(arguments)
        .current_dir(cwd)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {label}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child
                .wait_with_output()
                .with_context(|| format!("collect {label} output"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{label} timed out after {timeout:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
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

fn peer_credentials(socket: &Path) -> Result<(u32, u32)> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("connect for SO_PEERCRED {}", socket.display()))?;
    // SAFETY: zeroed ucred is valid for getsockopt to fill; the fd lives for the
    // duration of the call and `length` matches the allocation.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    ensure!(
        result == 0,
        "getsockopt(SO_PEERCRED) failed: {}",
        std::io::Error::last_os_error()
    );
    ensure!(credentials.pid > 0, "Core control socket peer has no pid");
    Ok((credentials.uid, credentials.pid as u32))
}

// ---------------------------------------------------------------------------
// /proc isolation checks
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
struct IsolationEvidence {
    test_cgroup: String,
    core_cgroup: String,
    peer_pid: u32,
    peer_uid: u32,
    peer_exe: String,
    peer_exe_sha256: String,
    configured_binary: String,
    configured_sha256: String,
    cwd: String,
    config_sha256: String,
    environment_keys: Vec<String>,
    listeners: Vec<String>,
    owned_tcp_sockets: usize,
    owned_udp_sockets: usize,
    child_processes: Vec<u32>,
}

fn inspect_isolation(
    live: &LiveCore,
    test_cgroup: &str,
    expected_uid: u32,
) -> Result<IsolationEvidence> {
    let pid = live.pid()?;
    let (peer_uid, peer_pid) = peer_credentials(&live.paths.socket)?;
    ensure!(
        peer_uid == expected_uid && peer_uid == current_euid(),
        "control peer uid {peer_uid} does not match test euid {expected_uid}"
    );
    ensure!(
        peer_pid == pid,
        "control socket peer pid {peer_pid} is not the spawned Core child {pid}"
    );

    let core_cgroup = unified_cgroup(pid)?;
    ensure!(!core_cgroup.is_empty(), "Core cgroup path is empty");
    ensure!(
        core_cgroup == test_cgroup,
        "Core cgroup {core_cgroup:?} differs from test cgroup {test_cgroup:?}; \
         the test must start Core inside its own transient scope"
    );
    for unit in PROTECTED_UNITS {
        ensure!(
            !test_cgroup.contains(unit) && !core_cgroup.contains(unit),
            "refusing to run the live Core smoke inside protected unit {unit}: \
             test cgroup {test_cgroup:?}, Core cgroup {core_cgroup:?}"
        );
    }

    let peer_exe = fs::read_link(format!("/proc/{pid}/exe"))
        .with_context(|| format!("read /proc/{pid}/exe"))?;
    let peer_exe = fs::canonicalize(&peer_exe)
        .with_context(|| format!("canonicalize /proc/{pid}/exe {}", peer_exe.display()))?;
    ensure!(
        peer_exe == live.binary,
        "running Core image {} is not the configured binary {}",
        peer_exe.display(),
        live.binary.display()
    );
    let peer_exe_sha256 = sha256_file(&peer_exe)?;
    ensure!(
        peer_exe_sha256 == live.binary_sha256,
        "running image SHA-256 {peer_exe_sha256} != configured binary SHA-256 {}",
        live.binary_sha256
    );
    let configured_meta = fs::metadata(&live.binary)?;
    let running_meta = fs::metadata(format!("/proc/{pid}/exe"))?;
    ensure!(
        configured_meta.dev() == running_meta.dev() && configured_meta.ino() == running_meta.ino(),
        "running /proc/{pid}/exe inode differs from the configured binary"
    );

    let cwd = fs::read_link(format!("/proc/{pid}/cwd"))
        .with_context(|| format!("read /proc/{pid}/cwd"))?;
    ensure!(
        cwd == live.paths.work,
        "Core cwd {} is not the private work directory {}",
        cwd.display(),
        live.paths.work.display()
    );

    let environment_keys = inspect_environment(pid, live)?;
    let config_bytes = fs::read(&live.paths.config_file)?;
    let config_sha256 = format!("{:x}", Sha256::digest(&config_bytes));

    let (listeners, owned_tcp_sockets, owned_udp_sockets) = inspect_sockets(pid, live.port)?;
    let child_processes = inspect_children(pid)?;
    inspect_log(live)?;

    Ok(IsolationEvidence {
        test_cgroup: test_cgroup.to_string(),
        core_cgroup,
        peer_pid,
        peer_uid,
        peer_exe: peer_exe.display().to_string(),
        peer_exe_sha256,
        configured_binary: live.binary.display().to_string(),
        configured_sha256: live.binary_sha256.clone(),
        cwd: cwd.display().to_string(),
        config_sha256,
        environment_keys,
        listeners,
        owned_tcp_sockets,
        owned_udp_sockets,
        child_processes,
    })
}

fn unified_cgroup(pid: u32) -> Result<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .with_context(|| format!("read /proc/{pid}/cgroup"))?;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            return Ok(path.to_string());
        }
    }
    bail!("no unified cgroup entry for pid {pid}: {text:?}")
}

fn inspect_environment(pid: u32, live: &LiveCore) -> Result<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/environ"))
        .with_context(|| format!("read /proc/{pid}/environ"))?;
    let mut keys = Vec::new();
    let mut values = BTreeMap::new();
    for entry in raw
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let text = std::str::from_utf8(entry).context("Core environment is not UTF-8")?;
        let (key, value) = text
            .split_once('=')
            .with_context(|| format!("malformed environment entry {text:?}"))?;
        let upper = key.to_ascii_uppercase();
        for forbidden in [
            "API_KEY",
            "TOKEN",
            "SECRET",
            "PASSWORD",
            "CREDENTIAL",
            "PROXY",
        ] {
            ensure!(
                !upper.contains(forbidden),
                "Core inherited a credential-like environment variable {key:?}"
            );
        }
        keys.push(key.to_string());
        values.insert(key.to_string(), value.to_string());
    }
    keys.sort();
    keys.dedup();
    ensure!(
        keys == live.environment.keys().cloned().collect::<Vec<_>>(),
        "Core environment keys {keys:?} do not match the env_clear allowlist {:?}",
        live.environment.keys().collect::<Vec<_>>()
    );
    for (key, expected) in &live.environment {
        ensure!(
            values.get(key) == Some(expected),
            "Core environment {key} is {:?}, expected {expected:?}",
            values.get(key)
        );
    }
    for key in [
        "HOME",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        let value = values.get(key).with_context(|| format!("missing {key}"))?;
        ensure!(
            Path::new(value).is_absolute() && Path::new(value).starts_with(&live.paths.root),
            "{key} {value:?} is outside the private temp root"
        );
    }
    Ok(keys)
}

#[derive(Debug)]
struct NetSocket {
    local_ip: String,
    local_port: u16,
    remote_ip: String,
    remote_port: u16,
    state: String,
    inode: String,
}

fn inspect_sockets(pid: u32, expected_port: u16) -> Result<(Vec<String>, usize, usize)> {
    let inodes = socket_inodes(pid)?;
    let mut listeners = Vec::new();
    let mut tcp_count = 0_usize;
    for socket in proc_net_sockets(pid, "tcp")? {
        if !inodes.contains(&socket.inode) {
            continue;
        }
        tcp_count += 1;
        ensure!(
            socket.state == "0A",
            "Core owns a non-listening TCP socket local={}:{} remote={}:{} state={}",
            socket.local_ip,
            socket.local_port,
            socket.remote_ip,
            socket.remote_port,
            socket.state
        );
        ensure!(
            socket.local_ip == "127.0.0.1" && socket.local_port == expected_port,
            "Core TCP listener is {}:{}, expected 127.0.0.1:{expected_port}",
            socket.local_ip,
            socket.local_port
        );
        ensure!(
            socket.remote_ip == "0.0.0.0" && socket.remote_port == 0,
            "Core TCP listener has unexpected remote address"
        );
        listeners.push(format!("{}:{}", socket.local_ip, socket.local_port));
    }
    ensure!(
        tcp_count == 1,
        "Core must own exactly one TCP listener (the random management port), found {tcp_count}: {listeners:?}"
    );
    ensure!(
        listeners == vec![format!("127.0.0.1:{expected_port}")],
        "Core listeners {listeners:?} do not match the random management port"
    );
    for protocol in ["tcp6", "udp", "udp6"] {
        let owned = proc_net_sockets(pid, protocol)?
            .into_iter()
            .filter(|socket| inodes.contains(&socket.inode))
            .count();
        ensure!(
            owned == 0,
            "Core owns {owned} {protocol} socket(s); only the IPv4 loopback management listener is allowed"
        );
    }
    Ok((listeners, tcp_count, 0_usize))
}

fn socket_inodes(pid: u32) -> Result<BTreeSet<String>> {
    let mut inodes = BTreeSet::new();
    let directory = format!("/proc/{pid}/fd");
    for entry in fs::read_dir(&directory).with_context(|| format!("read {directory}"))? {
        let entry = entry?;
        let target = match fs::read_link(entry.path()) {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("readlink {}", entry.path().display()))
            }
        };
        let text = target.to_string_lossy();
        if let Some(inode) = text
            .strip_prefix("socket:[")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            inodes.insert(inode.to_string());
        }
    }
    Ok(inodes)
}

fn proc_net_sockets(pid: u32, protocol: &str) -> Result<Vec<NetSocket>> {
    let path = format!("/proc/{pid}/net/{protocol}");
    let text = fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
    let mut sockets = Vec::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 {
            continue;
        }
        let (local_ip, local_port) = parse_proc_address(fields[1])?;
        let (remote_ip, remote_port) = parse_proc_address(fields[2])?;
        sockets.push(NetSocket {
            local_ip,
            local_port,
            remote_ip,
            remote_port,
            state: fields[3].to_string(),
            inode: fields[9].to_string(),
        });
    }
    Ok(sockets)
}

fn parse_proc_address(value: &str) -> Result<(String, u16)> {
    let (address, port) = value
        .split_once(':')
        .with_context(|| format!("malformed /proc/net address {value:?}"))?;
    let port = u16::from_str_radix(port, 16)
        .with_context(|| format!("malformed /proc/net port {port:?}"))?;
    match address.len() {
        8 => {
            let raw = u32::from_str_radix(address, 16)
                .with_context(|| format!("malformed /proc/net IPv4 address {address:?}"))?;
            let [a, b, c, d] = raw.to_le_bytes();
            Ok((format!("{a}.{b}.{c}.{d}"), port))
        }
        32 => Ok((format!("ipv6:{address}"), port)),
        other => bail!("unexpected /proc/net address length {other} in {value:?}"),
    }
}

fn inspect_children(pid: u32) -> Result<Vec<u32>> {
    let path = format!("/proc/{pid}/task/{pid}/children");
    let text = fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
    let children: Vec<u32> = text
        .split_whitespace()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("parse {path}: {text:?}"))?;
    ensure!(
        children.is_empty(),
        "Core spawned child processes {children:?}; with zero personas and QQ disabled it must not"
    );
    Ok(children)
}

fn inspect_log(live: &LiveCore) -> Result<()> {
    let log = fs::read_to_string(&live.paths.log)
        .with_context(|| format!("read Core log {}", live.paths.log.display()))?;
    ensure!(
        log.contains("deployment control socket listening"),
        "Core log does not confirm the deployment control socket\n{}",
        live.log_tail()
    );
    ensure!(
        log.contains(&format!("127.0.0.1:{}", live.port)),
        "Core log does not confirm management listener 127.0.0.1:{}\n{}",
        live.port,
        live.log_tail()
    );
    for forbidden in ["0.0.0.0:3132", "0.0.0.0:3133"] {
        ensure!(
            !log.contains(forbidden),
            "Core log contains production listener {forbidden}\n{}",
            live.log_tail()
        );
    }
    ensure!(
        log.contains("adapter instances launch as modules instances=0"),
        "Core did not report zero adapter instances (QQ must stay disabled)\n{}",
        live.log_tail()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// response helpers
// ---------------------------------------------------------------------------

fn get_bool(response: &Value, key: &str) -> Result<bool> {
    response
        .get(key)
        .and_then(Value::as_bool)
        .with_context(|| format!("Core response is missing boolean {key:?}"))
}

fn get_str<'a>(response: &'a Value, key: &str) -> Result<&'a str> {
    response
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Core response is missing string {key:?}"))
}

fn get_u64(response: &Value, key: &str) -> Result<u64> {
    response
        .get(key)
        .and_then(Value::as_u64)
        .with_context(|| format!("Core response is missing integer {key:?}"))
}

fn get_object<'a>(response: &'a Value, key: &str) -> Result<&'a Map<String, Value>> {
    response
        .get(key)
        .and_then(Value::as_object)
        .with_context(|| format!("Core response is missing object {key:?}"))
}

fn get_array<'a>(response: &'a Value, key: &str) -> Result<&'a [Value]> {
    response
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .with_context(|| format!("Core response is missing array {key:?}"))
}

fn map_array<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a [Value]> {
    object
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .with_context(|| format!("Core object is missing array {key:?}"))
}

fn string_set(values: &[Value], label: &str) -> Result<BTreeSet<String>> {
    let mut set = BTreeSet::new();
    for value in values {
        let text = value
            .as_str()
            .with_context(|| format!("{label} contains a non-string entry"))?;
        ensure!(!text.is_empty(), "{label} contains an empty string");
        set.insert(text.to_string());
    }
    Ok(set)
}

fn ensure_phase(response: &Value, expected: &str) -> Result<()> {
    let phase = get_str(response, "phase")?;
    ensure!(
        phase == expected,
        "Core phase is {phase:?}, expected {expected:?}"
    );
    Ok(())
}

fn ensure_failure_code(response: &Value, expected: &str) -> Result<()> {
    ensure!(
        !get_bool(response, "ok")?,
        "Core unexpectedly accepted a request that must fail with {expected}"
    );
    let error = get_object(response, "error")?;
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .context("failed Core response has no error.code")?;
    ensure!(
        code == expected,
        "Core failure code is {code:?}, expected {expected:?}: {response}"
    );
    Ok(())
}

fn check_writer_scope(scope: &Value, paused: bool, manifest: &Manifest) -> Result<()> {
    ensure!(
        get_bool(scope, "all_controlled_writers_paused")? == paused,
        "writer_scope.all_controlled_writers_paused is not {paused}"
    );
    ensure!(
        get_bool(scope, "management_commands_quiesced")? == paused,
        "writer_scope.management_commands_quiesced is not {paused}"
    );
    let covered = string_set(get_array(scope, "covered")?, "writer_scope.covered")?;
    for expected in &manifest.writer_scope_covered {
        ensure!(
            covered.contains(expected),
            "writer_scope.covered is missing {expected:?}: {covered:?}"
        );
    }
    let not_covered = string_set(get_array(scope, "not_covered")?, "writer_scope.not_covered")?;
    for expected in &manifest.writer_scope_not_covered {
        ensure!(
            not_covered.contains(expected),
            "writer_scope.not_covered is missing {expected:?}: {not_covered:?}"
        );
    }
    Ok(())
}

fn check_retirements(response: &Value) -> Result<()> {
    let retirements = get_object(response, "off_register_retirements")?;
    ensure!(
        retirements.get("checked").and_then(Value::as_bool) == Some(true),
        "off_register_retirements.checked is not true"
    );
    for key in ["failed", "retiring_ids"] {
        let values = retirements
            .get(key)
            .and_then(Value::as_array)
            .with_context(|| format!("off_register_retirements.{key} must be an array"))?;
        ensure!(
            values.is_empty(),
            "off_register_retirements.{key} is not empty: {values:?}"
        );
    }
    Ok(())
}

fn check_requires(response: &Value, expected: &[String]) -> Result<()> {
    let requires = string_set(get_array(response, "requires")?, "requires")?;
    for required in expected {
        ensure!(
            requires.contains(required),
            "Core response requires {requires:?}, missing {required:?}"
        );
    }
    Ok(())
}

fn check_agents(response: &Value, generation: u64) -> Result<Vec<String>> {
    let agents = get_array(response, "agents")?;
    ensure!(
        !agents.is_empty(),
        "prepare response has no agent acknowledgements"
    );
    let mut ids = Vec::new();
    for agent in agents {
        let id = get_str(agent, "id")?;
        ids.push(id.to_string());
        ensure!(
            get_u64(agent, "generation")? == generation,
            "agent {id:?} generation does not match the deployment generation {generation}"
        );
        ensure!(get_bool(agent, "prepared")?, "agent {id:?} is not prepared");
        ensure!(
            get_bool(agent, "writers_paused")?,
            "agent {id:?} did not pause writers"
        );
        ensure!(
            get_str(agent, "flush")? == "checked_ok",
            "agent {id:?} flush is not checked_ok"
        );
        if let Some(error) = agent.get("error") {
            ensure!(error.is_null(), "agent {id:?} reported error {error}");
        }
    }
    ids.sort();
    ensure!(
        ids == vec!["__core".to_string()],
        "with zero personas the only controlled writer must be __core, got {ids:?}"
    );
    Ok(ids)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModuleEvidenceKind {
    Live,
    AbsentLegacyBinary,
}

impl ModuleEvidenceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::AbsentLegacyBinary => "absent_legacy_binary",
        }
    }
}

fn module_evidence_kind(response: &Value) -> Result<ModuleEvidenceKind> {
    match (
        response.get("modules_verified"),
        response.get("module_state"),
    ) {
        (None, None) => Ok(ModuleEvidenceKind::AbsentLegacyBinary),
        (Some(_), Some(_)) => Ok(ModuleEvidenceKind::Live),
        _ => bail!("Core response has partial module evidence (modules_verified/module_state)"),
    }
}

fn check_module_evidence(
    response: &Value,
    kind: ModuleEvidenceKind,
    required: &[String],
) -> Result<Option<Value>> {
    match kind {
        ModuleEvidenceKind::AbsentLegacyBinary => {
            verify_required_modules(response, &[])
                .context("old Core without live module evidence must be accepted only with no required modules")?;
            Ok(None)
        }
        ModuleEvidenceKind::Live => {
            verify_required_modules(response, required).with_context(|| {
                format!("verify Core live module evidence for required modules {required:?}")
            })?;
            ensure!(
                get_bool(response, "modules_verified")?,
                "Core response reports modules_verified=false"
            );
            let state = get_object(response, "module_state")?;
            ensure!(
                state.get("verified").and_then(Value::as_bool) == Some(true),
                "Core module_state.verified is not true"
            );
            ensure!(
                state.get("source").and_then(Value::as_str) == Some("module_host_registry"),
                "Core module_state.source is not module_host_registry"
            );
            let mounted = map_array(state, "mounted")?;
            let active = map_array(state, "active")?;
            let modules = map_array(state, "modules")?;
            ensure!(
                !mounted.is_empty() && !active.is_empty() && !modules.is_empty(),
                "Core live module evidence is empty"
            );
            ensure!(
                active.iter().all(|module| mounted.contains(module)),
                "Core module_state.active is not a subset of mounted"
            );
            Ok(Some(json!({
                "verified": true,
                "source": "module_host_registry",
                "mounted": mounted.len(),
                "active": active.len(),
                "modules": modules.len(),
                "required": required,
                "missing_required": state.get("missing_required").cloned().unwrap_or(Value::Null),
            })))
        }
    }
}

fn protocol_evidence(response: &Value, key: &str) -> Result<Value> {
    response
        .get(key)
        .cloned()
        .with_context(|| format!("Core response is missing {key:?}"))
}

// ---------------------------------------------------------------------------
// the live exercise
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
struct ProtocolEvidence {
    phase_sequence: Vec<String>,
    first_generation: u64,
    second_generation: u64,
    generation_incremented: bool,
    agent_ids: Vec<String>,
    writer_scope_prepared: Value,
    writer_scope_cancelled: Value,
    ready: bool,
    ready_checks: Value,
    modules_verified: Option<bool>,
    module_evidence: String,
    module_summary: Option<Value>,
    cancel_results: Value,
    wrong_transaction_code: String,
    wrong_generation_code: String,
    cancel_restored_idle: bool,
    mapped_prepare: Value,
    mapped_ready: Value,
    ready_rejected_without_process_verification: bool,
    identity_limitations: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct RunManifest {
    schema_version: u32,
    fixture_manifest: String,
    fixture_manifest_sha256: String,
    config_template: String,
    core_binary: String,
    core_binary_sha256: String,
    core_version: String,
    core_version_identity: String,
    release_id: String,
    binary_claim: String,
    test_cgroup: String,
    isolation: IsolationEvidence,
    protocol: ProtocolEvidence,
    shutdown: ShutdownEvidence,
    temp_root_removed: bool,
}

fn exercise(live: &LiveCore, manifest: &Manifest) -> Result<ProtocolEvidence> {
    let mut phase_sequence = Vec::new();

    let status_initial = live.query(CoreOperation::Status, "core-live-status-initial", None)?;
    ensure_phase(&status_initial, &manifest.expected_phases.status_initial)?;
    phase_sequence.push(get_str(&status_initial, "phase")?.to_string());
    ensure!(get_bool(&status_initial, "ok")?, "initial status is not ok");
    ensure!(
        !get_bool(&status_initial, "prepared")?,
        "initial status must not be prepared"
    );
    ensure!(
        !get_bool(&status_initial, "maintenance_held")?,
        "initial status must not hold maintenance"
    );
    ensure!(
        !get_bool(&status_initial, "can_cancel")?,
        "initial status must not be cancellable"
    );
    let initial_scope = protocol_evidence(&status_initial, "writer_scope")?;
    check_writer_scope(&initial_scope, false, manifest)?;
    let initial_modules = module_evidence_kind(&status_initial)?;
    check_module_evidence(&status_initial, initial_modules, &manifest.required_modules)?;

    let identity = core_identity(&status_initial)?;
    ensure!(
        identity.declared.as_deref() == Some(manifest.release_id.as_str()),
        "Core declared release {:?}, expected {:?}",
        identity.declared,
        manifest.release_id
    );
    ensure!(
        !identity.identity_verified,
        "shipping Core must report declaration-only identity"
    );
    ensure!(
        !identity.limitations.is_empty(),
        "declaration-only identity must expose limitations"
    );

    let prepare_tx = format!("{}-prepare", manifest.transaction_prefix);
    let prepare = live.query(
        CoreOperation::Prepare,
        &prepare_tx,
        Some(&manifest.release_id),
    )?;
    ensure!(get_bool(&prepare, "ok")?, "prepare is not ok: {prepare}");
    ensure_phase(&prepare, &manifest.expected_phases.prepare)?;
    phase_sequence.push(get_str(&prepare, "phase")?.to_string());
    ensure!(
        get_bool(&prepare, "prepared")?,
        "prepare did not report prepared=true"
    );
    ensure!(
        get_bool(&prepare, "maintenance_held")?,
        "prepare did not hold maintenance"
    );
    ensure!(
        get_bool(&prepare, "can_cancel")?,
        "prepare did not report can_cancel=true"
    );
    let first_generation = get_u64(&prepare, "generation")?;
    ensure!(first_generation >= 1, "prepare returned a zero generation");
    let prepared_scope = protocol_evidence(&prepare, "writer_scope")?;
    check_writer_scope(&prepared_scope, true, manifest)?;
    check_retirements(&prepare)?;
    check_requires(&prepare, &manifest.prepare_requires)?;
    let agent_ids = check_agents(&prepare, first_generation)?;

    let status_prepared = live.query(CoreOperation::Status, "core-live-status-prepared", None)?;
    ensure_phase(&status_prepared, &manifest.expected_phases.status_prepared)?;
    phase_sequence.push(get_str(&status_prepared, "phase")?.to_string());
    ensure!(
        get_u64(&status_prepared, "generation")? == first_generation,
        "status generation changed during maintenance"
    );
    ensure!(
        get_bool(&status_prepared, "prepared")?,
        "status does not report prepared during maintenance"
    );
    ensure!(
        get_bool(&status_prepared, "maintenance_held")?,
        "status does not report maintenance_held"
    );
    ensure!(
        get_bool(&status_prepared, "can_cancel")?,
        "status does not report can_cancel"
    );
    let status_scope = protocol_evidence(&status_prepared, "writer_scope")?;
    check_writer_scope(&status_scope, true, manifest)?;

    let second_prepare = live.query(
        CoreOperation::Prepare,
        "core-live-prepare-again",
        Some(&manifest.release_id),
    )?;
    ensure!(
        get_bool(&second_prepare, "ok")?,
        "second prepare while maintenance is held failed unexpectedly"
    );
    ensure!(
        get_u64(&second_prepare, "generation")? == first_generation,
        "concurrent prepare changed the active generation"
    );

    let ready = live.query(
        CoreOperation::Ready,
        "core-live-ready",
        Some(&manifest.release_id),
    )?;
    ensure!(get_bool(&ready, "ok")?, "ready is not ok: {ready}");
    ensure!(
        get_bool(&ready, "ready")? == manifest.require_ready,
        "ready flag does not match the manifest requirement"
    );
    let checks = get_object(&ready, "checks")?;
    for check in [
        "configured_enabled_personas_running",
        "trunks_loaded",
        "no_retiring_personas",
        "release_matches_expected",
    ] {
        ensure!(
            checks.get(check).and_then(Value::as_bool) == Some(true),
            "ready check {check:?} is not true: {checks:?}"
        );
    }
    check_retirements(&ready)?;
    check_requires(&ready, &manifest.ready_requires)?;
    let ready_kind = module_evidence_kind(&ready)?;
    let module_summary = check_module_evidence(&ready, ready_kind, &manifest.required_modules)?;
    if ready_kind == ModuleEvidenceKind::AbsentLegacyBinary {
        eprintln!(
            "NOTE: this Core binary has no live `modules_verified`/`module_state` evidence. \
             The control protocol is validated, but this run cannot claim live module evidence. \
             Re-run after the module-evidence build lands."
        );
    }
    let modules_verified = ready.get("modules_verified").and_then(Value::as_bool);

    let wrong_tx = live.query(
        CoreOperation::cancel(first_generation),
        "core-live-wrong-transaction",
        Some(&manifest.release_id),
    )?;
    ensure_failure_code(&wrong_tx, "transaction_mismatch")?;
    let status_after_wrong_tx = live.query(
        CoreOperation::Status,
        "core-live-status-after-wrong-tx",
        None,
    )?;
    ensure_phase(
        &status_after_wrong_tx,
        &manifest.expected_phases.status_prepared,
    )?;
    ensure!(
        get_bool(&status_after_wrong_tx, "maintenance_held")?,
        "a failed cancel with the wrong transaction released maintenance"
    );

    let wrong_generation = live.query(
        CoreOperation::cancel(first_generation + 1),
        &prepare_tx,
        Some(&manifest.release_id),
    )?;
    ensure_failure_code(&wrong_generation, "generation_mismatch")?;
    let status_after_wrong_generation = live.query(
        CoreOperation::Status,
        "core-live-status-after-wrong-generation",
        None,
    )?;
    ensure_phase(
        &status_after_wrong_generation,
        &manifest.expected_phases.status_prepared,
    )?;
    ensure!(
        get_bool(&status_after_wrong_generation, "maintenance_held")?,
        "a failed cancel with the wrong generation released maintenance"
    );

    let cancel = live.query(
        CoreOperation::cancel(first_generation),
        &prepare_tx,
        Some(&manifest.release_id),
    )?;
    ensure!(get_bool(&cancel, "ok")?, "cancel is not ok: {cancel}");
    ensure_phase(&cancel, &manifest.expected_phases.cancel)?;
    phase_sequence.push(get_str(&cancel, "phase")?.to_string());
    ensure!(
        !get_bool(&cancel, "prepared")?,
        "cancel still reports prepared=true"
    );
    ensure!(
        !get_bool(&cancel, "maintenance_held")?,
        "cancel still reports maintenance_held=true"
    );
    ensure!(
        !get_bool(&cancel, "can_cancel")?,
        "cancel still reports can_cancel=true"
    );
    ensure!(
        cancel.get("generation").is_none_or(Value::is_null),
        "cancel returned a generation"
    );
    let cancelled_scope = protocol_evidence(&cancel, "writer_scope")?;
    check_writer_scope(&cancelled_scope, false, manifest)?;
    let cancel_results = get_array(&cancel, "cancel_results")?;
    ensure!(
        !cancel_results.is_empty(),
        "cancel returned no per-agent results"
    );
    let mut saw_core_release = false;
    for result in cancel_results {
        let result_name = get_str(result, "result")?;
        ensure!(
            matches!(result_name, "released" | "no_match"),
            "cancel result for {:?} is {result_name:?}, expected released/no_match",
            get_str(result, "id")?
        );
        if get_str(result, "id")? == "__core" {
            saw_core_release = true;
            ensure!(
                get_u64(result, "generation")? == first_generation,
                "cancel result generation does not match the prepare ACK"
            );
            ensure!(
                result_name == "released",
                "the __core writer guard was not released"
            );
        }
    }
    ensure!(saw_core_release, "cancel did not return __core result");

    let status_cancelled = live.query(CoreOperation::Status, "core-live-status-cancelled", None)?;
    ensure_phase(
        &status_cancelled,
        &manifest.expected_phases.status_cancelled,
    )?;
    phase_sequence.push(get_str(&status_cancelled, "phase")?.to_string());
    ensure!(
        !get_bool(&status_cancelled, "maintenance_held")?,
        "status still holds maintenance after cancel"
    );
    ensure!(
        status_cancelled
            .get("generation")
            .is_none_or(Value::is_null),
        "status still reports a generation after cancel"
    );
    let idle_scope = protocol_evidence(&status_cancelled, "writer_scope")?;
    check_writer_scope(&idle_scope, false, manifest)?;

    let prepare_tx_second = format!("{}-prepare-second", manifest.transaction_prefix);
    let prepare_second = live.query(
        CoreOperation::Prepare,
        &prepare_tx_second,
        Some(&manifest.release_id),
    )?;
    ensure!(
        get_bool(&prepare_second, "ok")?,
        "prepare after cancel is not ok"
    );
    ensure_phase(&prepare_second, &manifest.expected_phases.prepare)?;
    let second_generation = get_u64(&prepare_second, "generation")?;
    ensure!(
        second_generation > first_generation,
        "generation did not increase after cancel: {first_generation} -> {second_generation}"
    );
    let cancel_second = live.query(
        CoreOperation::cancel(second_generation),
        &prepare_tx_second,
        Some(&manifest.release_id),
    )?;
    ensure!(get_bool(&cancel_second, "ok")?, "second cancel is not ok");
    ensure_phase(&cancel_second, &manifest.expected_phases.cancel)?;
    let status_final = live.query(CoreOperation::Status, "core-live-status-final", None)?;
    ensure_phase(&status_final, &manifest.expected_phases.status_final)?;
    phase_sequence.push(get_str(&status_final, "phase")?.to_string());
    ensure!(
        !get_bool(&status_final, "maintenance_held")?,
        "final status still holds maintenance"
    );

    let prepare_request = ControlRequest {
        protocol: CONTROL_PROTOCOL.into(),
        operation: ControlOperation::PrepareUpgrade,
        instance: "core-live-smoke".into(),
        release: Some(manifest.release_id.clone()),
        manifest_sha256: Some(live.binary_sha256.clone()),
    };
    let mapped_prepare = map_to_systemd_reply(&prepare_request, prepare.clone())?;
    ensure!(
        mapped_prepare.success,
        "adapter mapped prepare to an unsuccessful reply"
    );
    ensure!(
        mapped_prepare.maintenance == Some(true),
        "adapter prepare reply does not hold maintenance"
    );
    let ready_request = ControlRequest {
        protocol: CONTROL_PROTOCOL.into(),
        operation: ControlOperation::Ready,
        instance: "core-live-smoke".into(),
        release: Some(manifest.release_id.clone()),
        manifest_sha256: Some(live.binary_sha256.clone()),
    };
    let rejected = map_to_systemd_reply(&ready_request, ready.clone());
    ensure!(
        rejected.is_err(),
        "Ready must be refused without running-process verification"
    );
    let rejected_text = format!("{:#}", rejected.unwrap_err());
    ensure!(
        rejected_text.contains("identity_verified=false"),
        "Ready refusal does not explain declaration-only identity: {rejected_text}"
    );
    let mapped_ready =
        map_to_systemd_reply_with_process_verification(&ready_request, ready.clone(), true)?;
    ensure!(
        mapped_ready.success && mapped_ready.ready == Some(true),
        "adapter mapped ready to an unsuccessful reply"
    );

    Ok(ProtocolEvidence {
        phase_sequence,
        first_generation,
        second_generation,
        generation_incremented: second_generation > first_generation,
        agent_ids,
        writer_scope_prepared: prepared_scope,
        writer_scope_cancelled: cancelled_scope,
        ready: get_bool(&ready, "ready")?,
        ready_checks: Value::Object(checks.clone()),
        modules_verified,
        module_evidence: ready_kind.as_str().to_string(),
        module_summary,
        cancel_results: Value::Array(cancel_results.to_vec()),
        wrong_transaction_code: "transaction_mismatch".to_string(),
        wrong_generation_code: "generation_mismatch".to_string(),
        cancel_restored_idle: true,
        mapped_prepare: serde_json::to_value(mapped_prepare)?,
        mapped_ready: serde_json::to_value(mapped_ready)?,
        ready_rejected_without_process_verification: true,
        identity_limitations: identity.limitations,
    })
}

// ---------------------------------------------------------------------------
// test
// ---------------------------------------------------------------------------

#[test]
#[ignore = "opt-in live smoke: runs the real echo-agent-core with ECHO_DEPLOY_RUN_CORE_LIVE=1"]
fn real_core_control_socket_isolation_smoke() -> Result<()> {
    let (manifest, manifest_path, template_path) = load_manifest()?;
    if !opted_in(&manifest)? {
        return Ok(());
    }
    let binary = selected_core_binary(&manifest)?;
    let binary_sha256 = sha256_file(&binary)?;
    let test_cgroup = unified_cgroup(std::process::id())?;
    for unit in PROTECTED_UNITS {
        ensure!(
            !test_cgroup.contains(unit),
            "refusing to run inside protected unit {unit} (cgroup {test_cgroup:?}); \
             run under `systemd-run --user --scope` as documented"
        );
    }
    eprintln!(
        "core_live: binary={} sha256={} cgroup={test_cgroup}",
        binary.display(),
        binary_sha256
    );

    let mut live = LiveCore::start(&manifest, binary, binary_sha256)?;
    let exercise_result = exercise(&live, &manifest);
    let isolation_result = inspect_isolation(&live, &test_cgroup, current_euid());
    let shutdown_result = live.shutdown();

    let protocol = exercise_result?;
    let isolation = isolation_result?;
    let shutdown = shutdown_result?;

    let manifest_sha256 = sha256_file(&manifest_path)?;
    let run = RunManifest {
        schema_version: 1,
        fixture_manifest: manifest_path.display().to_string(),
        fixture_manifest_sha256: manifest_sha256,
        config_template: template_path.display().to_string(),
        core_binary: isolation.configured_binary.clone(),
        core_binary_sha256: isolation.configured_sha256.clone(),
        core_version: live.core_version.clone(),
        core_version_identity: format!(
            "{}; running image SHA-256 {} (not build provenance)",
            live.core_version, isolation.peer_exe_sha256
        ),
        release_id: manifest.release_id.clone(),
        binary_claim:
            "observed_running_image_only; not a build provenance or release identity claim"
                .to_string(),
        test_cgroup: test_cgroup.clone(),
        isolation,
        protocol,
        temp_root_removed: shutdown.tempdir_removed,
        shutdown,
    };
    println!(
        "CORE_LIVE_RUN_MANIFEST {}",
        serde_json::to_string(&run).context("serialize run manifest")?
    );
    eprintln!(
        "core_live: PASS phase_sequence={:?} generations={}->{} module_evidence={}",
        run.protocol.phase_sequence,
        run.protocol.first_generation,
        run.protocol.second_generation,
        run.protocol.module_evidence
    );
    Ok(())
}
