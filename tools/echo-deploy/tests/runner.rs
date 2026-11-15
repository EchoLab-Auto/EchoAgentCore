//! Behavioural tests for the trusted deployment-hook runner.
//!
//! Every fixture is a temporary `/bin/sh` script under a `tempdir`; nothing here
//! touches services, the candidate store, or any real deployment.

use echo_deploy::runner::{run_json, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};
use tempfile::{tempdir, TempDir};

struct Fixture {
    _temp: TempDir,
    dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        Self { _temp: temp, dir }
    }

    /// Write an executable `/bin/sh` fixture (mode 0700, owned by the test user).
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, format!("#!/bin/sh\nset -u\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

/// Fixtures are written and executed from one multithreaded test process. A fork
/// in thread A can briefly inherit a write descriptor that thread B holds on a
/// script, making B's exec fail with ETXTBSY. Serialise write+spawn sequences so
/// the suite tests the runner rather than that process-wide fork artefact.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Report how many bytes arrived on stdin.
const COUNT_STDIN: &str = r#"bytes=$(wc -c)
printf '{"bytes":%s}' "$bytes""#;

/// Emit 2 MiB of ASCII zeroes on stdout, slowly enough to fill any pipe.
const FLOOD_STDOUT: &str = r#"i=0
while [ "$i" -lt 2048 ]; do
  printf '%01024d' 0
  i=$((i + 1))
done"#;

#[test]
fn round_trips_json_over_stdin_and_args() {
    let _serial = serial();
    let fixture = Fixture::new();
    let hook = fixture.script(
        "echo.sh",
        r#"payload=$(cat)
printf '{"ok":true,"length":%d,"argc":%d,"first":"%s"}' "${#payload}" "$#" "$1""#,
    );
    let request = json!({"token": "s3cret-request-value"});
    let expected = serde_json::to_vec(&request).unwrap().len();

    let response = run_json(
        &hook,
        &["alpha".to_string()],
        &request,
        Duration::from_secs(5),
    )
    .unwrap();

    assert_eq!(response["ok"], json!(true));
    assert_eq!(response["length"], json!(expected));
    assert_eq!(response["argc"], json!(1));
    assert_eq!(response["first"], json!("alpha"));
}

#[test]
fn delivers_a_request_close_to_the_size_limit() {
    let _serial = serial();
    let fixture = Fixture::new();
    let hook = fixture.script("count.sh", COUNT_STDIN);
    let request = json!({"blob": "x".repeat(60_000)});
    let expected = serde_json::to_vec(&request).unwrap().len();
    assert!(expected < MAX_REQUEST_BYTES);

    let response = run_json(&hook, &[], &request, Duration::from_secs(5)).unwrap();

    assert_eq!(response["bytes"], json!(expected));
}

#[test]
fn rejects_oversized_request_before_spawning() {
    let fixture = Fixture::new();
    let marker = fixture.path("spawned");
    let hook = fixture.script("marker.sh", "printf x > \"$1\"\nprintf '{}'");
    let request = json!({"blob": "x".repeat(MAX_REQUEST_BYTES)});
    assert!(serde_json::to_vec(&request).unwrap().len() > MAX_REQUEST_BYTES);

    let error = run_json(
        &hook,
        &[marker.display().to_string()],
        &request,
        Duration::from_secs(5),
    )
    .unwrap_err();

    assert!(error.to_string().contains("64 KiB"), "{error}");
    assert!(
        !marker.exists(),
        "hook must not run for an oversized request"
    );
}

#[test]
fn times_out_and_kills_the_process_group() {
    let _serial = serial();
    let fixture = Fixture::new();
    let pidfile = fixture.path("pid");
    let hook = fixture.script("slow.sh", "echo $$ > \"$1\"\nsleep 30\nprintf '{}'");

    let started = Instant::now();
    let error = run_json(
        &hook,
        &[pidfile.display().to_string()],
        &json!({}),
        Duration::from_millis(500),
    )
    .unwrap_err();
    let elapsed = started.elapsed();

    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");

    let pid = wait_for_pid(&pidfile).expect("hook never recorded its pid");
    assert!(
        wait_until_gone(pid),
        "hook process {pid} survived the timeout"
    );
}

#[test]
fn rejects_oversized_stdout_without_waiting_for_the_timeout() {
    let _serial = serial();
    let fixture = Fixture::new();
    let hook = fixture.script("flood.sh", FLOOD_STDOUT);

    let started = Instant::now();
    let error = run_json(&hook, &[], &json!({}), Duration::from_secs(10)).unwrap_err();
    let elapsed = started.elapsed();

    assert!(error.to_string().contains("1 MiB"), "{error}");
    assert!(
        elapsed < Duration::from_secs(3),
        "the size cap must stop the producer, took {elapsed:?}"
    );
}

#[test]
fn accepts_stdout_exactly_at_the_size_limit() {
    let _serial = serial();
    let fixture = Fixture::new();
    let hook = fixture.script("cat.sh", r#"cat "$1""#);
    let overhead = r#"{"data":""}"#.len();
    let document = format!(
        "{{\"data\":\"{}\"}}",
        "x".repeat(MAX_RESPONSE_BYTES - overhead)
    );
    assert_eq!(document.len(), MAX_RESPONSE_BYTES);
    let document_path = fixture.path("document.json");
    fs::write(&document_path, &document).unwrap();

    let response = run_json(
        &hook,
        &[document_path.display().to_string()],
        &json!({}),
        Duration::from_secs(5),
    )
    .unwrap();

    assert_eq!(
        response["data"].as_str().unwrap().len(),
        MAX_RESPONSE_BYTES - overhead
    );
}

#[test]
fn rejects_invalid_json_without_echoing_output() {
    let _serial = serial();
    let fixture = Fixture::new();
    let hook = fixture.script("bad.sh", "printf 'not-json-output-6f2c'");
    let request = json!({"token": "request-secret-4d1e"});

    let error = run_json(&hook, &[], &request, Duration::from_secs(5)).unwrap_err();
    let text = error.to_string();

    assert!(text.contains("JSON"), "{text}");
    assert!(!text.contains("not-json-output-6f2c"), "{text}");
    assert!(!text.contains("request-secret-4d1e"), "{text}");
}

#[test]
fn reports_nonzero_exit_without_echoing_output() {
    let _serial = serial();
    let fixture = Fixture::new();
    let hook = fixture.script(
        "fail.sh",
        "printf '{\"secret\":\"output-secret-91a\"}'\nexit 7",
    );

    let error = run_json(&hook, &[], &json!({}), Duration::from_secs(5)).unwrap_err();
    let text = error.to_string();

    assert!(text.contains("exit code 7"), "{text}");
    assert!(!text.contains("output-secret-91a"), "{text}");
}

#[test]
fn descendant_holding_stdout_does_not_hang_the_caller() {
    let _serial = serial();
    let fixture = Fixture::new();
    // The shell exits immediately but its background child inherits stdout, so the
    // pipe never closes. The runner must hit its deadline instead of blocking.
    let hook = fixture.script("leak.sh", "( sleep 20 ) &\nprintf '{\"ok\":true}'");

    let started = Instant::now();
    let error = run_json(&hook, &[], &json!({}), Duration::from_millis(1000)).unwrap_err();
    let elapsed = started.elapsed();

    assert!(error.to_string().contains("stdout"), "{error}");
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
}

#[test]
fn discards_stderr_instead_of_buffering_it() {
    let _serial = serial();
    let fixture = Fixture::new();
    let hook = fixture.script(
        "noisy.sh",
        &format!("{FLOOD_STDOUT} 1>&2\nprintf '{{\"ok\":true}}'"),
    );

    let response = run_json(&hook, &[], &json!({}), Duration::from_secs(5)).unwrap();

    assert_eq!(response["ok"], json!(true));
}

#[test]
fn rejects_untrusted_paths() {
    let fixture = Fixture::new();
    let hook = fixture.script("hook.sh", "printf '{}'");
    let error = run_json(
        Path::new("hook.sh"),
        &[],
        &json!({}),
        Duration::from_secs(5),
    )
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("absolute"), "{text}");
    let leaf_link = fixture.path("link.sh");
    symlink(&hook, &leaf_link).unwrap();
    let error = run_json(&leaf_link, &[], &json!({}), Duration::from_secs(5)).unwrap_err();
    assert!(error.to_string().contains("symlink"), "{error}");

    // A symlinked ancestor must be rejected by Dir::absolute, not followed.
    let real = fixture.path("real");
    fs::create_dir(&real).unwrap();
    let buried = real.join("hook.sh");
    fs::write(&buried, "#!/bin/sh\nprintf '{}'\n").unwrap();
    fs::set_permissions(&buried, fs::Permissions::from_mode(0o700)).unwrap();
    let linked_dir = fixture.path("linked-dir");
    symlink(&real, &linked_dir).unwrap();
    let error = run_json(
        &linked_dir.join("hook.sh"),
        &[],
        &json!({}),
        Duration::from_secs(5),
    )
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("symbolic link") || text.contains("Not a directory"),
        "{text}"
    );

    let writable = fixture.script("writable.sh", "printf '{}'");
    fs::set_permissions(&writable, fs::Permissions::from_mode(0o722)).unwrap();
    let error = run_json(&writable, &[], &json!({}), Duration::from_secs(5)).unwrap_err();
    assert!(error.to_string().contains("writable"), "{error}");

    let unexecutable = fixture.script("noexec.sh", "printf '{}'");
    fs::set_permissions(&unexecutable, fs::Permissions::from_mode(0o600)).unwrap();
    let error = run_json(&unexecutable, &[], &json!({}), Duration::from_secs(5)).unwrap_err();
    assert!(error.to_string().contains("executable"), "{error}");
}

fn wait_for_pid(path: &Path) -> Option<i32> {
    for _ in 0..100 {
        if let Ok(text) = fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return Some(pid);
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

fn wait_until_gone(pid: i32) -> bool {
    for _ in 0..100 {
        // SAFETY: signal 0 only probes whether the pid still exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
        if !alive {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}
