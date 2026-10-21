//! 工作区插件的跨机查询适配器（解耦计划 Phase 3 ⑤）。
//!
//! `echo-plugin-workspace` 不认识联邦：它只声明 [`RemoteWorkspaceQuery`] 的三个
//! 语义方法（git 状态 / 列文件 / 浏览目录），由宿主实现。本模块就是把
//! 「语义方法」映射到「联邦 `QueryKind` + `remote_querier`」的那一层。
//!
//! 为什么适配器在宿主侧：查询要发 `FedFrame::Query`、要吃 `allow_queries`
//! 授权、要按 peer 路由——全部是联邦子系统的职责。插件侧只关心载荷形状。
//!
//! 未接线（联邦关闭）时组合根不注入本类型，插件的远程目录自然降级为
//! 「远程不可采集」占位——单机部署零影响。

use echo_federation::QueryKind;
// 定义层（S12）：trait 的声明方是 workspace 插件、实现方是本 crate，
// 但它既不属于谁——放在定义层，两边都不必依赖对方。
use echo_defs::workspace::{QueryFuture, RemoteWorkspaceQuery};

/// 用联邦 `RemoteQuerier` 实现工作区插件的跨机查询。
///
/// 每个方法都只做一件事：挑对 `QueryKind`，把结果原样交回插件——载荷的
/// 解析（`WorkspaceGitInfo` / `WorkspaceFileEntry`）在插件侧，宿主不插手。
pub struct FederationWorkspaceQuery {
    /// 远程查询出口（组合根装配期注入；None = 联邦未接线）。
    ///
    /// P3 终态：出口作为**字段**持有，不再从 kernel cell 回查——适配器
    /// 的生命周期与联邦装配绑定，装配期拿到的就是运行期要用的。
    querier: Option<crate::RemoteQuerier>,
}

impl FederationWorkspaceQuery {
    /// 用组合根注入的出口构造（None = 未接线，查询立即报可读错误）。
    pub fn new(querier: Option<crate::RemoteQuerier>) -> Self {
        Self { querier }
    }

    /// 发起一次联邦查询；未接线时立即返回可读错误（插件降级为占位）。
    fn query(&self, peer: String, kind: QueryKind, subject: String) -> QueryFuture {
        match &self.querier {
            Some(querier) => querier(peer, kind, subject),
            None => Box::pin(async {
                Err("联邦未接线：远程工作区查询不可用".to_string())
            }),
        }
    }
}

impl RemoteWorkspaceQuery for FederationWorkspaceQuery {
    fn git_status(&self, peer: String, path: String) -> QueryFuture {
        self.query(peer, QueryKind::WorkspaceGitStatus, path)
    }

    fn list_files(&self, peer: String, path: String) -> QueryFuture {
        self.query(peer, QueryKind::WorkspaceFiles, path)
    }

    fn browse(&self, peer: String, path: String) -> QueryFuture {
        self.query(peer, QueryKind::BrowseDirectories, path)
    }
}

/// 类型别名，便于组合根少写一层装箱。
pub type WorkspaceQueryHandle = std::sync::Arc<dyn RemoteWorkspaceQuery>;

/// 构造适配器句柄（组合根装配期调用）。
///
/// `querier = None` 时插件侧降级为「远程不可采集」占位——单机部署零影响。
pub fn workspace_query(querier: Option<crate::RemoteQuerier>) -> WorkspaceQueryHandle {
    std::sync::Arc::new(FederationWorkspaceQuery::new(querier))
}

/// 保证适配器的 future 确实是 `Send`（跨 await 点持有闭包时必需）。
const _: fn() -> QueryFuture = || {
    let adapter = FederationWorkspaceQuery::new(None);
    adapter.query(String::new(), QueryKind::NodeStatus, String::new())
};

#[cfg(test)]
mod tests {
    use super::*;

    /// 未接线联邦时，三个方法都必须返回可读错误而不是 panic——插件据此
    /// 显示「远程不可采集」，本机功能不受影响。
    #[tokio::test]
    async fn unconnected_federation_degrades_with_readable_error() {
        let adapter = FederationWorkspaceQuery::new(None);
        for future in [
            adapter.git_status("peer-a".into(), "/srv/repo".into()),
            adapter.list_files("peer-a".into(), "/srv/repo".into()),
            adapter.browse("peer-a".into(), "/srv".into()),
        ] {
            let error = future.await.expect_err("未接线时必须是错误");
            assert!(error.contains("联邦未接线"), "{error}");
        }
    }

    #[test]
    fn handle_is_object_safe_and_arc_shareable() {
        let handle: WorkspaceQueryHandle = workspace_query(None);
        let cloned = std::sync::Arc::clone(&handle);
        // 仅断言 trait 对象可共享（不实际发查询）。
        assert_eq!(std::sync::Arc::strong_count(&cloned), 2);
    }
}
