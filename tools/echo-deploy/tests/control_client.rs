//! Behavior tests for the Core control-socket client.
//!
//! No real Core socket and no network are used.  A `UnixListener` fixture
//! stands in for Core, so path validation, `SO_PEERCRED`, timeouts, oversize
//! lines and reply mapping are exercised against the real blocking client code.
//!
//! The module is included directly (the same pattern as `tests/systemd.rs`) so
//! it can be tested before library wiring lands.

mod systemd {
    pub use echo_deploy::systemd::*;
}

#[path = "../src/control_client.rs"]
#[allow(dead_code)]
mod control_client;

use anyhow::Result;
use control_client::{
    core_identity, current_euid, map_to_systemd_reply,
    map_to_systemd_reply_with_process_verification, peer_uid, query_core,
    query_core_expect_peer_uid, verify_required_modules, CoreOperation, MAX_ID_BYTES,
    MAX_RESPONSE_BYTES,
};
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::{
        fs::{symlink, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use systemd::{ControlOperation, ControlRequest, CONTROL_PROTOCOL};
use tempfile::TempDir;

const INSTANCE: &str = "local-dev";
const RELEASE: &str = "release-1";
const DECLARATION_ONLY: &str = "env_echo_release_id_declaration_only";

fn private_dir() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("control.sock");
    (dir, socket)
}

fn accept_once(listener: &UnixListener) -> io::Result<UnixStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                return Ok(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "fixture did not receive a connection",
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn read_request(stream: &mut UnixStream) -> Value {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).expect("read control request");
        assert!(count > 0, "client closed before sending a request");
        if let Some(newline) = buffer[..count].iter().position(|byte| *byte == b'\n') {
            bytes.extend_from_slice(&buffer[..newline]);
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() <= 128 * 1024, "fixture request too large");
    }
    serde_json::from_slice(&bytes).expect("request is JSON")
}

fn write_json_line(stream: &mut UnixStream, value: &Value) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    let _ = stream.write_all(&bytes);
    let _ = stream.flush();
}

fn serve_accepted<F>(socket: &Path, handler: F) -> thread::JoinHandle<()>
where
    F: FnOnce(UnixStream) + Send + 'static,
{
    let listener = UnixListener::bind(socket).unwrap();
    thread::spawn(move || {
        let stream = accept_once(&listener).expect("fixture client did not connect");
        handler(stream);
    })
}

fn serve_once<F>(socket: &Path, handler: F) -> thread::JoinHandle<()>
where
    F: FnOnce(Value, UnixStream) + Send + 'static,
{
    serve_accepted(socket, move |mut stream| {
        let request = read_request(&mut stream);
        handler(request, stream);
    })
}

fn systemd_request(operation: ControlOperation) -> ControlRequest {
    ControlRequest {
        protocol: CONTROL_PROTOCOL.into(),
        operation,
        instance: INSTANCE.into(),
        release: Some(RELEASE.into()),
        manifest_sha256: Some("a".repeat(64)),
    }
}

fn release_info(declared: Option<&str>, identity_verified: bool) -> Value {
    json!({
        "declared": declared,
        "identity_verified": identity_verified,
        "verification": if identity_verified {
            "process_sha256"
        } else {
            DECLARATION_ONLY
        }
    })
}

fn core_prepare_response() -> Value {
    json!({
        "ok": true,
        "operation": "prepare",
        "transaction_id": "tx-prepare",
        "phase": "prepared",
        "generation": 7,
        "prepared": true,
        "maintenance_held": true,
        "can_cancel": true,
        "release": release_info(Some(RELEASE), false),
        "writer_scope": {
            "all_controlled_writers_paused": true,
            "management_commands_quiesced": true,
            "covered": [
                "management_command_writes",
                "in_flight_turns",
                "periodic_session_save",
                "module_reload_watcher"
            ],
            "not_covered": ["user_arbitrary_subprocesses"],
            "note": "user-spawned arbitrary subprocess writers are outside the process write locks"
        },
        "agents": [{
            "id": "__core",
            "generation": 7,
            "prepared": true,
            "writers_paused": true,
            "flush": "checked_ok"
        }],
        "off_register_retirements": {
            "checked": true,
            "failed": [],
            "retiring_ids": []
        },
        "requires": [
            "off_register_retirements_clean",
            "external_stop_user_arbitrary_subprocesses"
        ],
        "message": "controlled writers paused; user subprocesses are NOT covered"
    })
}

