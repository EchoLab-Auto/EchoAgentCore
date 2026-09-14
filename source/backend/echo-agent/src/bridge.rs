//! Communication bridge between agent backend and frontends (TUI / API).
//!
//! The definitions live in the standalone [`echo_protocol`] crate (the
//! frontend–Core wire contract); this module re-exports them so existing
//! `echo_agent::bridge::…` paths keep working.

pub use echo_protocol::{
    create_bridge, deserialize_menu_answer, deserialize_message, deserialize_sudo_password,
    serialize_command, serialize_event, serialize_menu_answer, serialize_sudo_password,
    BackendBridge, BackendHandle, FanoutHandle, MenuAnswerSubmit, SudoPasswordSubmit, WsMessage,
};
