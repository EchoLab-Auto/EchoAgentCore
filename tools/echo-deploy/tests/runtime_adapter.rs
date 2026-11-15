//! Integration tests for the runtime control adapter. Core is a local Unix
//! socket fixture; no systemd unit or real service is started.

pub use echo_deploy::{control_client, model, state_lock, systemd};

use echo_deploy::runtime_adapter;

use runtime_adapter::{handle, RuntimeConfig};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::{symlink, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use systemd::{ControlOperation, ControlRequest};

const INSTANCE: &str = "test-instance";
const GATE_PROTOCOL: &str = "management-contract-1";

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

struct Fixture {
    root: tempfile::TempDir,
    config: RuntimeConfig,
    r1_manifest: String,
}

impl Fixture {
    fn new(ack: bool, gate: Option<bool>) -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let base = root.path();
        for dir in ["state", "store", "run"] {
            let path = base.join(dir);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let exe = fs::read(std::env::current_exe().unwrap()).unwrap();
        let mut manifests = Vec::new();
        for release in ["r0", "r1"] {
            manifests.push(make_release(base, release, &exe, None));
        }
        symlink(base.join("store/r0"), base.join("state/current")).unwrap();
        if let Some(maintenance) = gate {
            write_gate(base, maintenance, "r0");
        }
        let config = RuntimeConfig {
            instance_id: INSTANCE.into(),
            core_socket: base.join("run/core.sock"),
            panel_gate_file: gate.map(|_| base.join("gate.json")),
            instance_state: base.join("state"),
            current_selector: base.join("state/current"),
            release_store: base.join("store"),
            required_modules: vec![],
            acknowledge_external_writers_stopped: ack,
            first_install: None,
        };
        Self {
            root,
            config,
            r1_manifest: manifests.remove(1),
        }
    }

    fn base(&self) -> &Path {
        self.root.path()
    }

    fn switch(&self, release: &str) {
        fs::remove_file(&self.config.current_selector).unwrap();
        symlink(
            self.config.release_store.join(release),
            &self.config.current_selector,
        )
        .unwrap();
    }

    fn request(
        &self,
        operation: ControlOperation,
        release: Option<&str>,
        manifest: Option<&str>,
    ) -> ControlRequest {
        ControlRequest {
            protocol: systemd::CONTROL_PROTOCOL.into(),
            operation,
            instance: INSTANCE.into(),
            release: release.map(str::to_owned),
            manifest_sha256: manifest.map(str::to_owned),
        }
    }

    fn write_state(&self, prepared: Option<Value>, maintenance: bool) {
        let mut state = json!({
            "schema_version": 1,
            "instance_id": INSTANCE,
            "maintenance": maintenance
        });
        if let Some(prepared) = prepared {
            state["prepared"] = prepared;
        }
        fs::write(
            self.config
                .instance_state
                .join("runtime-adapter-state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        fs::set_permissions(
            self.config
                .instance_state
                .join("runtime-adapter-state.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
}

fn make_release(base: &Path, release: &str, exe: &[u8], sha_override: Option<&str>) -> String {
    let dir = base.join("store").join(release);
    fs::create_dir_all(dir.join("bin")).unwrap();
    fs::write(dir.join("bin/core"), exe).unwrap();
    fs::set_permissions(dir.join("bin/core"), fs::Permissions::from_mode(0o755)).unwrap();
    let manifest = json!({
        "schema_version": 1,
        "release_id": release,
        "target_os": "linux",
        "target_arch": std::env::consts::ARCH,
        "management_protocol": GATE_PROTOCOL,
        "development": true,
        "sources": [{
            "id": "src",
            "revision": "rev",
            "snapshot_sha256": "a".repeat(64),
            "lock_sha256": "b".repeat(64)
        }],
        "artifacts": [{
            "id": "core",
            "kind": "core",
            "path": "bin/core",
            "sha256": sha_override.unwrap_or(&sha(exe)),
            "size": exe.len(),
            "executable": true,
            "source_id": "src"
        }]
    });
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_path = dir.join("release.json");
    // A previous `make_release` call may have made this 0444; remove it before
    // rewriting so the test can intentionally replace a release manifest.
    match fs::remove_file(&manifest_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove old release.json: {error}"),
    }
    fs::write(&manifest_path, &bytes).unwrap();
    fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o444)).unwrap();
    sha(&bytes)
}

fn write_gate(base: &Path, maintenance: bool, release: &str) {
    fs::write(
        base.join("gate.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "instance_id": INSTANCE,
            "maintenance": maintenance,
            "release_id": release,
            "management_protocol": GATE_PROTOCOL
        }))
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(base.join("gate.json"), fs::Permissions::from_mode(0o600)).unwrap();
}

struct FakeCore {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    path: PathBuf,
}

impl FakeCore {
    fn start(path: PathBuf, mut handler: impl FnMut(Value) -> Value + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let bound = path.clone();
        let listener = UnixListener::bind(&bound).unwrap();
        listener.set_nonblocking(true).unwrap();
        let join = thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => handle_connection(stream, &mut handler),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            stop,
            join: Some(join),
            path,
        }
    }
}

impl Drop for FakeCore {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = UnixStream::connect(&self.path);
        if let Some(join) = self.join.take() {
            join.join().unwrap();
        }
    }
}