fn core_ready_response(identity_verified: bool) -> Value {
    json!({
        "ok": true,
        "operation": "ready",
        "transaction_id": "tx-ready",
        "release": release_info(Some(RELEASE), identity_verified),
        "ready": true,
        "checks": {
            "configured_enabled_personas_running": true,
            "trunks_loaded": true,
            "no_retiring_personas": true,
            "release_matches_expected": true
        },
        "off_register_retirements": {
            "checked": true,
            "failed": [],
            "retiring_ids": []
        },
        "requires": ["external_release_identity_check"],
        "message": "release identity is only the ECHO_RELEASE_ID declaration"
    })
}

fn core_status_response() -> Value {
    json!({
        "ok": true,
        "operation": "status",
        "transaction_id": "tx-status",
        "phase": "idle",
        "prepared": false,
        "maintenance_held": false,
        "can_cancel": false,
        "release": release_info(Some(RELEASE), false)
    })
}

fn module_entry(
    id: &str,
    enabled: bool,
    mounted: bool,
    active: bool,
    state: &str,
    error: Option<&str>,
) -> Value {
    let mut entry = json!({
        "id": id,
        "name": id,
        "version": "0.0.0-test",
        "kind": "tool",
        "runtime": "in_process",
        "builtin": true,
        "enabled": enabled,
        "mounted": mounted,
        "active": active,
        "state": state
    });
    if let Some(error) = error {
        entry["error"] = json!(error);
    }
    entry
}

/// New-Core `status` shape: one live active module, one mounted-but-failed
/// module, and one enabled-but-never-mounted module.  `enabled` is deliberately
/// true on the unmounted entry to prove configuration is not treated as a mount.
fn core_status_with_modules() -> Value {
    let mut response = core_status_response();
    response["modules_verified"] = json!(true);
    response["module_state"] = json!({
        "source": "module_host_registry",
        "verified": true,
        "mounted": ["echo-agent.session", "echo-agent.shell"],
        "active": ["echo-agent.session"],
        "missing_required": [],
        "modules": [
            module_entry("echo-agent.session", true, true, true, "ready", None),
            module_entry("echo-agent.shell", true, true, false, "failed", None),
            module_entry("registered.not_mounted", true, false, false, "", None)
        ]
    });
    response
}

fn core_ready_response_with_modules() -> Value {
    let mut response = core_ready_response(true);
    response["modules_verified"] = json!(true);
    response["module_state"] = core_status_with_modules()["module_state"].clone();
    response["checks"]["modules_verified"] = json!(true);
    response["checks"]["required_modules_mounted"] = json!(true);
    response
}

fn required(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_string()).collect()
}

fn assert_modules_rejected(response: &Value, ids: &[&str], needle: &str) {
    let error = verify_required_modules(response, &required(ids)).unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains(needle),
        "expected {needle:?} in error, got: {text}"
    );
}
fn run_query(response: Value) -> Result<Value> {
    run_query_for(CoreOperation::Status, "tx-env", response)
}

fn run_query_for(operation: CoreOperation, transaction_id: &str, response: Value) -> Result<Value> {
    let (_dir, socket) = private_dir();
    let server = serve_once(&socket, move |_request, mut stream| {
        write_json_line(&mut stream, &response);
    });
    let result = query_core(
        &socket,
        operation,
        transaction_id,
        None,
        Duration::from_secs(2),
    );
    server.join().unwrap();
    result
}

fn assert_prepare_rejected(value: Value, needle: &str) {
    let error = map_to_systemd_reply(&systemd_request(ControlOperation::PrepareUpgrade), value)
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains(needle),
        "expected {needle:?} in error, got: {text}"
    );
}

// ---------------------------------------------------------------------------
// Socket protocol
// ---------------------------------------------------------------------------

#[test]
fn prepare_roundtrip_sends_strict_fields_and_returns_json() {
    let (_dir, socket) = private_dir();
    let server = serve_once(&socket, |request, mut stream| {
        assert_eq!(request["operation"], "prepare");
        assert_eq!(request["transaction_id"], "tx-prepare");
        assert_eq!(request["expected_release"], RELEASE);
        assert!(request.get("expected_generation").is_none());
        write_json_line(&mut stream, &core_prepare_response());
    });

    let response = query_core(
        &socket,
        CoreOperation::Prepare,
        "tx-prepare",
        Some(RELEASE),
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["operation"], "prepare");
    assert_eq!(response["prepared"], true);
    server.join().unwrap();
}

