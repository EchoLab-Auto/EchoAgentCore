//! EchoAgentCore frontend protocol crate.
//!
//! This crate is the **single source of truth** for the contract between the
//! agent Core and its frontends (TUI Panel, API clients):
//!
//! - [`command::BackendCommand`] — commands a frontend sends to the Core.
//! - [`event::BackendEvent`] — events the Core emits to frontends, plus the
//!   snapshot types they carry ([`SessionInfo`], [`TimelineMessage`], ...).
//! - [`mode`] — shared enums ([`GateMode`], [`ThinkingMode`],
//!   [`ReasoningEffort`]) referenced by both sides of the protocol.
//! - [`bridge`] — the in-process mpsc bridge pair and the WebSocket wire
//!   format ([`WsMessage`], [`serialize_command`], [`serialize_event`],
//!   [`deserialize_message`]).
//!
//! It deliberately depends on nothing agent- or platform-specific so that a
//! thin frontend (e.g. EchoAgentPanel's `echo-tui`) can depend on this crate
//! alone. The serde representation is the wire contract on
//! `ws://…:3132` — field/variant names must stay stable; new fields need
//! `#[serde(default)]` to keep old peers decodable.

pub mod bridge;
pub mod command;
pub mod event;
pub mod mode;

pub use bridge::{
    create_bridge, deserialize_message, deserialize_sudo_password, serialize_command,
    serialize_event, serialize_sudo_password, BackendBridge, BackendHandle, FanoutHandle,
    SudoPasswordSubmit, WsMessage,
};
pub use command::{command_clearance, BackendCommand, CommandClearance};
pub use event::{
    AdapterStatus, AgentInfo, ApiProfileInfo, BackendEvent, BackendState, ContextBlockInfo,
    ContextMessageInfo, FriendInfo, GroupInfo, PluginInfo, SessionInfo, SkillInfo, TimelineMessage,
    TimelineSource, TimelineTool, ToolInfo,
};
pub use mode::{GateMode, ReasoningEffort, ThinkingMode};
