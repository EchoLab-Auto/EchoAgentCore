//! 联邦执行层（federation Phase 2）：远程工具调用的大脑侧与执行端。
//!
//! - [`router`]：调用调度——`call_id → oneshot` 路由表，消费链路飞来的
//!   `InvokeAccepted/InvokeOutput/InvokeResult`，按 call_id 派发给等待中的
//!   代理工具；同时承担执行端入口（收 `Invoke` → 白名单裁决 → 本机注册表
//!   执行 → 回传）。
//! - [`remote_tool`]：`RemoteTool` 代理工具——`Tool` trait 实现，`execute()`
//!   内发 `FedFrame::Invoke` 并等待终态。
//!
//! - [`ports`]：出口集合（`FederationPorts`）——组合根装配期构造一份，
//!   显式下发给 Agent 与联邦泵。
//!
//! 设计约束（RFC §4）：不改 `Tool` trait；远程工具以 `<peer>__<tool>` 命名
//! 显式注册；路径校验由执行端在本机工作区根内完成；取消/超时尽力送达。
//!
//! ## 为什么这是一个独立 crate（全模块化 S3）
//!
//! 这四件事（路由表 / 代理工具 / 聚合器 / 跨机查询）**只依赖机制层**：
//! `agent-module-federation`（线协议与链路）、`agent-module-tools`（工具词汇与注册表）、
//! `echo-defs`（定义）、`agent-module-workspace`（它消费的查询 trait）。
//! 它们此前住在宿主 `packages/federation` 里，唯一的宿主依赖是
//! `FederationPorts::command_handler` 的类型 `Fn(&Agent, cmd)`——而真正需要
//! 的只是 `emit`（现在是 [`ports::FederationAdmin`] 逐动作接口）。把这一层
//! 改成不依赖 `Agent`，整块 1,698 行就能搬出来。
//!
//! 于是"联邦"成为一个可独立禁用、可独立替换的能力：禁用后内核照常启动，
//! 远程工具报「联邦未接线」、远程目录降级为不可采集（显式降级，不 panic）。

pub mod admin;
pub mod aggregator;
pub mod commands;
pub mod host_keys;
pub mod ports;
pub mod remote_tool;
pub mod router;
pub mod workspace_query;

pub use admin::{Emit as FederationEmit, FederationAdminHost};
pub use ports::FederationAdmin;
pub use remote_tool::{
    proxy_schema, remote_tool_name, remote_tool_prefix, sanitize_peer_component, RemoteTool,
};
pub use router::{remote_tool_candidates, ExecutorPolicy, InvokeRouter, PendingInvoke};
pub use workspace_query::{workspace_query, FederationWorkspaceQuery, WorkspaceQueryHandle};

pub use commands::{FederationCommandBridge, COMMANDS as FEDERATION_COMMANDS};
pub use ports::{
    notify_remote_subagent, path_within_roots, AggregateDeliver, FederationPorts, RemoteInvoker,
    RemoteQuerier, RemoteSubagentNotifier,
};