#[test]
fn cancel_sends_generation_and_status_omits_it() {
    let (_dir, socket) = private_dir();
    let server = serve_once(&socket, |request, mut stream| {
        assert_eq!(request["operation"], "cancel");
        assert_eq!(request["transaction_id"], "tx-cancel");
        assert_eq!(request["expected_generation"], 9);
        assert!(request.get("expected_release").is_none());
        write_json_line(
            &mut stream,
            &json!({
                "ok": true,
                "operation": "cancel",
                "transaction_id": "tx-cancel",
                "prepared": false,
                "maintenance_held": false,
                "release": release_info(Some(RELEASE), false)
            }),
        );
    });

    let response = query_core(
        &socket,
        CoreOperation::cancel(9),
        "tx-cancel",
        None,
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(response["operation"], "cancel");
    server.join().unwrap();

    let (_dir, socket) = private_dir();
    let server = serve_once(&socket, |request, mut stream| {
        assert_eq!(request["operation"], "status");
        assert_eq!(request["transaction_id"], "tx-status");
        assert!(request.get("expected_generation").is_none());
        // Old protocol: query_core never sends `required_modules`; the
        // deployment adapter verifies live evidence from the response instead.
        assert!(request.get("required_modules").is_none());
        write_json_line(&mut stream, &core_status_response());
    });
    let response = query_core(
        &socket,
        CoreOperation::Status,
        "tx-status",
        None,
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(response["phase"], "idle");
    server.join().unwrap();
}

#[test]
fn query_and_map_prepare_end_to_end() {
    let (_dir, socket) = private_dir();
    let server = serve_once(&socket, |_request, mut stream| {
        write_json_line(&mut stream, &core_prepare_response());
    });
    let response = query_core(
        &socket,
        CoreOperation::Prepare,
        "tx-prepare",
        Some(RELEASE),
        Duration::from_secs(2),
    )
    .unwrap();
    let reply =
        map_to_systemd_reply(&systemd_request(ControlOperation::PrepareUpgrade), response).unwrap();
    assert!(reply.success);
    assert_eq!(reply.operation, ControlOperation::PrepareUpgrade);
    assert_eq!(reply.instance, INSTANCE);
    assert_eq!(reply.maintenance, Some(true));
    assert_eq!(reply.current_release.as_deref(), Some(RELEASE));
    assert!(reply.error.is_none());
    server.join().unwrap();
}

#[test]
fn peer_uid_is_read_from_socket_credentials() {
    let (left, _right) = UnixStream::pair().unwrap();
    assert_eq!(peer_uid(&left).unwrap(), current_euid());

    let (_dir, socket) = private_dir();
    let server = serve_accepted(&socket, |mut stream| {
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut byte = [0_u8; 1];
        // The client must reject the uid before writing anything; it may just
        // close the connection, which reads as EOF here.
        let _ = stream.read(&mut byte);
    });
    let wrong_uid = current_euid().wrapping_add(1);
    let error = query_core_expect_peer_uid(
        &socket,
        CoreOperation::Status,
        "tx-peer",
        None,
        Duration::from_secs(1),
        wrong_uid,
    )
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("peer uid"), "{text}");
    assert!(text.contains("impostor"), "{text}");
    server.join().unwrap();
}

#[test]
fn silent_peer_times_out_within_the_deadline() {
    let (_dir, socket) = private_dir();
    // Bind but never accept: connect succeeds through the backlog, then the
    // client must stop waiting for a response at its own deadline.
    let _listener = UnixListener::bind(&socket).unwrap();
    let started = Instant::now();
    let error = query_core(
        &socket,
        CoreOperation::Status,
        "tx-timeout",
        None,
        Duration::from_millis(120),
    )
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    let text = format!("{error:#}");
    assert!(text.contains("timed out"), "{text}");
}

#[test]
fn partial_line_then_stall_times_out() {
    let (_dir, socket) = private_dir();
    let server = serve_once(&socket, |_request, mut stream| {
        let _ = stream.write_all(b"{\"ok\":true");
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(300));
    });
    let error = query_core(
        &socket,
        CoreOperation::Status,
        "tx-partial",
        None,
        Duration::from_millis(100),
    )
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("timed out"), "{text}");
    server.join().unwrap();
}

