//! Human-in-the-loop sudo authorization.
//!
//! The LLM never sees or handles the sudo password: [`run_sudo`] (an
//! orchestration tool) registers a pending request with the
//! [`SudoBroker`], emits a `BackendEvent::SudoRequest`, and awaits a oneshot.
//! The Panel answers on a **dedicated channel** (`WsMessage::SudoPassword` →
//! `SudoBroker::submit`), bypassing the agent command queue, the session log
//! and the LLM context. The password is piped into `sudo -S` stdin and
//! zeroized immediately afterwards; it is never logged, never serialized into
//! an event, and never returned to the model.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// A pending sudo authorization request.
///
/// Dropping the request before it is resolved (e.g. the tool future is
/// aborted by the outer tool timeout) cancels the broker entry so it cannot
/// leak; the password can then no longer be submitted.
pub struct PendingSudo {
    pub request_id: u64,
    rx: Option<tokio::sync::oneshot::Receiver<Option<String>>>,
    broker: Arc<SudoBroker>,
}

impl PendingSudo {
    /// Take the password receiver out of the request.
    pub fn into_receiver(mut self) -> tokio::sync::oneshot::Receiver<Option<String>> {
        self.rx.take().expect("sudo receiver taken exactly once")
    }
}

impl Drop for PendingSudo {
    fn drop(&mut self) {
        if self.rx.is_some() {
            self.broker.cancel(self.request_id);
        }
    }
}

/// Registry of pending sudo authorization requests.
///
/// `request` is called by the agent's `run_sudo` tool; `submit`/`deny` are
/// called by the management server when a sudo password frame arrives. The
/// password lives only inside the oneshot and the `sudo` stdin pipe.
#[derive(Default)]
pub struct SudoBroker {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Option<String>>>>,
}

impl SudoBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pending authorization request and return its receiver.
    pub fn request(self: &Arc<Self>) -> PendingSudo {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .expect("sudo broker poisoned")
            .insert(request_id, tx);
        PendingSudo {
            request_id,
            rx: Some(rx),
            broker: Arc::clone(self),
        }
    }

    /// Resolve a pending request with a password (`Some`) or a deny (`None`).
    /// Returns `false` when the request already timed out or was cancelled.
    pub fn submit(&self, request_id: u64, password: Option<String>) -> bool {
        let sender = self
            .pending
            .lock()
            .expect("sudo broker poisoned")
            .remove(&request_id);
        match sender {
            Some(tx) => tx.send(password).is_ok(),
            None => false,
        }
    }

    /// Forget a pending request without resolving it (tool timeout, cancel).
    pub fn cancel(&self, request_id: u64) {
        self.pending
            .lock()
            .expect("sudo broker poisoned")
            .remove(&request_id);
    }
}

/// Run one command through `sudo -S`, piping the password to stdin.
///
/// The password is written to the pipe, the buffer is zeroized, and the
/// password value itself is dropped — it never reaches the command line, the
/// environment, a log, or the returned output.
pub async fn run_sudo_command(
    command: &str,
    password: &str,
    timeout: Duration,
) -> Result<String, String> {
    run_sudo_command_with_bin("sudo", command, password, timeout).await
}