fn handle_connection(stream: UnixStream, handler: &mut impl FnMut(Value) -> Value) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let request: Value = serde_json::from_str(line.trim()).unwrap();
    let response = handler(request);
    let mut stream = stream;
    writeln!(stream, "{}", response).unwrap();
    stream.flush().unwrap();
}

fn release_info(declared: &str) -> Value {
    json!({
        "declared": declared,
        "identity_verified": false,
        "verification": "env_echo_release_id_declaration_only"
    })
}

fn status(
    req: &Value,
    declared: &str,
    phase: &str,
    prepared: bool,
    held: bool,
    gen: Option<u64>,
) -> Value {
    json!({
        "ok": true, "operation": "status", "transaction_id": req["transaction_id"],
        "phase": phase, "generation": gen, "prepared": prepared,
        "maintenance_held": held, "release": release_info(declared)
    })
}

fn prepare(req: &Value, declared: &str, generation: u64) -> Value {
    json!({
        "ok": true, "operation": "prepare", "transaction_id": req["transaction_id"],
        "phase": "prepared", "generation": generation, "prepared": true,
        "maintenance_held": true, "can_cancel": true, "release": release_info(declared),
        "writer_scope": {
            "all_controlled_writers_paused": true,
            "management_commands_quiesced": true,
            "covered": ["management_command_writes","in_flight_turns",
                        "periodic_session_save","module_reload_watcher"],
            "not_covered": ["user_arbitrary_subprocesses"]
        },
        "agents": [{"id":"__core","generation":generation,"prepared":true,
                    "writers_paused":true,"flush":"checked_ok"}],
        "off_register_retirements": {"checked":true,"failed":[],"retiring_ids":[]},
        "requires": ["external_stop_user_arbitrary_subprocesses"]
    })
}

fn ready(req: &Value, declared: &str) -> Value {
    json!({
        "ok": true, "operation": "ready", "transaction_id": req["transaction_id"],
        "release": release_info(declared), "ready": true,
        "checks": {
            "configured_enabled_personas_running": true,
            "trunks_loaded": true,
            "no_retiring_personas": true,
            "release_matches_expected": true
        },
        "off_register_retirements": {"checked":true,"failed":[],"retiring_ids":[]},
        "requires": ["external_release_identity_check"]
    })
}

fn cancel(req: &Value, declared: &str) -> Value {
    json!({
        "ok": true, "operation": "cancel", "transaction_id": req["transaction_id"],
        "prepared": false, "maintenance_held": false, "release": release_info(declared)
    })
}

#[test]
fn config_is_strict_and_unknown_fields_are_rejected() {
    let text = r#"{"instance_id":"i","core_socket":"/tmp/s","panel_gate_file":null,
        "instance_state":"/tmp/i","current_selector":"/tmp/c","release_store":"/tmp/r",
        "acknowledge_external_writers_stopped":true,"extra":1}"#;
    let error = serde_json::from_str::<RuntimeConfig>(text).unwrap_err();
    assert!(error.to_string().contains("unknown field"), "{error}");
}

#[test]
fn observe_without_core_uses_selector_and_is_not_ready() {
    let fixture = Fixture::new(false, Some(false));
    let reply = handle(
        &fixture.config,
        fixture.request(ControlOperation::Observe, None, None),
    )
    .unwrap();
    assert_eq!(reply.current_release.as_deref(), Some("r0"));
    assert_eq!(reply.maintenance, Some(false));
    assert_eq!(reply.ready, Some(false));
    assert_eq!(reply.active_work, Some(0));
}