#[test]
fn oversize_response_is_rejected() {
    let (_dir, socket) = private_dir();
    let server = serve_once(&socket, |_request, mut stream| {
        let mut line = vec![b' '; MAX_RESPONSE_BYTES + 1];
        line.push(b'\n');
        let _ = stream.write_all(&line);
        let _ = stream.flush();
    });
    let error = query_core(
        &socket,
        CoreOperation::Status,
        "tx-oversize",
        None,
        Duration::from_secs(2),
    )
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("exceeds"), "{text}");
    server.join().unwrap();
}

#[test]
fn oversize_and_empty_request_fields_are_rejected_before_io() {
    // The socket does not exist; argument validation must win.
    let path = Path::new("/tmp/does-not-exist-echo-control.sock");
    let long_id = "x".repeat(MAX_ID_BYTES + 1);

    let error = query_core(
        path,
        CoreOperation::Status,
        &long_id,
        None,
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("transaction_id"));

    let error = query_core(
        path,
        CoreOperation::Status,
        "tx",
        Some(&long_id),
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("expected_release"));

    let error = query_core(
        path,
        CoreOperation::Cancel {
            expected_generation: 0,
        },
        "tx",
        None,
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("non-zero expected_generation"));

    let error = query_core(path, CoreOperation::Status, "tx", None, Duration::ZERO).unwrap_err();
    assert!(format!("{error:#}").contains("non-zero"));
}

#[test]
fn response_envelope_must_echo_operation_and_transaction() {
    let cases = [
        (
            json!({
                "ok": true,
                "operation": "status",
                "transaction_id": "someone-else",
                "release": release_info(Some(RELEASE), false)
            }),
            "transaction_id",
        ),
        (
            json!({
                "ok": true,
                "operation": "prepare",
                "transaction_id": "tx-env",
                "release": release_info(Some(RELEASE), false)
            }),
            "operation",
        ),
        (
            json!({
                "operation": "status",
                "transaction_id": "tx-env",
                "release": release_info(Some(RELEASE), false)
            }),
            "boolean `ok`",
        ),
        (
            json!({
                "ok": false,
                "operation": "status",
                "transaction_id": "tx-env"
            }),
            "error",
        ),
        (json!("not an object"), "JSON object"),
    ];
    for (response, needle) in cases {
        let error = run_query(response).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains(needle), "expected {needle:?} in {text}");
    }
}

#[test]
fn failed_ack_is_returned_then_rejected_by_mapping() {
    let response = json!({
        "ok": false,
        "operation": "prepare",
        "transaction_id": "tx-fail",
        "error": {
            "code": "release_mismatch",
            "message": "release mismatch: expected \"release-1\", declared \"release-2\""
        }
    });
    let value = run_query_for(CoreOperation::Prepare, "tx-fail", response).unwrap();
    assert_eq!(value["ok"], false);
    let error = map_to_systemd_reply(&systemd_request(ControlOperation::PrepareUpgrade), value)
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("release_mismatch"), "{text}");
    assert!(text.contains("Core rejected prepare"), "{text}");
}

#[test]
fn paths_with_symlinks_public_parents_and_wrong_types_are_rejected() {
    // Missing socket.
    let (_dir, missing) = private_dir();
    let error = query_core(
        &missing,
        CoreOperation::Status,
        "tx",
        None,
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("stat Core control socket"));

    // Relative and non-normalized paths.
    for path in [
        Path::new("control.sock"),
        Path::new("/tmp/../tmp/control.sock"),
    ] {
        let error = query_core(
            path,
            CoreOperation::Status,
            "tx",
            None,
            Duration::from_secs(1),
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("absolute") || text.contains("normalized"),
            "{text}"
        );
    }

    // Too long for a Unix socket path.
    let long_path = PathBuf::from("/tmp")
        .join("x".repeat(120))
        .join("control.sock");
    let error = query_core(
        &long_path,
        CoreOperation::Status,
        "tx",
        None,
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("1..=107"));

    // Regular file instead of a socket.
    let (dir, socket) = private_dir();
    fs::write(&socket, b"not a socket").unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let error = query_core(
        &socket,
        CoreOperation::Status,
        "tx",
        None,
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("not a Unix socket"));
    drop(dir);

    // Symlinked socket leaf.
    let (dir, socket) = private_dir();
    let real = dir.path().join("real.sock");
    let _listener = UnixListener::bind(&real).unwrap();
    symlink(&real, &socket).unwrap();
    let error = query_core(
        &socket,
        CoreOperation::Status,
        "tx",
        None,
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("symlink"));

    // Public parent directory.
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let public = dir.path().join("public");
    fs::create_dir(&public).unwrap();
    fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = public.join("control.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let error = query_core(
        &socket,
        CoreOperation::Status,
        "tx",
        None,
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("not private"));
}

