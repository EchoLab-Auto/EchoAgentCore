//! Events emitted by the agent backend to subscribers (TUI, API, ...).
//!
//! The definitions live in the standalone [`echo_protocol`] crate (the
//! frontend–Core wire contract); this module re-exports them so existing
//! `echo_agent::event::…` paths keep working.

pub use echo_protocol::{
    AdapterStatus, ApiProfileInfo, BackendEvent, BackendState, ContextMessageInfo, FriendInfo,
    GroupInfo, SessionInfo, TimelineMessage, TimelineSource, TimelineTool,
};
