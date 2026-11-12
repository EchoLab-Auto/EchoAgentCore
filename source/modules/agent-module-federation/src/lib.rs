//! # agent-module-federation — 联邦模块
//!
//! 合并去中心化连接、Core 协议适配与联邦业务执行：连接机制位于
//! [`connection`]，不依赖 EchoAgent 业务；Core 的 `FedFrame`、`NodeCaps` 与
//! Panel 身份在根模块适配；远程工具、委派、查询和会话迁移沿用原有执行层。

pub mod admin;
pub mod aggregator;
pub mod commands;
pub mod connection;
pub mod error;
pub mod frame;
pub mod host_keys;
pub mod link;
pub mod panel;
pub mod ports;
pub mod protocol;
mod service;
pub use protocol::{FederatedNodeInfo, FederationLinkState, FederationPeerInfo};
pub use service::{FederationCommand, FederationEvent};
pub mod remote_tool;
pub mod router;

pub use admin::{Emit as FederationEmit, FederationAdminHost};
pub use commands::COMMANDS as FEDERATION_COMMANDS;
pub use connection::{decode_invite, encode_invite, generate_token, InvitePayload, INVITE_SCHEME};
pub use error::FederationError;
pub use frame::{
    call_origin, new_call_id, FedError, FedFrame, InvokeRequest, InvokeResult, InvokeVerdict,
    NodeCaps, NodeHello, OutputStream, QueryKind, QueryRequest, QueryResultFrame,
    SessionImportAckFrame, SessionImportFrame, SessionImportResultFrame, SubagentEventFrame,
    SubagentSpawnRequest, SubagentStatus, PROTOCOL_VERSION,
};
pub use link::{bearer_request, Federation, LinkEvent, LinkHandle, PeerConfig, PeerInfo};
pub use panel::{PanelAccess, PanelGate, PanelIdentity};
pub use ports::FederationAdmin;
pub use ports::{
    notify_remote_subagent, path_within_roots, AggregateDeliver, FederationPorts, RemoteInvoker,
    RemoteQuerier, RemoteSubagentNotifier,
};
pub use remote_tool::{
    proxy_schema, remote_tool_name, remote_tool_prefix, sanitize_peer_component, RemoteTool,
};
pub use router::{remote_tool_candidates, ExecutorPolicy, InvokeRouter, PendingInvoke};

/// 模块 id（宿主既有的联邦模块 id 与本常量同源字面量）。
pub const MODULE_ID: &str = "echo-agent.federation";

/// 模块清单。
pub fn manifest(version: &str) -> echo_module::ModuleManifest {
    echo_module::ModuleManifest::builtin(
        MODULE_ID,
        "联邦",
        version,
        "federation",
        "federation",
        "Core↔Core 对等链路：远程工具 / 跨机委派 / 跨机只读查询 / 会话迁移",
    )
}

/// 构造模块：命令域（7 条联邦 / 自更新命令）+ persona 门控。
///
/// 三件事都在这里声明，宿主只需给一份 [`FederationPorts`] 与事件出口：
///
/// | 贡献 | 内容 |
/// |---|---|
/// | `mount` | persona 门控开关（联邦能力随白名单启停） |
/// | `Contribution::Command` | 6 条命令（见 [`commands::COMMANDS`]） |
///
/// 实现留在组合根（`Federation` 句柄与 `ConfigStore` 是它的所有物），
/// 本模块负责**归属关系**与未接线时的兜底语义。
/// 联邦协议经管理面总线发送时的扩展挂载点：服务方提供命令名，调用方用
/// `COMMANDS` 里的名称把服务自己的 JSON 载荷交给本模块，不经集中式协议 crate。
pub async fn handle_command(
    admin: &dyn FederationAdminHost,
    name: &str,
    payload: serde_json::Value,
) -> Option<Result<serde_json::Value, String>> {
    admin::dispatch_service(admin, name, payload).await
}

pub fn module(version: &str) -> std::sync::Arc<dyn echo_module::Module> {
    use echo_module::{BuiltinModule, Inject, ServiceSlot};

    // **零参数**（全模块化 S6）：出口集合与事件出口都是装配期构造的进程级
    // 对象，经服务表解析（[`host_keys::PORTS`] / [`host_keys::EVENTS`]）。
    let ports = ServiceSlot::<std::sync::Arc<FederationPorts>>::new();
    let (bind_ports, serve_ports) = (ports.clone(), ports.clone());

    std::sync::Arc::new(
        BuiltinModule::new(manifest(version), move |mctx| {
            let mut disposers = vec![bind_ports.bind(mctx, &host_keys::PORTS)?];
            // persona 门控不在这里（宿主监听挂载/卸载事件统一拨）。
            disposers.shrink_to_fit();
            Ok(disposers)
        })
        .with_inject(Inject::new([host_keys::PORTS.name().to_string()]))
        .with_commands(commands::COMMANDS)
        .with_command_handler_async(move |name, payload| {
            let ports = serve_ports.clone();
            Box::pin(async move {
                let Some(ports) = ports.get() else {
                    return Err(format!(
                        "命令 {name} 到 {} 时模块尚未挂载（宿主接缝未绑定）",
                        MODULE_ID
                    ));
                };
                commands::FederationCommandBridge::new(ports)
                    .handle(&name, payload)
                    .await
                    .unwrap_or_else(|| {
                        Err(format!(
                            "命令 {name} 被 {MODULE_ID} 声明，但 federation 处理器没有认领它"
                        ))
                    })
            })
        }),
    )
}