#[test]
fn core_identity_marks_declaration_only_and_rejects_inconsistency() {
    let identity = core_identity(&core_prepare_response()).unwrap();
    assert_eq!(identity.declared.as_deref(), Some(RELEASE));
    assert!(!identity.identity_verified);
    assert_eq!(identity.verification, DECLARATION_ONLY);
    assert!(!identity.limitations.is_empty());
    assert!(
        identity
            .limitations
            .iter()
            .any(|line| line.contains("declared only")),
        "{:?}",
        identity.limitations
    );
    assert!(
        identity
            .limitations
            .iter()
            .any(|line| line.contains("not build or running-process verification")),
        "{:?}",
        identity.limitations
    );

    let mut inconsistent = core_prepare_response();
    inconsistent["release"]["identity_verified"] = json!(true);
    inconsistent["release"]["verification"] = json!(DECLARATION_ONLY);
    let error = core_identity(&inconsistent).unwrap_err();
    assert!(format!("{error:#}").contains("inconsistent"));

    let mut missing = core_prepare_response();
    missing.as_object_mut().unwrap().remove("release");
    assert!(core_identity(&missing).is_err());
}

// ---------------------------------------------------------------------------
// module readiness verification
// ---------------------------------------------------------------------------

#[test]
fn module_verification_accepts_old_core_only_without_required_modules() {
    // Old Core has neither `modules_verified` nor `module_state`.
    verify_required_modules(&core_status_response(), &[]).unwrap();
    verify_required_modules(&core_prepare_response(), &[]).unwrap();
    // Old Ready replies are still compatible too.
    verify_required_modules(&core_ready_response(true), &[]).unwrap();

    // A deployment that requires modules cannot be satisfied by an old Core.
    assert_modules_rejected(
        &core_status_response(),
        &["echo-agent.session"],
        "no live module evidence",
    );
}

#[test]
fn module_verification_accepts_live_mounted_and_active_required_module() {
    let response = core_status_with_modules();
    verify_required_modules(&response, &[]).unwrap();
    verify_required_modules(&response, &required(&["echo-agent.session"])).unwrap();

    // Registered + enabled is not mounted; configuration is not evidence.
    assert_modules_rejected(&response, &["registered.not_mounted"], "not mounted");
    // Mounted but failed is not active.
    assert_modules_rejected(&response, &["echo-agent.shell"], "not active");
    // Not registered at all.
    assert_modules_rejected(&response, &["ghost.module"], "not present");
}

#[test]
fn module_verification_rejects_missing_false_and_partial_fields() {
    let mut value = core_status_with_modules();
    value.as_object_mut().unwrap().remove("modules_verified");
    assert_modules_rejected(&value, &[], "partial module evidence");

    let mut value = core_status_with_modules();
    value.as_object_mut().unwrap().remove("module_state");
    assert_modules_rejected(&value, &[], "partial module evidence");

    let mut value = core_status_with_modules();
    value["modules_verified"] = json!(false);
    assert_modules_rejected(&value, &[], "modules_verified");

    let mut value = core_status_with_modules();
    value["modules_verified"] = Value::Null;
    assert_modules_rejected(&value, &[], "modules_verified");

    let mut value = core_status_with_modules();
    value["module_state"]["verified"] = json!(false);
    assert_modules_rejected(&value, &[], "module_state.verified");

    let mut value = core_status_with_modules();
    value["module_state"] = Value::Null;
    assert_modules_rejected(&value, &[], "module_state");

    let mut value = core_status_with_modules();
    value["module_state"]["modules"] = json!([]);
    assert_modules_rejected(&value, &[], "modules is empty");

    let mut value = core_status_with_modules();
    value["module_state"]
        .as_object_mut()
        .unwrap()
        .remove("mounted");
    assert_modules_rejected(&value, &[], "module_state.mounted");

    let mut value = core_status_with_modules();
    value["module_state"]["modules"][0]
        .as_object_mut()
        .unwrap()
        .remove("mounted");
    assert_modules_rejected(&value, &[], "mounted");
}

