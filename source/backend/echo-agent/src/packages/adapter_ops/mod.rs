//! 适配器（平台适配器插件）框架侧接缝：命令域 + 入站桥（泛化 S5，2026-10）。
//!
//! - [`bridge`]：入站 hook 桥（`AgentMessageHook`——把平台消息包成结构化
//!   输入并投递给 agent）；
//! - [`commands`]：适配器命令域（通用 `InvokeAdapterOp` 分发、管理通道
//!   基础设施、缺省实例回退与写方法广播泛化），由主 `apply_command` 一行
//!   委托进来（见 `agent/commands.rs`）；
//! - [`legacy`]：迁移兼容层——旧 QQ 命令变体的处理臂（N+2 随协议变体删除；
//!   01b §D.1 / §D.5，grep 验收豁免）。
//! - [`test_support`]：两个命令域测试模块共用的 mock 与装配助手（仅测试）。

pub mod bridge;
pub(crate) mod commands;
pub(crate) mod legacy;

#[cfg(test)]
pub(crate) mod test_support;
