//! Shared mocks and test helpers for the EchoAgentCore workspace.
//!
//! Extracted from ad-hoc `#[cfg(test)]` mocks so that every crate can use
//! the same implementations as a `[dev-dependency]`:
//! - [`MockAdapter`] — a scriptable platform adapter.
//! - [`temp_config_file`] — a temp TOML file with automatic cleanup.
//!
//! Note: this crate deliberately depends only on echo-adapter, never on
//! echo-agent — agent crates' `[dev-dependencies]` on this crate would
//! otherwise compile echo-agent twice (dev-dependency cycle), breaking type
//! identity. Mock LLM providers live inside echo-agent's own test module.

pub use adapter::MockAdapter;

/// Create a temporary TOML config file in the system temp dir.
///
/// The file is removed on drop; returns the path so callers can hand it to
/// `set_config_path` / `set_persist_path` etc.
pub fn temp_config_file(name: &str, content: &str) -> TempFile {
    let path = std::env::temp_dir().join(format!("echo-test-{name}-{}.toml", uuid_lite()));
    std::fs::write(&path, content).expect("write temp config");
    TempFile { path }
}

/// RAII guard that removes a temp file when dropped.
pub struct TempFile {
    path: std::path::PathBuf,
}

impl TempFile {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A short random-ish suffix (test-only, uniqueness is all that matters).
fn uuid_lite() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}")
}

mod adapter {
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use echo_adapter::traits::{Adapter, AdapterConnectionState, AdapterError, AdapterInfo};

    /// A scriptable `Adapter` for registry and integration tests.
    pub struct MockAdapter {
        pub name: String,
        pub configured: bool,
        pub started: AtomicBool,
        /// Canned outbound send results, drained in order.
        pub send_results: Vec<echo_adapter::types::SendResult>,
        /// Events received via `subscribe`, for assertions.
        pub subscribers: std::sync::Mutex<
            Vec<tokio::sync::mpsc::UnboundedSender<echo_adapter::types::AdapterEvent>>,
        >,
        /// Filter state updated via `update_allowlist` / `update_denylist`.
        pub allowlist_users: std::sync::Mutex<Vec<String>>,
        pub allowlist_groups: std::sync::Mutex<Vec<String>>,
        pub denylist_users: std::sync::Mutex<Vec<String>>,
        pub denylist_groups: std::sync::Mutex<Vec<String>>,
    }

    impl MockAdapter {
        pub fn new(name: &str, configured: bool) -> Self {
            Self {
                name: name.into(),
                configured,
                started: AtomicBool::new(false),
                send_results: Vec::new(),
                subscribers: std::sync::Mutex::new(Vec::new()),
                allowlist_users: std::sync::Mutex::new(Vec::new()),
                allowlist_groups: std::sync::Mutex::new(Vec::new()),
                denylist_users: std::sync::Mutex::new(Vec::new()),
                denylist_groups: std::sync::Mutex::new(Vec::new()),
            }
        }

        pub fn is_started(&self) -> bool {
            self.started.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Adapter for MockAdapter {
        fn name(&self) -> &str {
            &self.name
        }
        fn display_name(&self) -> &str {
            &self.name
        }
        fn platform(&self) -> &str {
            "mock"
        }
        fn is_configured(&self) -> bool {
            self.configured
        }
        fn status_info(&self) -> AdapterInfo {
            AdapterInfo {
                name: self.name.clone(),
                display_name: self.name.clone(),
                status: if self.is_started() {
                    AdapterConnectionState::Connected
                } else {
                    AdapterConnectionState::Stopped
                },
                self_id: None,
                started_at: None,
                platform: "mock".into(),
                configured: self.configured,
            }
        }
        async fn start(&self) -> Result<(), AdapterError> {
            self.started.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn stop(&self) -> Result<(), AdapterError> {
            self.started.store(false, Ordering::SeqCst);
            Ok(())
        }
        fn subscribe(
            &self,
            tx: tokio::sync::mpsc::UnboundedSender<echo_adapter::types::AdapterEvent>,
        ) {
            self.subscribers
                .lock()
                .expect("subscribers poisoned")
                .push(tx);
        }
        async fn send_message(
            &self,
            _target: &echo_adapter::types::MessageTarget,
            _content: &str,
        ) -> Result<echo_adapter::types::SendResult, AdapterError> {
            Ok(self
                .send_results
                .first()
                .cloned()
                .unwrap_or(echo_adapter::types::SendResult {
                    message_id: Some("mock-id".into()),
                    success: true,
                    error: None,
                }))
        }
    }
}

/// Minimal standalone sanity check that the helpers compile and work.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_file_creates_and_removes() {
        let path = {
            let f = temp_config_file("probe", "[agent]\nprovider = \"mock\"\n");
            assert!(f.path().exists());
            let content = std::fs::read_to_string(f.path()).unwrap();
            assert!(content.contains("[agent]"));
            f.path().to_path_buf()
        };
        // Dropped above; the file must be gone.
        assert!(!path.exists());
    }
}
