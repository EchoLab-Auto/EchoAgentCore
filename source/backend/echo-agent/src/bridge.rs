//! Communication bridge between agent backend and frontends (TUI / API).
//!
//! The definitions live in the standalone [`echo_protocol`] crate (the
//! frontend–Core wire contract); this module re-exports them so existing
//! `echo_agent::bridge::…` paths keep working.

pub use echo_protocol::{
    create_bridge, deserialize_message, serialize_command, serialize_event, BackendBridge,
    BackendHandle, FanoutHandle, WsMessage,
};
