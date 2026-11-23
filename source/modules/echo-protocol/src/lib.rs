//! # echo-protocol —— 管理面线契约（wire）
//!
//! Core 与 Panel 之间**唯一**的线格式真值：命令、事件、信封，以及它们的
//! TypeScript 绑定生成。**零领域实现**——不监听端口、不持有通道、不投影
//! 时间线、不写任何界面文案。
//!
//! ## 边界（2026-11 B2「反转」）
//!
//! 此前 `echo-protocol` 只是 `agent-module-management` 的兼容 facade：协议真值
//! 住在管理面 WS 模块里，于是「只要一个命令的 serde 形状」也得拖进
//! tokio-tungstenite 与联邦运行时。现在反过来：
//!
//! | 内容 | owner |
//! |---|---|
//! | `BackendCommand` / `BackendEvent` / 载荷 DTO | **本 crate** |
//! | `WsMessage` / `SequencedEvent` / `serialize_*`（`envelope`） | **本 crate** |
//! | `dump-protocol-ts` 的绑定清单（`ts_bindings`） | **本 crate** |
//! | 联邦状态 DTO（`federation`） | **本 crate** |
//! | WS 桥接、扇出、序号分配 | `agent-module-management` |
//! | 命令准入策略（只读 / 广播白名单） | `agent-module-management` |
//! | 增量合批窗口、条目身份追踪 | `agent-module-management` |
//! | 时间线 / 显示文案（`timeline_text`） | `agent-module-management` |
//! | 联邦运行时（链路、路由、聚合、端口合并） | `agent-module-federation` |
//!
//! 依赖方向：只向下到定义层（`echo-defs`）与机制词汇（`echo-context` 的
//! `Event` marker trait——类型与 trait 都在本 crate 之外时无法在别处补 impl）。
//! **不依赖任何模块 crate**；`agent-module-management` /
//! `agent-module-federation` 反过来依赖本 crate 并再导出线类型。
//!
//! ## 兼容
//!
//! 旧路径一律保留（都是再导出，不是拷贝）：
//! - `agent_module_management::protocol::{command, event, bridge, ts_bindings, …}`
//! - `agent_module_federation::{FederationPeerInfo, FederatedNodeInfo, FederationLinkState}`
//! - `echo_protocol::module::MODULE_ID`（协议模块 id）

pub mod command;
pub mod emit;
pub mod envelope;
pub mod event;
pub mod federation;
#[cfg(feature = "ts-bindings")]
pub mod ts_bindings;

pub use command::{BackendCommand, PendingFile};
pub use echo_defs::{ReasoningEffort, ThinkingMode};
pub use emit::Emit;
pub use envelope::{
    deserialize_message, serialize_command, serialize_event, serialize_event_identified,
    serialize_event_sequenced, SequencedEvent, WsMessage,
};
pub use event::*;
pub use federation::{FederatedNodeInfo, FederationLinkState, FederationPeerInfo};

/// 仅用于 rustdoc：`event.rs` 里有一条 `crate::protocol::ToolInfo` 的文档链接，
/// 它的文本会**逐字**出现在 `dump-protocol-ts` 的生成物里（Panel 按 sha256 校验），
/// 所以不能为了搬家而改文案。这里给它一个解析目标，仅 `cfg(doc)` 存在。
#[cfg(doc)]
pub mod protocol {
    pub use crate::event::*;
}

/// 协议模块（`kind = "kernel"`）的 id：`modules.toml` 与 Panel 模块清单同一口径。
pub const PROTOCOL_MODULE_ID: &str = "echo-agent.kernel.protocol";

/// 协议模块的声明（**单一真值**：身份），由组合根解析成 manifest。
pub const PROTOCOL_MANIFEST_TOML: &str = include_str!("../module.protocol.toml");

/// 兼容别名：旧 facade 的 `echo_protocol::module::MODULE_ID`。
pub mod module {
    pub const MODULE_ID: &str = super::PROTOCOL_MODULE_ID;
}

#[cfg(test)]
#[path = "ratchet.rs"]
mod ratchet;
