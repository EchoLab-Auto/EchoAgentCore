//! Commands sent by the TUI / API layer into the agent backend.
//!
//! The definitions live in the standalone [`echo_protocol`] crate (the
//! frontend–Core wire contract); this module re-exports them so existing
//! `echo_agent::command::…` paths keep working.

pub use echo_protocol::BackendCommand;