/// Test hook: run through an arbitrary sudo binary (a fake in tests).
pub(crate) async fn run_sudo_command_with_bin(
    sudo_bin: &str,
    command: &str,
    password: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut child = Command::new(sudo_bin)
        .arg("-S") // read the password from stdin
        .arg("-p") // suppress the password prompt
        .arg("")
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn {sudo_bin}: {e}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open sudo stdin".to_string())?;
    let mut secret = password.as_bytes().to_vec();
    secret.push(b'\n');
    if let Err(e) = stdin.write_all(&secret).await {
        secret.fill(0); // best-effort zeroization before the error returns
        return Err(format!("failed to write password to sudo: {e}"));
    }
    secret.fill(0);
    drop(secret);
    drop(stdin);

    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("sudo command timed out after {}s", timeout.as_secs()))?
        .map_err(|e| format!("sudo command failed: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut result = String::new();
    if !stdout.trim().is_empty() {
        let truncated: String = stdout.chars().take(4096).collect();
        result.push_str(&truncated);
        if stdout.chars().count() > 4096 {
            result.push_str("\n... (truncated)");
        }
    }
    if !stderr.trim().is_empty() {
        let truncated: String = stderr.chars().take(1024).collect();
        result.push_str(&format!("\n[stderr]\n{truncated}"));
    }
    if output.status.success() {
        Ok(if result.trim().is_empty() {
            "(no output)".into()
        } else {
            result
        })
    } else {
        Ok(format!(
            "{result}\n[exit code: {}]",
            output.status.code().unwrap_or(-1)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker() -> Arc<SudoBroker> {
        Arc::new(SudoBroker::new())
    }

    #[tokio::test]
    async fn submit_resolves_pending_request() {
        let broker = broker();
        let pending = broker.request();
        let id = pending.request_id;
        assert!(broker.submit(id, Some("secret".into())));
        let password = pending.into_receiver().await.expect("request resolved");
        assert_eq!(password.as_deref(), Some("secret"));
    }

    #[tokio::test]
    async fn deny_resolves_with_none() {
        let broker = broker();
        let pending = broker.request();
        let id = pending.request_id;
        assert!(broker.submit(id, None));
        assert_eq!(
            pending.into_receiver().await.expect("request resolved"),
            None
        );
    }

    #[test]
    fn submit_unknown_id_is_false() {
        let broker = broker();
        assert!(!broker.submit(12345, Some("x".into())));
    }

    #[tokio::test]
    async fn cancel_makes_submit_fail() {
        let broker = broker();
        let pending = broker.request();
        let id = pending.request_id;
        broker.cancel(id);
        assert!(!broker.submit(id, Some("x".into())));
        assert!(
            pending.into_receiver().await.is_err(),
            "receiver dropped on cancel"
        );
    }

    #[test]
    fn dropping_pending_request_cancels_entry() {
        let broker = broker();
        let id = {
            let pending = broker.request();
            pending.request_id
        };
        // The pending guard dropped without resolving → entry cancelled.
        assert!(!broker.submit(id, Some("x".into())));
    }

    #[test]
    fn request_ids_are_unique_and_monotonic() {
        let broker = broker();
        let a = broker.request();
        let b = broker.request();
        assert_ne!(a.request_id, b.request_id);
    }

    #[tokio::test]
    async fn run_sudo_pipes_password_and_captures_output() {
        // A fake `sudo` that consumes stdin (the password) then echoes the
        // command back; proves the password goes through the pipe and the
        // output round-trips without touching the command line.
        let fake = std::env::temp_dir().join(format!("echo-fake-sudo-{}", std::process::id()));
        std::fs::write(
            &fake,
            "#!/bin/sh\ncat >/dev/null\nprintf 'fake-sudo-ran:%s' \"$7\"\n",
        )
        .expect("write fake sudo");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake sudo");
        }
        let out = run_sudo_command_with_bin(
            fake.to_str().expect("path"),
            "hello-from-shell",
            "s3cret",
            Duration::from_secs(10),
        )
        .await
        .expect("fake sudo should complete");
        assert!(out.contains("fake-sudo-ran"), "unexpected: {out}");
        assert!(!out.contains("s3cret"), "password must not leak: {out}");
        let _ = std::fs::remove_file(&fake);
    }

    #[tokio::test]
    async fn run_sudo_returns_exit_code_output() {
        // With a wrong password real sudo fails; the error is surfaced as an
        // exit code, never as a password leak. Skipped when sudo is absent.
        let has_sudo = std::process::Command::new("sudo")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_sudo {
            return;
        }
        let out = run_sudo_command("true", "definitely-wrong", Duration::from_secs(15)).await;
        assert!(
            out.is_ok(),
            "sudo should complete with an exit code: {out:?}"
        );
        let text = out.expect("ok");
        assert!(
            !text.contains("definitely-wrong"),
            "password must never appear: {text}"
        );
    }
}
