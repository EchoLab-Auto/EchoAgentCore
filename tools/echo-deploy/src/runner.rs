//! Run an explicitly trusted deployment hook as an isolated child process.
//!
//! The caller must name a program the operator explicitly trusts, by absolute
//! path. This is **not** a sandbox: the hook runs with the agent's privileges.
//! The module only guarantees a bounded, non-interactive JSON protocol:
//!
//! * stdin receives one JSON document (at most [`MAX_REQUEST_BYTES`]), then EOF;
//! * stdout must contain one JSON document (at most [`MAX_RESPONSE_BYTES`]);
//! * stderr is discarded.
//!
//! The hook inherits the caller's environment and working directory; only the
//! three standard streams are arranged. Requests and hook output may contain
//! credentials, so this module never logs them and error messages never include
//! their bytes.

use anyhow::{anyhow, ensure, Context, Result};
use serde_json::Value;
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::MetadataExt,
            process::{CommandExt, ExitStatusExt},
        },
    },
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

/// Maximum serialized request accepted for the hook's stdin.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Maximum stdout accepted from the hook.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// How often the parent checks whether the hook has exited.
const WAIT_POLL: Duration = Duration::from_millis(2);

/// Run a trusted hook with a single JSON request and return its JSON response.
///
/// The executable must be an absolute path whose every component is a real
/// directory entry (no symlinks), and whose leaf is an ordinary file that is not
/// group/other writable and is owned by the current user or root. The child gets
/// its own process group, so a timeout terminates the hook and everything it
/// spawned. There is no shell and no PATH lookup involved.
///
/// `request` must serialize to at most 64 KiB. The hook must exit successfully
/// and write exactly one JSON document of at most 1 MiB to stdout before the
/// deadline; anything else is an error. Neither the request nor the output is
/// ever included in an error message.
pub fn run_json(
    executable: &Path,
    args: &[String],
    request: &Value,
    timeout: Duration,
) -> Result<Value> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("deploy hook timeout is too large")?;

    let request = serde_json::to_vec(request).context("serialize deploy hook request")?;
    ensure!(
        request.len() <= MAX_REQUEST_BYTES,
        "deploy hook request exceeds 64 KiB"
    );

    // Validate and pin the operator-provided executable before starting anything.
    let _executable = open_trusted_executable(executable)?;

    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Independent process group: a timeout can stop the hook and every process
        // it spawned without signalling the agent's own group.
        .process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn deploy hook {}", executable.display()))?;
    let pid = child.id();

    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        terminate(&mut child, pid);
        return Err(anyhow!("deploy hook pipes unavailable"));
    };

    // Write stdin on a thread: a hook that never reads must not block the parent
    // before it can enforce the deadline. Dropping stdin closes the pipe (EOF).
    let _stdin_writer = match thread::Builder::new()
        .name("deploy-hook-stdin".into())
        .spawn(move || {
            let _ = stdin.write_all(&request);
            let _ = stdin.flush();
        }) {
        Ok(handle) => handle,
        Err(error) => {
            terminate(&mut child, pid);
            return Err(error).context("start deploy hook stdin writer");
        }
    };

    // Drain stdout concurrently with waiting. Never wait for exit before reading:
    // a hook that fills the pipe would deadlock against the parent. The reader is
    // bounded; on overflow it stops reading and terminates the producer so the
    // hook cannot stall forever on a full pipe.
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let _stdout_reader = match thread::Builder::new()
        .name("deploy-hook-stdout".into())
        .spawn(move || {
            let mut reader = stdout.take((MAX_RESPONSE_BYTES + 1) as u64);
            let mut bytes = Vec::new();
            let result = reader.read_to_end(&mut bytes);
            if result.is_ok() && bytes.len() > MAX_RESPONSE_BYTES {
                kill_group(pid);
            }
            let _ = stdout_tx.send(result.map(|_| bytes));
        }) {
        Ok(handle) => handle,
        Err(error) => {
            terminate(&mut child, pid);
            return Err(error).context("start deploy hook stdout reader");
        }
    };

    let status = match wait_with_deadline(&mut child, deadline) {
        Ok(Some(status)) => status,
        Ok(None) => {
            terminate(&mut child, pid);
            // Deliberately do not join the reader/writer afterwards: a descendant
            // that escaped the group could still hold the pipes, and blocking here
            // would defeat the deadline. The killed leader has been reaped above.
            return Err(anyhow!("deploy hook timed out after {timeout:?}"));
        }
        Err(error) => {
            terminate(&mut child, pid);
            return Err(error);
        }
    };

    let remaining = deadline.saturating_duration_since(Instant::now());
    let output = match stdout_rx.recv_timeout(remaining) {
        Ok(result) => result.context("read deploy hook stdout")?,
        Err(RecvTimeoutError::Timeout) => {
            // The leader exited but something it spawned still holds stdout open.
            // Stop the group so the reader can finish; never block waiting for it.
            kill_group(pid);
            return Err(anyhow!(
                "deploy hook stdout did not close before the deadline"
            ));
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Err(anyhow!("deploy hook stdout reader stopped unexpectedly"))
        }
    };

    // Check the size before the exit status: the reader kills an oversized stream,
    // so that status may be SIGKILL rather than a clean exit. Neither value is
    // echoed in the error.
    ensure!(
        output.len() <= MAX_RESPONSE_BYTES,
        "deploy hook stdout exceeds 1 MiB"
    );
    ensure!(
        status.success(),
        "deploy hook exited with {}",
        describe_exit(status)
    );
    serde_json::from_slice(&output).context("deploy hook stdout is not valid JSON")
}

