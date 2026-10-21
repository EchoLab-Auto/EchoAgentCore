//! EchoAgentCore frontend protocol crate.
//!
//! This crate is the **single source of truth** for the contract between the
//! agent Core and its frontends (TUI Panel, API clients):
//!
//! - [`command::BackendCommand`] — commands a frontend sends to the Core.
//! - [`event::BackendEvent`] — events the Core emits to frontends, plus the
//!   snapshot types they carry ([`SessionInfo`], [`TimelineMessage`], ...).
//! - [`mode`] — shared enums ([`ThinkingMode`],
//!   [`ReasoningEffort`], [`LoopMode`]) referenced by both sides of the protocol.
//! - [`bridge`] — the in-process mpsc bridge pair and the WebSocket wire
//!   format ([`WsMessage`], [`serialize_command`], [`serialize_event`],
//!   [`deserialize_message`]).
//!
//! It deliberately depends on nothing agent- or platform-specific so that a
//! thin frontend (e.g. the standalone EchoAgentTui) can depend on this crate
//! alone. The serde representation is the wire contract on
//! `ws://…:3132` — field/variant names must stay stable; new fields need
//! `#[serde(default)]` to keep old peers decodable.

pub mod bridge;
pub mod command;
pub mod delta_batch;
pub mod entry_identity;
pub mod event;
pub mod mode;

pub use delta_batch::{DeltaBatcher, Emit, DEFAULT_BATCH_WINDOW};
pub use entry_identity::EntryTracker;

pub use bridge::{
    create_bridge, deserialize_message, serialize_command, serialize_event,
    serialize_event_identified, serialize_event_sequenced, BackendBridge, BackendHandle,
    FanoutHandle, SequencedEvent, WsMessage,
};

pub use command::{
    command_clearance, is_panel_readonly_allowed, BackendCommand, CommandClearance,
    PANEL_READONLY_COMMANDS,
};
pub use event::{
    ApiBalancePoint, ApiMetricsEntry, ApiProfileInfo, ApiUsagePoint, BackendEvent, BackendState,
    BrowseRoot, ContextBlockInfo, ContextMessageInfo, FederatedNodeInfo, FederationLinkState,
    FederationPeerInfo,
    InstanceMetadataEntry, InstanceStatus, PluginInfo, SessionInfo, ShellSessionInfo, SkillInfo,
    SkillSourceInfo, TeamInfo, TimelineMessage, TimelineSource, TimelineTool, ToolInfo,
    ToolStateEventBuilder,
    WorkspaceConversationInfo, WorkspaceDirectory, WorkspaceFileEntry, WorkspaceGitInfo,
    WorkspaceSessionInfo,
};
pub use mode::{LoopMode, ReasoningEffort, ThinkingMode};
