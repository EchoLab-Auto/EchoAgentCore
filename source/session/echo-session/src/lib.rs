//! EchoAgentCore event-sourced session store.
//!
//! The session log is the single source of truth for the model-facing
//! context: every durable fact is a [`SessionEvent`] appended to the log, and
//! the model context is projected from it by [`derive_messages`].
//!
//! # Design rules (dsh "model-visible means logged")
//!
//! - Anything that reaches a model request must be reconstructable from the
//!   log; [`derive_messages`] is the only projection, and trimming happens
//!   there (projection time), never destructively on the log.
//! - Compaction is an explicit event: a summary replaces the events it
//!   covers, so the log stays append-only and replayable.
//! - [`SessionHeader`] carries fork/resume metadata (parent, seed length,
//!   origin, delegation depth) so replay can distinguish inherited history
//!   from child work.
//!
//! # Crate layout
//!
//! | module | owns |
//! |---|---|
//! | [`event`] | `SessionEvent` and per-kind payload structs |
//! | [`log`] | the append-only [`EventLog`] and its durable persistence |
//! | [`derive`] | `derive_messages` projection + compaction |
//! | [`header`] | `SessionHeader` fork/resume metadata |
//! | [`legacy`] | migration from the pre-event-sourced `echo-sessions.json` v4 format |

pub mod derive;
pub mod event;
pub mod header;
pub mod legacy;
pub mod log;

pub use derive::{derive_messages, project_messages, CompactedRange};
pub use event::{
    AssistantMessage, CompactionEvent, SessionEvent, ToolCallEvent, ToolResultEvent, UserMessage,
};
pub use header::SessionHeader;
pub use legacy::{migrate_v4_document, MigrationError};
pub use log::{EventLog, EventLogError};

/// The on-disk format version for event logs.
pub const LOG_FORMAT_VERSION: u32 = 5;
