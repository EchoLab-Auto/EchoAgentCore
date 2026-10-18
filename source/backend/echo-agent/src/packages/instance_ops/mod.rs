//! 插件实例（平台插件插件）框架侧接缝：命令域 + 入站桥（泛化 S5，2026-10）。
//!
//! - [`bridge`]：入站 hook 桥（`AgentMessageHook`——把平台消息包成结构化
//!   输入并投递给 agent）；
//! - [`commands`]：插件实例命令域（通用 `InvokeInstanceOp` 分发、管理通道
//!   基础设施、缺省实例回退与写方法广播泛化），由主 `apply_command` 一行
//!   委托进来（见 `agent/commands.rs`）；
//! - [`test_support`]：命令域测试的 mock 与装配助手（仅测试）。

pub mod bridge;
pub(crate) mod commands;

#[cfg(test)]
pub(crate) mod test_support;
