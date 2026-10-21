//! 联邦执行层的**兼容路径**（实现已外迁到 [`echo_plugin_federation`]）。
//!
//! 2026-11 全插件化 S3：路由表 / 代理工具 / 聚合器 / 跨机查询 / 出口集合
//! 共 1,698 行搬成独立 crate。搬家的唯一阻碍是 `FederationPorts` 上那条
//! 命令回调的类型 `Fn(&Agent, cmd)`——S11 之后它变成
//! [`FederationAdminHost`]（只含原子状态迁移的 trait），规则与命令域实现
//! 全在插件里，本 crate 对宿主仍然零依赖。
//!
//! 保留本模块是为了 `echo_agent::federation::…` 这条公开路径不变（组合根与
//! 集成测试都在用）；内容全部是 re-export，没有实现。

pub use echo_plugin_federation::aggregator;
pub use echo_plugin_federation::remote_tool;
pub use echo_plugin_federation::router;
pub use echo_plugin_federation::workspace_query;

pub use echo_plugin_federation::{
    notify_remote_subagent, path_within_roots, proxy_schema, remote_tool_candidates,
    remote_tool_name, remote_tool_prefix, sanitize_peer_component, AggregateDeliver,
    ExecutorPolicy, FederationCommandFuture, FederationPorts, FederationWorkspaceQuery,
    InvokeRouter, PendingInvoke, RemoteQuerier, RemoteSubagentNotifier, RemoteTool,
    WorkspaceQueryHandle,
};
// `SubagentStatus` 的**源头**是机制层（线协议词汇），插件只是 re-export。
pub use echo_federation::SubagentStatus;

/// node:// 远程文件 / 命令分流出口（类型与 `echo-plugin-tools` 共享）。
pub type RemoteInvoker = echo_plugin_federation::RemoteInvoker;

/// 联邦管理面的状态迁移出口（类型��义在插件里；别名给组合根）。
///
/// 2026-11 S11 之前这里叫 `FederationCommandHandler`（一条命令一个回调），
/// 那条形状把整条命令的处理留在组合根；现在是每个原子动作一个方法。
pub type FederationAdmin = echo_plugin_federation::FederationAdmin;
pub use echo_plugin_federation::admin::{FederationAdminHost, PeerSave, PeerSummary};

#[cfg(test)]
mod tests {
    /// 未接线的出口集合必须被**显式**观察为"未接线"（不是靠字段为 None 猜）。
    #[test]
    fn unlinked_ports_are_observably_unlinked() {
        let ports = FederationPorts::unlinked();
        assert!(!ports.is_linked());
        assert!(ports.admin.is_none());
    }

    use super::*;
}
