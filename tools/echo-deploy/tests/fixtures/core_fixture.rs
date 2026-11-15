//! Test-only stand-in for the **Core side** of the `echo-deploy-control`
//! Unix-socket protocol (`prepare` / `cancel` / `status` / `ready`).
//!
//! This is a fixture, **not** Core. Anything a test concludes while this binary
//! serves the socket is a statement about the *deployment adapter protocol*, not
//! about the shipping Core implementation. It never reads real configuration,
//! real user data, secrets or the network; it only binds one Unix socket inside
//! a throw-away temporary directory.
//!
//! Usage: `core_fixture --socket /absolute/path/core.sock`
//!
//! * The declared release id is derived from the executable's own resolved path:
//!   `<store>/<release_id>/bin/echo-agent-core` -> `<release_id>`. That mirrors
//!   Core's `ECHO_RELEASE_ID` declaration and lets the real adapter hash
//!   `/proc/<pid>/exe` against the release manifest.
//! * Compile with `--cfg echo_fixture_refuse_ready` to get a *different* binary
//!   whose `ready` operation is rejected. Tests publish it as an upgrade that
//!   starts but is never ready, so a real rollback can be exercised.
//! * `Type=notify` readiness is implemented by writing `READY=1` to
//!   `$NOTIFY_SOCKET` once the socket is listening, so `systemctl start` really
//!   blocks until the fixture can answer queries.
//!
//! Deliberately dependency-free (`std` only): the integration test compiles it
//! with plain `rustc` at run time and publishes the resulting bytes, together
//! with their real SHA-256, as the release's Core artifact.

use std::{
    env,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixDatagram, UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process,
    time::Duration,
};

const DECLARATION_ONLY: &str = "env_echo_release_id_declaration_only";

// `prctl(2)` constants (linux/prctl.h). Declared here to keep the fixture
// dependency-free.
const PR_SET_PTRACER: i32 = 0x5961_6d61;
const PR_SET_PTRACER_ANY: std::os::raw::c_ulong = std::os::raw::c_ulong::MAX;

extern "C" {
    fn prctl(option: i32, ...) -> i32;
    fn getsockopt(
        socket: i32,
        level: i32,
        option: i32,
        value: *mut std::os::raw::c_void,
        length: *mut u32,
    ) -> i32;
}

const SOL_SOCKET: i32 = 1;
const SO_PEERCRED: i32 = 17;

#[repr(C)]
struct PeerCredentials {
    pid: i32,
    uid: u32,
    gid: u32,
}

/// Allow a *non-ancestor* deployment process to read `/proc/<pid>/exe`.
///
/// This host runs `kernel.yama.ptrace_scope=1`, where reading the executable
/// link of a systemd-managed process is denied to everything that is not its
/// ancestor. A real Core would have to do the same (`PR_SET_PTRACER`) or the
/// adapter's image verification cannot work from a separate update unit; the
/// shipping Core does **not** do this, so live verification against a real Core
/// remains unverified. The fixture opts in so the rest of the transaction can
/// be exercised honestly.
fn authorize_peer(stream: &UnixStream) {
    use std::os::fd::AsRawFd;
    let mut credentials = PeerCredentials {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<PeerCredentials>() as u32;
    // SAFETY: the fd is borrowed from `stream` and `credentials`/`length` are
    // valid for the duration of the call.
    let result = unsafe {
        getsockopt(
            stream.as_raw_fd(),
            SOL_SOCKET,
            SO_PEERCRED,
            (&mut credentials as *mut PeerCredentials).cast(),
            &mut length,
        )
    };
    if result != 0 || credentials.pid <= 0 {
        return;
    }
    // Declare this exact peer as an allowed tracer, so it may read
    // `/proc/<our pid>/exe` even with `kernel.yama.ptrace_scope=1`.
    // SAFETY: prctl(PR_SET_PTRACER, pid) only widens this process's own scope.
    let _ = unsafe {
        prctl(
            PR_SET_PTRACER,
            credentials.pid as std::os::raw::c_ulong,
        )
    };
}

fn allow_peer_exe_verification() {
    // SAFETY: prctl(PR_SET_PTRACER, ...) only widens this throw-away process's
    // own ptrace scope; it has no memory-safety preconditions.
    let result = unsafe { prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY) };
    if result != 0 {
        eprintln!("core-fixture: warning: PR_SET_PTRACER failed (errno unavailable)");
    }
}

