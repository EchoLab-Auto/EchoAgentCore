//! Test-only **slow-start** stand-in for the Core side of the
//! `echo-deploy-control` Unix-socket protocol, used by
//! `tests/systemd_crash_live.rs` to prove real interruption recovery.
//!
//! This is a fixture, **not** Core. It never reads real configuration, user
//! data, secrets or the network; it only binds one Unix socket inside the
//! throw-away temporary directory of the live test.
//!
//! Why a second Core fixture instead of `core_fixture.rs`
//! -------------------------------------------------------
//!
//! The interruption test has to kill `echo-deploy systemd-apply` at an
//! *observable* point: the durable selector already names the target release,
//! the target unit is `activating`, this ELF has really executed, but the unit
//! has not reached `Ready` and the control socket does not exist yet. A plain
//! sleep in the test would be a guess; this fixture provides a deterministic
//! brake instead:
//!
//! Usage:
//!
//! ```text
//! crash_core --socket /abs/core.sock \
//!            --started /abs/target-started \
//!            --ready-gate /abs/ready-gate
//! ```
//!
//! 1. write `--started` (pid, release and paths) - durable proof that this
//!    release's ELF executed, kept even after systemd stops the unit;
//! 2. wait until `--ready-gate` exists *before* binding the socket and before
//!    `READY=1`. `systemctl start` therefore blocks in `activating/start`, which
//!    is the condition the live test observes;
//! 3. only then bind, `READY=1` and speak the same `status` / `prepare` /
//!    `cancel` / `ready` protocol as `core_fixture.rs`.
//!
//! The live test never creates the gate before it kills `systemd-apply`, so the
//! target can never become ready by accident. Recovery must stop this unit and
//! restore the previous release on its own.
//!
//! There is deliberately **no** `prctl(PR_SET_PTRACER)` here (neither `ANY` nor
//! per-peer): this test drives the transaction through the documented trusted
//! *test* adapter and never through the shipping adapter's `/proc/<pid>/exe`
//! image check, so no ptrace permission is needed or widened.
//!
//! Deliberately dependency-free (`std` only); the integration test compiles it
//! with plain `rustc` and publishes the bytes as the release's Core artifact.

use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixDatagram, UnixListener, UnixStream},
    },
    path::PathBuf,
    process, thread,
    time::Duration,
};

const DECLARATION_ONLY: &str = "env_echo_release_id_declaration_only";

struct Arguments {
    socket: PathBuf,
    started: PathBuf,
    ready_gate: PathBuf,
}

fn main() {
    let arguments = parse_arguments().unwrap_or_else(|error| die(&error));
    let declared = release_from_exe().unwrap_or_else(|error| die(&error));

    // Durable proof that the target release really executed. The live test
    // reads this after recovery; it is not removed by the unit's ExecStopPost.
    fs::write(
        &arguments.started,
        format!(
            "pid={} declared={} socket={} gate={}\n",
            process::id(),
            declared,
            arguments.socket.display(),
            arguments.ready_gate.display()
        ),
    )
    .unwrap_or_else(|error| die(&format!("write started marker: {error}")));

    // Deterministic brake. `--ready-gate` is created by the caller only when it
    // wants readiness; the interruption test never does before SIGKILL.
    while !arguments.ready_gate.exists() {
        thread::sleep(Duration::from_millis(20));
    }

    // A previous run may have been killed before ExecStopPost cleaned up.
    let _ = fs::remove_file(&arguments.socket);
    let listener = UnixListener::bind(&arguments.socket)
        .unwrap_or_else(|error| die(&format!("bind {:?}: {error}", arguments.socket)));
    let _ = fs::set_permissions(&arguments.socket, fs::Permissions::from_mode(0o600));
    notify_ready();
    println!(
        "crash-core-fixture: release={declared} socket={} pid={}",
        arguments.socket.display(),
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

fn parse_arguments() -> Result<Arguments, String> {
    let mut socket = None;
    let mut started = None;
    let mut ready_gate = None;
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--socket" => socket = Some(PathBuf::from(next_value(&mut arguments, &argument)?)),
            "--started" => started = Some(PathBuf::from(next_value(&mut arguments, &argument)?)),
            "--ready-gate" => {
                ready_gate = Some(PathBuf::from(next_value(&mut arguments, &argument)?))
            }
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let socket =
        socket.ok_or("usage: crash_core --socket <abs> --started <abs> --ready-gate <abs>")?;
    let started =
        started.ok_or("usage: crash_core --socket <abs> --started <abs> --ready-gate <abs>")?;
    let ready_gate =
        ready_gate.ok_or("usage: crash_core --socket <abs> --started <abs> --ready-gate <abs>")?;
    for path in [&socket, &started, &ready_gate] {
        if !path.is_absolute() {
            return Err(format!("{} must be an absolute path", path.display()));
        }
    }
    Ok(Arguments {
        socket,
        started,
        ready_gate,
    })
}

fn next_value(
    arguments: &mut std::iter::Skip<std::env::Args>,
    flag: &str,
) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

/// `<store>/<release>/bin/echo-agent-core` -> `<release>`.
fn release_from_exe() -> Result<String, String> {
    let exe =
        fs::read_link("/proc/self/exe").map_err(|error| format!("/proc/self/exe: {error}"))?;
    let release = exe
        .parent()
        .and_then(|path| path.parent())
        .and_then(|path| path.file_name())
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

struct Core {
    declared: String,
    prepared: bool,
    maintenance: bool,
    generation: u64,
}
fn serve(stream: UnixStream, core: &mut Core) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
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
                        "crash fixture: expected_release does not match the running release",
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
                    "crash fixture: cancel requires the prepared generation",
                );
            }
            core.prepared = false;
            core.maintenance = false;
            format!(
                "{{\"ok\":true,\"operation\":\"cancel\",\"transaction_id\":{},\
                 \"phase\":\"idle\",\"prepared\":false,\"maintenance_held\":false,\
                 \"generation\":null,\"release\":{}}}",
                quote(&transaction),
                release_object(&core.declared)
            )
        }
        "ready" => {
            if let Some(expected) = expected_release.as_deref() {
                if expected != core.declared {
                    return error_reply(
                        "ready",
                        &transaction,
                        "release_mismatch",
                        "crash fixture: ready expected_release does not match the running release",
                    );
                }
            }
            format!(
                "{{\"ok\":true,\"operation\":\"ready\",\"transaction_id\":{},\
                 \"ready\":true,\"prepared\":{},\"maintenance_held\":{},\"release\":{}}}",
                quote(&transaction),
                core.prepared,
                core.maintenance,
                release_object(&core.declared)
            )
        }
        other => error_reply(
            other,
            &transaction,
            "unknown_operation",
            "unsupported operation",
        ),
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
        "{{\"ok\":true,\"operation\":\"prepare\",\"transaction_id\":{},\
         \"phase\":\"prepared\",\"generation\":{},\"prepared\":true,\
         \"maintenance_held\":true,\"can_cancel\":true,\"release\":{}}}",
        quote(transaction),
        core.generation,
        release_object(&core.declared)
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

/// Read `"key" : "value"` tolerating whitespace around the colon.
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

fn die(message: &str) -> ! {
    eprintln!("crash-core-fixture: fatal: {message}");
    process::exit(70);
}