/// Open and validate an operator-supplied executable, returning a pinned handle.
///
/// The returned handle is only proof of validation; execution uses the path.
/// Ancestors are walked descriptor-relative by [`crate::safe_fs::Dir::absolute`],
/// which rejects symlinked components. The leaf is opened with `O_PATH |
/// O_NOFOLLOW`, so a raced-in symlink or FIFO cannot be followed or block us.
fn open_trusted_executable(executable: &Path) -> Result<File> {
    crate::model::absolute_path(executable).context("deploy hook path")?;
    let parent = executable
        .parent()
        .context("deploy hook path has no parent")?;
    let leaf = executable
        .file_name()
        .and_then(|name| name.to_str())
        .context("deploy hook path has no file name")?;

    let directory = open_parent(parent)?;
    let leaf = CString::new(leaf).context("deploy hook path contains NUL")?;
    let file = owned_fd(
        // SAFETY: the directory fd is a pinned real directory, `leaf` is one C
        // string, and O_PATH|O_NOFOLLOW opens the entry itself without following.
        unsafe {
            libc::openat(
                directory.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        },
        "deploy hook",
    )?;

    let meta = file.metadata().context("stat deploy hook")?;
    ensure!(
        !meta.file_type().is_symlink(),
        "deploy hook must not be a symlink"
    );
    ensure!(meta.is_file(), "deploy hook must be a regular file");
    ensure!(
        meta.mode() & 0o022 == 0,
        "deploy hook must not be group/other writable"
    );
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    ensure!(
        meta.uid() == euid || meta.uid() == 0,
        "deploy hook must be owned by the current user or root"
    );
    ensure!(meta.mode() & 0o111 != 0, "deploy hook must be executable");
    Ok(file)
}

/// Open the executable's parent, rejecting symlinked ancestors.
fn open_parent(path: &Path) -> Result<File> {
    if path == Path::new("/") {
        // `model::absolute_path` intentionally rejects the root itself, but a hook
        // directly under `/` still has a legitimate, pinned parent.
        return owned_fd(
            // SAFETY: a constant C string and valid open flags; returns an owned fd.
            unsafe {
                libc::open(
                    c"/".as_ptr(),
                    libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            },
            "root directory",
        );
    }
    Ok(crate::safe_fs::Dir::absolute(path)
        .context("open deploy hook directory")?
        .0)
}

fn owned_fd(fd: libc::c_int, what: &str) -> Result<File> {
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return Err(error).with_context(|| format!("open {what}"));
    }
    // SAFETY: a successful open/openat returns a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Wait for the child to exit, polling until the deadline.
fn wait_with_deadline(child: &mut Child, deadline: Instant) -> Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait().context("wait for deploy hook")? {
            return Ok(Some(status));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        thread::sleep((deadline - now).min(WAIT_POLL));
    }
}

/// Terminate the child's whole process group and reap the leader.
fn terminate(child: &mut Child, pid: u32) {
    kill_group(pid);
    let _ = child.wait();
}

/// SIGKILL the process group whose id equals the child's pid.
fn kill_group(pid: u32) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: a negative pid addresses the process group with that id. The child was
    // spawned with process_group(0), so its group id equals its pid. An already
    // empty group yields ESRCH, which is harmless.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

fn describe_exit(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => format!("signal {}", status.signal().unwrap_or(0)),
    }
}