struct Core {
    declared: String,
    prepared: bool,
    maintenance: bool,
    generation: u64,
}

fn main() {
    allow_peer_exe_verification();
    let socket = socket_argument().unwrap_or_else(|error| die(&error));
    let declared = release_from_exe().unwrap_or_else(|error| die(&error));
    // A previous run may have been killed before ExecStopPost cleaned up.
    let _ = fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).unwrap_or_else(|error| die(&format!("bind {socket:?}: {error}")));
    let _ = fs::set_permissions(&socket, fs::Permissions::from_mode(0o600));
    // Only now is the fixture able to answer; tell systemd we are ready.
    notify_ready();
    println!(
        "core-fixture: release={declared} socket={} pid={}",
        socket.display(),
        process::id()
    );
    let mut core = Core {
        declared,
        prepared: false,
        maintenance: false,
        generation: 0,
    };
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let _ = serve(stream, &mut core);
            }
            Err(error) => die(&format!("accept: {error}")),
        }
    }
}

fn serve(stream: UnixStream, core: &mut Core) -> std::io::Result<()> {
    authorize_peer(&stream);
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        // Peer only opened a connection to read SO_PEERCRED (adapter does this).
        return Ok(());
    }
    let reply = respond(core, line.trim());
    let mut stream = stream;
    stream.write_all(reply.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn respond(core: &mut Core, request: &str) -> String {
    let operation = string_field(request, "operation").unwrap_or_default();
    let transaction = string_field(request, "transaction_id").unwrap_or_default();
    let expected_release = string_field(request, "expected_release");
    let expected_generation = number_field(request, "expected_generation");
    match operation.as_str() {
        "status" => status_reply(core, &transaction),
        "prepare" => {
            if let Some(expected) = expected_release.as_deref() {
                if expected != core.declared {
                    return error_reply(
                        "prepare",
                        &transaction,
                        "release_mismatch",
                        "fixture: expected_release does not match the running release",
                    );
                }
            }
            if !core.prepared {
                core.prepared = true;
                core.maintenance = true;
                core.generation += 1;
            }
            prepare_reply(core, &transaction)
        }
        "cancel" => {
            if !core.prepared || expected_generation != Some(core.generation) {
                return error_reply(
                    "cancel",
                    &transaction,
                    "generation_mismatch",
                    "fixture: cancel requires the prepared generation",
                );
            }
            core.prepared = false;
            core.maintenance = false;
            format!(
                "{{\"ok\":true,\"operation\":\"cancel\",\"transaction_id\":{},\"phase\":\"idle\",\
                 \"prepared\":false,\"maintenance_held\":false,\"generation\":null,\
                 \"release\":{}}}",
                quote(&transaction),
                release_object(&core.declared)
            )
        }
        "ready" => {
            if cfg!(echo_fixture_refuse_ready) {
                return error_reply(
                    "ready",
                    &transaction,
                    "fixture_release_not_ready",
                    "fixture: this build refuses readiness on purpose (failed-upgrade scenario)",
                );
            }
            if let Some(expected) = expected_release.as_deref() {
                if expected != core.declared {
                    return error_reply(
                        "ready",
                        &transaction,
                        "release_mismatch",
                        "fixture: ready expected_release does not match the running release",
                    );
                }
            }
            format!(
                "{{\"ok\":true,\"operation\":\"ready\",\"transaction_id\":{},\"ready\":true,\
                 \"prepared\":{},\"maintenance_held\":{},\"release\":{},\
                 \"checks\":{{\"configured_enabled_personas_running\":true,\"trunks_loaded\":true,\
                 \"no_retiring_personas\":true,\"release_matches_expected\":true}},\
                 \"off_register_retirements\":{{\"checked\":true,\"failed\":[],\"retiring_ids\":[]}}}}",
                quote(&transaction),
                core.prepared,
                core.maintenance,
                release_object(&core.declared)
            )
        }
        other => error_reply(other, &transaction, "unknown_operation", "unsupported operation"),
    }
}

fn status_reply(core: &Core, transaction: &str) -> String {
    let generation = if core.prepared {
        core.generation.to_string()
    } else {
        "null".to_string()
    };
    let phase = if core.prepared { "prepared" } else { "idle" };
    format!(
        "{{\"ok\":true,\"operation\":\"status\",\"transaction_id\":{},\"phase\":{},\
         \"generation\":{generation},\"prepared\":{},\"maintenance_held\":{},\"release\":{}}}",
        quote(transaction),
        quote(phase),
        core.prepared,
        core.maintenance,
        release_object(&core.declared)
    )
}

fn prepare_reply(core: &Core, transaction: &str) -> String {
    format!(
        "{{\"ok\":true,\"operation\":\"prepare\",\"transaction_id\":{},\"phase\":\"prepared\",\
         \"generation\":{},\"prepared\":true,\"maintenance_held\":true,\"can_cancel\":true,\
         \"release\":{},\
         \"writer_scope\":{{\"all_controlled_writers_paused\":true,\
         \"management_commands_quiesced\":true,\
         \"covered\":[\"management_command_writes\",\"in_flight_turns\",\"periodic_session_save\"],\
         \"not_covered\":[\"user_arbitrary_subprocesses\"]}},\
         \"agents\":[{{\"id\":\"__core\",\"generation\":{},\"prepared\":true,\
         \"writers_paused\":true,\"flush\":\"checked_ok\"}}],\
         \"off_register_retirements\":{{\"checked\":true,\"failed\":[],\"retiring_ids\":[]}},\
         \"requires\":[\"external_stop_user_arbitrary_subprocesses\"]}}",
        quote(transaction),
        core.generation,
        release_object(&core.declared),
        core.generation
    )
}

fn release_object(declared: &str) -> String {
    format!(
        "{{\"declared\":{},\"identity_verified\":false,\"verification\":{}}}",
        quote(declared),
        quote(DECLARATION_ONLY)
    )
}

fn error_reply(operation: &str, transaction: &str, code: &str, message: &str) -> String {
    format!(
        "{{\"ok\":false,\"operation\":{},\"transaction_id\":{},\
         \"error\":{{\"code\":{},\"message\":{}}}}}",
        quote(operation),
        quote(transaction),
        quote(code),
        quote(message)
    )
}

/// Minimal JSON string escaping for the ASCII identifiers we produce.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Read `"key" : "value"` tolerating any amount of whitespace around the
/// colon (different JSON serializers disagree; Core would parse properly).
fn string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after_key = &json[json.find(&needle)? + needle.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?.trim_start();
    let after_quote = after_colon.strip_prefix('"')?;
    let end = after_quote.find('"')?;
    Some(after_quote[..end].to_string())
}

fn number_field(json: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\"");
    let after_key = &json[json.find(&needle)? + needle.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?.trim_start();
    let end = after_colon
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(after_colon.len());
    after_colon[..end].parse().ok()
}

fn socket_argument() -> Result<PathBuf, String> {
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--socket" {
            let value = arguments.next().ok_or("--socket requires a value")?;
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err("--socket must be an absolute path".into());
            }
            return Ok(path);
        }
    }
    Err("usage: core_fixture --socket /absolute/path/core.sock".into())
}

/// `<store>/<release>/bin/echo-agent-core` -> `<release>`.
fn release_from_exe() -> Result<String, String> {
    let exe = fs::read_link("/proc/self/exe").map_err(|error| format!("/proc/self/exe: {error}"))?;
    let release = exe
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("cannot derive release id from {}", exe.display()))?;
    Ok(release.to_string())
}

fn notify_ready() {
    let Some(socket) = env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let socket = PathBuf::from(socket);
    if let Ok(datagram) = UnixDatagram::unbound() {
        let _ = datagram.send_to(b"READY=1", &socket);
    }
}

fn die(message: &str) -> ! {
    eprintln!("core-fixture: fatal: {message}");
    process::exit(70);
}
