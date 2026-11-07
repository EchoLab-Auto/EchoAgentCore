//! 管理面服务拥有的 Panel 通信协议。
//!
//! 命令、事件、线编码和辅助投影由服务方统一定义；消费者遵循这里的契约，
//! 不再依赖独立的 `echo-protocol` 总协议 crate。

pub mod bridge;
pub mod command;
pub mod delta_batch;
pub mod entry_identity;
pub mod event;
pub mod timeline_text;

pub use bridge::{
    create_bridge, deserialize_message, serialize_command, serialize_event,
    serialize_event_identified, serialize_event_sequenced, BackendBridge, BackendHandle,
    FanoutHandle, SequencedEvent, WsMessage,
};
pub use command::{
    command_clearance, is_panel_readonly_allowed, BackendCommand, CommandClearance,
    PANEL_READONLY_COMMANDS,
};
pub use delta_batch::{DeltaBatcher, Emit, DEFAULT_BATCH_WINDOW};
pub use echo_defs::{ReasoningEffort, ThinkingMode};
pub use entry_identity::EntryTracker;
pub use event::*;