#[test]
fn module_verification_rejects_duplicates_and_conflicts() {
    // Duplicate module ids cannot be disambiguated.
    let mut value = core_status_with_modules();
    let duplicate = value["module_state"]["modules"][0].clone();
    value["module_state"]["modules"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert_modules_rejected(&value, &[], "duplicate module id");

    // Duplicate entries in the live set are malformed evidence.
    let mut value = core_status_with_modules();
    value["module_state"]["mounted"] = json!([
        "echo-agent.session",
        "echo-agent.session",
        "echo-agent.shell"
    ]);
    assert_modules_rejected(&value, &[], "duplicate id");

    // Aggregate `mounted` disagrees with `modules[].mounted`.
    let mut value = core_status_with_modules();
    value["module_state"]["modules"][2]["mounted"] = json!(true);
    value["module_state"]["modules"][2]["state"] = json!("pending");
    assert_modules_rejected(&value, &[], "module_state.mounted does not match");

    // A mounted entry that is absent from the aggregate set is a conflict too.
    let mut value = core_status_with_modules();
    value["module_state"]["mounted"] =
        json!(["echo-agent.session", "echo-agent.shell", "ghost.mounted"]);
    assert_modules_rejected(&value, &[], "module_state.mounted does not match");

    // `active` must be a subset of `mounted`.
    let mut value = core_status_with_modules();
    value["module_state"]["active"] = json!(["echo-agent.session", "not.mounted"]);
    assert_modules_rejected(&value, &[], "not a subset");

    // active=true with a non-ready runtime state is contradictory.
    let mut value = core_status_with_modules();
    value["module_state"]["mounted"] = json!(["echo-agent.session", "echo-agent.shell"]);
    value["module_state"]["active"] = json!(["echo-agent.session", "echo-agent.shell"]);
    value["module_state"]["modules"][1]["active"] = json!(true);
    assert_modules_rejected(&value, &[], "active=true");

    // mounted=true + state=ready must be active; active=false is a conflict.
    let mut value = core_status_with_modules();
    value["module_state"]["active"] = json!([]);
    value["module_state"]["modules"][0]["active"] = json!(false);
    assert_modules_rejected(&value, &[], "active=false");

    // Duplicate caller requirements are malformed.
    assert_modules_rejected(
        &core_status_with_modules(),
        &["echo-agent.session", "echo-agent.session"],
        "duplicate required",
    );
}

#[test]
fn module_verification_rejects_required_module_errors() {
    let mut value = core_status_with_modules();
    value["module_state"]["modules"][0]["active"] = json!(false);
    value["module_state"]["modules"][0]["state"] = json!("failed");
    value["module_state"]["modules"][0]["error"] = json!("handshake failed");
    value["module_state"]["active"] = json!([]);
    assert_modules_rejected(&value, &["echo-agent.session"], "reports error");

    // Even a contradictory active=true + error is rejected.
    let mut value = core_status_with_modules();
    value["module_state"]["modules"][0]["error"] = json!("handshake failed");
    assert_modules_rejected(&value, &[], "active but reports error");
}

#[test]
fn ready_mapping_rejects_false_module_checks_even_when_ready_is_true() {
    let request = systemd_request(ControlOperation::Ready);
    let response = core_ready_response_with_modules();
    verify_required_modules(&response, &required(&["echo-agent.session"])).unwrap();
    let reply = map_to_systemd_reply(&request, response).unwrap();
    assert_eq!(reply.ready, Some(true));

    let mut value = core_ready_response_with_modules();
    value["checks"]["modules_verified"] = json!(false);
    assert_modules_rejected(&value, &[], "checks.modules_verified");
    assert!(map_to_systemd_reply(&request, value).is_err());

    let mut value = core_ready_response_with_modules();
    value["checks"]["required_modules_mounted"] = json!(false);
    assert_modules_rejected(&value, &[], "checks.required_modules_mounted");
    assert!(map_to_systemd_reply(&request, value).is_err());

    let mut value = core_ready_response_with_modules();
    value["checks"]["missing_modules"] = json!(["ghost.module"]);
    assert_modules_rejected(&value, &[], "missing_modules");
    assert!(map_to_systemd_reply(&request, value).is_err());

    // Contradictory top-level evidence is refused even with ready=true.
    let mut value = core_ready_response_with_modules();
    value["modules_verified"] = json!(false);
    value["module_state"]["verified"] = json!(false);
    assert!(map_to_systemd_reply(&request, value).is_err());

    // Top-level evidence without the new checks fields is partial.
    let mut value = core_ready_response_with_modules();
    value["checks"]
        .as_object_mut()
        .unwrap()
        .remove("modules_verified");
    value["checks"]
        .as_object_mut()
        .unwrap()
        .remove("required_modules_mounted");
    assert!(map_to_systemd_reply(&request, value).is_err());

    // Checks claiming verification without live evidence are just claims.
    let mut value = core_ready_response_with_modules();
    value.as_object_mut().unwrap().remove("modules_verified");
    value.as_object_mut().unwrap().remove("module_state");
    assert!(map_to_systemd_reply(&request, value).is_err());
}

// ---------------------------------------------------------------------------
// systemd reply mapping
// ---------------------------------------------------------------------------

#[test]
fn prepare_mapping_requires_complete_controlled_writer_scope() {
    let reply = map_to_systemd_reply(
        &systemd_request(ControlOperation::PrepareUpgrade),
        core_prepare_response(),
    )
    .unwrap();
    assert!(reply.success);
    assert_eq!(reply.maintenance, Some(true));

    let mut value = core_prepare_response();
    value["prepared"] = json!(false);
    assert_prepare_rejected(value, "prepared");

    let mut value = core_prepare_response();
    value["maintenance_held"] = json!(false);
    assert_prepare_rejected(value, "maintenance_held");

    let mut value = core_prepare_response();
    value["writer_scope"]["all_controlled_writers_paused"] = json!(false);
    assert_prepare_rejected(value, "all_controlled_writers_paused");

    let mut value = core_prepare_response();
    value["writer_scope"]["management_commands_quiesced"] = json!(false);
    assert_prepare_rejected(value, "management_commands_quiesced");

    let mut value = core_prepare_response();
    value["writer_scope"]["covered"] = json!([]);
    assert_prepare_rejected(value, "covered");

    let mut value = core_prepare_response();
    value["writer_scope"]["not_covered"] = json!(["something_else"]);
    assert_prepare_rejected(value, "user_arbitrary_subprocesses");

    let mut value = core_prepare_response();
    value["agents"] = json!([]);
    assert_prepare_rejected(value, "no agent");

    let mut value = core_prepare_response();
    value["agents"][0]["prepared"] = json!(false);
    assert_prepare_rejected(value, "not prepared");

    let mut value = core_prepare_response();
    value["agents"][0]["writers_paused"] = json!(false);
    assert_prepare_rejected(value, "did not pause");

    let mut value = core_prepare_response();
    value["agents"][0]["error"] = json!("pause failed");
    assert_prepare_rejected(value, "reported an error");

    let mut value = core_prepare_response();
    value["off_register_retirements"]["failed"] = json!(["still-retiring"]);
    assert_prepare_rejected(value, "`failed`");

    let mut value = core_prepare_response();
    value["off_register_retirements"]["retiring_ids"] = json!(["still-retiring"]);
    assert_prepare_rejected(value, "`retiring_ids`");

    let mut value = core_prepare_response();
    value["requires"] = json!(["off_register_retirements_clean"]);
    assert_prepare_rejected(value, "external stopping");

    let mut value = core_prepare_response();
    value["operation"] = json!("status");
    assert_prepare_rejected(value, "cannot satisfy PrepareUpgrade");

    let mut value = core_prepare_response();
    value["release"]["declared"] = json!("release-2");
    assert_prepare_rejected(value, "declares release");

    let mut value = core_prepare_response();
    value.as_object_mut().unwrap().remove("release");
    assert_prepare_rejected(value, "release");

    let mut value = core_prepare_response();
    value.as_object_mut().unwrap().remove("requires");
    assert_prepare_rejected(value, "requires");
}

#[test]
fn ready_fails_closed_without_verified_identity() {
    let request = systemd_request(ControlOperation::Ready);
    let error = map_to_systemd_reply(&request, core_ready_response(false)).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("identity_verified=false"), "{text}");
    assert!(
        text.contains("not build or running-process verification"),
        "{text}"
    );

    // The explicit opt-in is the only escape hatch for a declaration-only
    // Core: the caller must have verified the running process image itself.
    let reply =
        map_to_systemd_reply_with_process_verification(&request, core_ready_response(false), true)
            .unwrap();
    assert!(reply.success);
    assert_eq!(reply.operation, ControlOperation::Ready);
    assert_eq!(reply.ready, Some(true));
    assert_eq!(reply.current_release.as_deref(), Some(RELEASE));

    // A Core that explicitly reports verified identity is accepted without the
    // external proof (the shipping Core never does).
    let reply = map_to_systemd_reply(&request, core_ready_response(true)).unwrap();
    assert_eq!(reply.ready, Some(true));
}

