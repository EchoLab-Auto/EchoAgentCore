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
//! ## 为什么这是一个独立 crate（全插件化 S3）
//!
//! 这四件事（路由表 / 代理工具 / 聚合器 / 跨机查询）**只依赖机制层**：
//! `echo-federation`（线协议与链路）、`echo-tool`（工具词汇与注册表）、
//! `echo-defs`（定义）、`echo-plugin-workspace`（它消费的查询 trait）。
//! 它们此前住在宿主 `packages/federation` 里，唯一的宿主依赖是
//! `FederationPorts::command_handler` 的类型 `Fn(&Agent, cmd)`——而真正需要
//! 的只是 `emit`（见 [`ports::FederationCommandHandler`]）。把这一个类型改成
//! 不依赖 `Agent`，整块 1,698 行就能搬出来。
//!
//! 于是"联邦"成为一个可独立禁用、可独立替换的能力：禁用后内核照常启动，
//! 远程工具报「联邦未接线」、远程目录降级为不可采集（显式降级，不 panic）。

pub mod aggregator;
pub mod commands;
pub mod ports;
pub mod remote_tool;
pub mod router;
pub mod workspace_query;

pub use remote_tool::{
    proxy_schema, remote_tool_name, remote_tool_prefix, sanitize_peer_component, RemoteTool,
};
pub use router::{remote_tool_candidates, ExecutorPolicy, InvokeRouter, PendingInvoke};
pub use workspace_query::{workspace_query, FederationWorkspaceQuery, WorkspaceQueryHandle};

pub use commands::{FederationCommandBridge, COMMANDS as FEDERATION_COMMANDS};
pub use ports::{
    notify_remote_subagent, path_within_roots, AggregateDeliver, FederationCommandHandler,
    FederationCommandFuture, FederationPorts, RemoteQuerier, RemoteSubagentNotifier, RemoteInvoker,
};

/// 插件 id（宿主既有的联邦插件 id 与本常量同源字面量）。
pub const PLUGIN_ID: &str = "echo-agent.federation";

/// 插件清单。
pub fn manifest(version: &str) -> echo_plugin::PluginManifest {
    echo_plugin::PluginManifest::builtin(
        PLUGIN_ID,
        "联邦",
        version,
        "federation",
        "federation",
        "Core↔Core 对等链路：远程工具 / 跨机委派 / 跨机只读查询 / 会话迁移",
    )
}

/// 构造插件：命令域（7 条联邦 / 自更新命令）+ persona 门控。
///
/// 三件事都在这里声明，宿主只需给一份 [`FederationPorts`] 与事件出口：
///
/// | 贡献 | 内容 |
/// |---|---|
/// | `mount` | persona 门控开关（联邦能力随白名单启停） |
/// | `Contribution::Command` | 7 条命令（见 [`commands::COMMANDS`]） |
///
/// 实现留在组合根（`Federation` 句柄与 `ConfigStore` 是它的所有物），
/// 本插件负责**归属关系**与未接线时的兜底语义。
pub fn plugin(
    version: &str,
    ports: std::sync::Arc<FederationPorts>,
    emit: std::sync::Arc<dyn Fn(echo_protocol::BackendEvent) + Send + Sync>,
    gate: impl Fn(bool) + Send + Sync + 'static,
) -> std::sync::Arc<dyn echo_plugin::Plugin> {
    use echo_plugin::BuiltinPlugin;
    let gate = std::sync::Arc::new(gate);
    let gate_mount = gate.clone();
    let bridge = std::sync::Arc::new(commands::FederationCommandBridge::new(ports, emit));
    std::sync::Arc::new(
        BuiltinPlugin::new(manifest(version), move |_mctx| {
            gate_mount(true);
            let gate_unmount = gate_mount.clone();
            Ok(vec![echo_context::Disposer::from_fn(move || {
                gate_unmount(false)
            })])
        })
        .with_commands(commands::COMMANDS)
        .with_command_handler_async(move |name, payload| {
            let bridge = bridge.clone();
            Box::pin(async move {
                bridge
                    .handle(&name, payload)
                    .await
                    .unwrap_or_else(|| Err(format!("命令 {name} 被 echo-agent.federation 声明，但 federation 处理器没有认领它")))
            })
        }),
    )
}
