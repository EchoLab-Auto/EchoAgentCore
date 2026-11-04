//! Read-only session query seam.
//!
//! The session subsystem owns the data; the host resolves a persona to its
//! store; agent-facing tools depend only on this small, storage-neutral port.

use serde_json::Value;

/// Which projection of a session should be returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionQueryView {
    /// The model-visible, compacted and token-budgeted context.
    Model,
    /// Durable events, including tool calls and model-step observations.
    Audit,
}

/// Semantic, synchronous snapshots of session data.
///
/// Implementations must not read persistence files directly: a live store is
/// newer than its periodically persisted JSON snapshot.
pub trait SessionQuery: Send + Sync {
    fn list_sessions(&self, team_id: Option<&str>, limit: usize) -> Result<Value, String>;

    fn read_session(
        &self,
        team_id: Option<&str>,
        session_id: &str,
        view: SessionQueryView,
        limit: usize,
        include_tool_results: bool,
    ) -> Result<Value, String>;
}
