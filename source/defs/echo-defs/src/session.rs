//! The event-sourced session seam — traits (definitions only).
//!
//! The session log is the single source of truth for the model-facing
//! context: every durable fact is a [`SessionEvent`] appended to the log, and
//! the model context is projected from it (the `derive_messages` contract on
//! [`SessionStore`]). "Model-visible means logged": anything that reaches a
//! model request must be reconstructable from the log.
//!
//! The concrete event set and the store implementation arrive with the
//! event-sourced session (Phase 3); this crate owns only the seams.

/// One durable fact in a session log.
///
/// Implementors are concrete event structs/enums (user message entered,
/// assistant message appended, tool result recorded, compaction summary, ...).
/// The trait is `Any`-downcastable so consumers can handle known events and
/// ignore unknown ones — the Rust analogue of dsh's merge-extensible event
/// maps.
pub trait SessionEvent: Send + Sync + std::fmt::Debug + 'static {
    /// Stable event kind, e.g. `"user/message"` — the wire/persistence tag.
    fn kind(&self) -> &'static str;
}

/// The session store seam: an append-only event log per session.
///
/// Implementations own persistence (durable file format), replay, and the
/// projection from events to model messages. Consumers (the agent loop, the
/// prompt assembler) depend only on this trait.
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    /// The event type this store logs.
    type Event: SessionEvent;

    /// Append one event to the session's log (durable when persisted).
    async fn append(&self, session_id: &str, event: Self::Event) -> Result<(), String>;

    /// Read the session's full event log, oldest first.
    async fn log(&self, session_id: &str) -> Result<Vec<Self::Event>, String>;

    /// Project the model-facing message history from the log under a token
    /// budget. Trimming happens here (projection time), never destructively
    /// on the log itself.
    async fn derive_messages(
        &self,
        session_id: &str,
        token_budget: usize,
    ) -> Result<Vec<crate::message::ChatMessage>, String>;
}