#[test]
fn ready_mapping_requires_complete_checks_and_matching_release() {
    let request = systemd_request(ControlOperation::Ready);

    let mut value = core_ready_response(true);
    value["ready"] = json!(false);
    assert!(map_to_systemd_reply(&request, value).is_err());

    let mut value = core_ready_response(true);
    value["checks"]["trunks_loaded"] = json!(false);
    assert!(map_to_systemd_reply(&request, value).is_err());

    let mut value = core_ready_response(true);
    value["checks"]["configured_enabled_personas_running"] = json!(false);
    assert!(map_to_systemd_reply(&request, value).is_err());

    let mut value = core_ready_response(true);
    value["off_register_retirements"]["failed"] = json!(["x"]);
    assert!(map_to_systemd_reply(&request, value).is_err());

    let mut value = core_ready_response(true);
    value["release"]["declared"] = json!("release-2");
    assert!(map_to_systemd_reply(&request, value).is_err());

    let mut value = core_ready_response(true);
    value["operation"] = json!("status");
    assert!(map_to_systemd_reply(&request, value).is_err());

    let mut request_without_release = systemd_request(ControlOperation::Ready);
    request_without_release.release = None;
    assert!(map_to_systemd_reply(&request_without_release, core_ready_response(true)).is_err());

    // Declaration-only Core cannot become verified merely because checks pass.
    let mut value = core_ready_response(false);
    value["checks"]["trunks_loaded"] = json!(true);
    assert!(map_to_systemd_reply(&request, value).is_err());
}

