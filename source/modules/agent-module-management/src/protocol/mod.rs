//! 管理面 WS 模块的协议入口（**兼容再导出**）。
//!
//! 线契约（命令 / 事件 / 信封 / TS 绑定）的真值已在 `echo-protocol`（2026-11
//! B2 反转）：那边零运行时、零 UI 文案，谁要一个 serde 形状就不必把 WS 与联邦
//! 运行时一起拖进来。本模块保留原有路径（`agent_module_management::protocol::
//! BackendEvent` 等一律是再导出），并把仍属于本 crate 的部分留在原地：
//!
//! - [`bridge`]：进程内通道与扇出（运行时）；
//! - [`command`]：命令准入策略（只读 / 广播白名单）；
//! - [`delta_batch`]：增量合批窗口与保序（运行时策略）；
//! - [`entry_identity`]：条目身份追踪；
//! - [`timeline_text`]：显示文案（UI 词汇，不进线协议）。

pub mod bridge;
pub mod command;
pub mod delta_batch;
pub mod entry_identity;
pub mod timeline_text;

pub use bridge::{
    create_bridge, deserialize_message, serialize_command, serialize_event,
    serialize_event_identified, serialize_event_sequenced, BackendBridge, BackendHandle,
    FanoutHandle, SequencedEvent, WsMessage,
};
pub use command::{
    command_clearance, is_panel_readonly_allowed, BackendCommand, CommandClearance, PendingFile,
    PANEL_READONLY_COMMANDS,
};
pub use delta_batch::{DeltaBatcher, Emit, DEFAULT_BATCH_WINDOW};
pub use echo_defs::{ReasoningEffort, ThinkingMode};
pub use echo_protocol::event;
pub use echo_protocol::event::*;
pub use echo_protocol::federation;
pub use entry_identity::EntryTracker;

/// TypeScript 绑定生成：真值在 `echo-protocol::ts_bindings`，这里只转发。
#[cfg(feature = "ts-bindings")]
pub use echo_protocol::ts_bindings;