#[test]
fn observe_uses_core_status_and_verifies_running_executable() {
    let fixture = Fixture::new(false, Some(false));
    let _core = FakeCore::start(fixture.config.core_socket.clone(), |request| {
        status(&request, "r0", "idle", false, false, None)
    });
    let reply = handle(
        &fixture.config,
        fixture.request(ControlOperation::Observe, None, None),
    )
    .unwrap();
    assert_eq!(reply.current_release.as_deref(), Some("r0"));
    assert_eq!(reply.ready, Some(true));
}

#[test]
fn prepare_requires_explicit_external_writer_acknowledgement() {
    let fixture = Fixture::new(false, Some(false));
    let error = handle(
        &fixture.config,
        fixture.request(
            ControlOperation::PrepareUpgrade,
            Some("r1"),
            Some(&fixture.r1_manifest),
        ),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("acknowledge_external_writers_stopped"),
        "{error}"
    );
}

#[test]
fn prepare_records_generation_and_drain_requires_prepared_status() {
    let fixture = Fixture::new(true, Some(false));
    let _core = FakeCore::start(fixture.config.core_socket.clone(), |request| match request
        ["operation"]
        .as_str()
        .unwrap()
    {
        "prepare" => prepare(&request, "r0", 7),
        "status" => status(&request, "r0", "prepared", true, true, Some(7)),
        other => panic!("unexpected operation {other}"),
    });
    handle(
        &fixture.config,
        fixture.request(
            ControlOperation::PrepareUpgrade,
            Some("r1"),
            Some(&fixture.r1_manifest),
        ),
    )
    .unwrap();
    let state: Value = serde_json::from_slice(
        &fs::read(
            fixture
                .config
                .instance_state
                .join("runtime-adapter-state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(state["prepared"]["expected_generation"], 7);
    assert_eq!(state["prepared"]["previous"], "r0");
    let reply = handle(
        &fixture.config,
        fixture.request(ControlOperation::Drain, None, None),
    )
    .unwrap();
    assert_eq!(reply.active_work, Some(0));
}

#[test]
fn drain_without_prepared_state_is_refused() {
    let fixture = Fixture::new(true, Some(false));
    let error = handle(
        &fixture.config,
        fixture.request(ControlOperation::Drain, None, None),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("prepared"), "{error}");
}

#[test]
fn maintenance_enter_preserves_declared_gate_identity() {
    let fixture = Fixture::new(true, Some(false));
    handle(
        &fixture.config,
        fixture.request(ControlOperation::MaintenanceEnter, None, None),
    )
    .unwrap();
    let gate: Value =
        serde_json::from_slice(&fs::read(fixture.base().join("gate.json")).unwrap()).unwrap();
    assert_eq!(gate["maintenance"], true);
    assert_eq!(gate["release_id"], "r0");
    assert_eq!(gate["instance_id"], INSTANCE);
    assert_eq!(gate["management_protocol"], GATE_PROTOCOL);
}

#[test]
fn maintenance_leave_cancels_held_generation_and_requires_ready() {
    let fixture = Fixture::new(true, Some(true));
    fixture.write_state(
        Some(json!({
            "transaction_id": "tx-7",
            "expected_generation": 7,
            "release": "r1",
            "manifest_sha256": fixture.r1_manifest.clone(),
            "previous": "r0"
        })),
        true,
    );
    let mut status_calls = 0;
    let _core = FakeCore::start(
        fixture.config.core_socket.clone(),
        move |request| match request["operation"].as_str().unwrap() {
            "status" => {
                status_calls += 1;
                if status_calls == 1 {
                    status(&request, "r0", "prepared", true, true, Some(7))
                } else {
                    status(&request, "r0", "idle", false, false, None)
                }
            }
            "cancel" => {
                assert_eq!(request["expected_generation"], 7);
                assert_eq!(request["transaction_id"], "tx-7");
                cancel(&request, "r0")
            }
            "ready" => ready(&request, "r0"),
            other => panic!("unexpected operation {other}"),
        },
    );
    let reply = handle(
        &fixture.config,
        fixture.request(ControlOperation::MaintenanceLeave, None, None),
    )
    .unwrap();
    assert_eq!(reply.maintenance, Some(false));
    let gate: Value =
        serde_json::from_slice(&fs::read(fixture.base().join("gate.json")).unwrap()).unwrap();
    assert_eq!(gate["maintenance"], false);
    assert_eq!(gate["release_id"], "r0");
    let state: Value = serde_json::from_slice(
        &fs::read(
            fixture
                .config
                .instance_state
                .join("runtime-adapter-state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(state["prepared"], Value::Null);
}

#[test]
fn ready_rejects_running_binary_manifest_mismatch() {
    let fixture = Fixture::new(true, Some(false));
    // Rewrite r1's manifest so its declared Core SHA cannot match the test binary.
    make_release(
        fixture.base(),
        "r1",
        b"different bytes",
        Some(&"0".repeat(64)),
    );
    fixture.switch("r1");
    let _core = FakeCore::start(fixture.config.core_socket.clone(), |request| {
        ready(&request, "r1")
    });
    let error = handle(
        &fixture.config,
        fixture.request(ControlOperation::Ready, Some("r1"), None),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("running executable SHA"),
        "{error}"
    );
}

#[test]
fn ready_verifies_running_binary_and_accepts_core_ready() {
    let fixture = Fixture::new(true, Some(false));
    fixture.switch("r1");
    let _core = FakeCore::start(fixture.config.core_socket.clone(), |request| {
        ready(&request, "r1")
    });
    let reply = handle(
        &fixture.config,
        fixture.request(ControlOperation::Ready, Some("r1"), None),
    )
    .unwrap();
    assert_eq!(reply.ready, Some(true));
    assert_eq!(reply.current_release.as_deref(), Some("r1"));
    assert!(reply.success);
}

#[test]
fn maintenance_leave_refuses_unknown_generation() {
    let fixture = Fixture::new(true, Some(true));
    let _core = FakeCore::start(fixture.config.core_socket.clone(), |request| {
        status(&request, "r0", "prepared", true, true, Some(99))
    });
    let error = handle(
        &fixture.config,
        fixture.request(ControlOperation::MaintenanceLeave, None, None),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("no prepared state"),
        "{error}"
    );
    let gate: Value =
        serde_json::from_slice(&fs::read(fixture.base().join("gate.json")).unwrap()).unwrap();
    assert_eq!(gate["maintenance"], true);
}

#[test]
fn protocol_and_instance_context_are_enforced() {
    let fixture = Fixture::new(true, Some(false));
    let mut request = fixture.request(ControlOperation::Observe, None, None);
    request.protocol = "echo-deploy-control/2".into();
    let error = handle(&fixture.config, request).unwrap_err();
    assert!(format!("{error:#}").contains("unsupported control protocol"));

    let mut request = fixture.request(ControlOperation::Observe, None, None);
    request.instance = "other".into();
    let error = handle(&fixture.config, request).unwrap_err();
    assert!(format!("{error:#}").contains("instance mismatch"));
}

#[test]
fn ready_requires_live_module_evidence_when_modules_are_requested() {
    let mut fixture = Fixture::new(true, None);
    fixture.switch("r1");
    fixture.config.required_modules = vec!["echo-agent.session".into()];
    let _core = FakeCore::start(fixture.config.core_socket.clone(), |request| {
        let mut response = ready(&request, "r1");
        response["modules_verified"] = json!(true);
        response["module_state"] = json!({
            "source": "module_host_registry", "verified": true,
            "mounted": ["echo-agent.session"], "active": ["echo-agent.session"],
            "modules": [{"id": "echo-agent.session", "mounted": true,
                "active": true, "enabled": true, "state": "ready", "error": null}]
        });
        response["checks"]["modules_verified"] = json!(true);
        response["checks"]["required_modules_mounted"] = json!(true);
        response
    });
    let request = fixture.request(ControlOperation::Ready, Some("r1"), None);
    assert!(handle(&fixture.config, request.clone())
        .unwrap()
        .ready
        .unwrap());
    fixture
        .config
        .required_modules
        .push("echo-agent.missing".into());
    let error = handle(&fixture.config, request).unwrap_err();
    assert!(format!("{error:#}").contains("echo-agent.missing"));
}

#[test]
fn ready_cannot_ignore_required_modules_on_old_core() {
    let mut fixture = Fixture::new(true, None);
    fixture.switch("r1");
    fixture.config.required_modules = vec!["echo-agent.session".into()];
    let _core = FakeCore::start(fixture.config.core_socket.clone(), |request| {
        ready(&request, "r1")
    });
    let error = handle(
        &fixture.config,
        fixture.request(ControlOperation::Ready, Some("r1"), None),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("no live module evidence"));
}