#[test]
fn mapping_rejects_bad_requests_operations_and_shapes() {
    // Wrong adapter protocol.
    let mut request = systemd_request(ControlOperation::PrepareUpgrade);
    request.protocol = "something-else".into();
    assert!(map_to_systemd_reply(&request, core_prepare_response()).is_err());

    // Unsafe release id in the request.
    let mut request = systemd_request(ControlOperation::PrepareUpgrade);
    request.release = Some("../escape".into());
    assert!(map_to_systemd_reply(&request, core_prepare_response()).is_err());

    // Empty instance.
    let mut request = systemd_request(ControlOperation::PrepareUpgrade);
    request.instance = String::new();
    assert!(map_to_systemd_reply(&request, core_prepare_response()).is_err());

    // Non-object response.
    assert!(map_to_systemd_reply(
        &systemd_request(ControlOperation::PrepareUpgrade),
        json!(null)
    )
    .is_err());

    // Missing `ok`.
    assert!(map_to_systemd_reply(
        &systemd_request(ControlOperation::PrepareUpgrade),
        json!({"operation": "prepare", "transaction_id": "tx"})
    )
    .is_err());

    // Core has no equivalent for these systemd operations; never fabricate.
    for operation in [
        ControlOperation::Observe,
        ControlOperation::Drain,
        ControlOperation::MaintenanceEnter,
        ControlOperation::MaintenanceLeave,
    ] {
        let error =
            map_to_systemd_reply(&systemd_request(operation), core_status_response()).unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("no Core deployment-control equivalent"),
            "{text}"
        );
    }
}
