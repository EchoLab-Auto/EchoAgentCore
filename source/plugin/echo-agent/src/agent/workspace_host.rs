//! 工作区插件的**宿主能力实现**（解耦计划：命令域迁入插件）。
//!
//! `echo-plugin-workspace` 的命令处理器不认识 `Agent` / `TrunkStore` / 会话
//! 通道——它只认识 [`WorkspaceHost`] 的四个方法。本模块就是那四个方法的宿主
//! 实现：一个薄适配层，把"插件要什么"翻成"宿主有什么"。
//!
//! ## 为什么这么薄
//!
//! 每个方法都只做一次查表 + 一次转发：
//!
//! - [`store`](WorkspaceHost::store)：`manager.resolve(team_id)` → `workspace_store`；
//! - [`emit`](WorkspaceHost::emit)：`manager.resolve(team_id)` → `Agent::emit`
//!   （team 标注、事件总线、进程级汇聚点全部由它负责——插件拿不到也不需要
//!   这些知识）；
//! - [`browse_roots`](WorkspaceHost::browse_roots)：遍历全部 persona 的 store
//!   目录并集 ∪ HOME；
//! - [`remote_query`](WorkspaceHost::remote_query)：直接交出联邦适配器。
//!
//! **反过来说**：如果哪天这里变厚了（出现业务判断、格式转换、缓存），就说明
//! 有一件事被放错了层——它要么该进插件，要么该进 `Agent`。
//!
//! ## 不在这四个方法里的
//!
//! store 变更后的"广播列表 / 注册会话通道 / 恢复会话级 API"（原
//! `Agent::workspace_after_change`）**不需要**穿过这层：它是 store 的变更钩子，
//! 宿主在 `set_workspace_store` 时就接好了。插件改 store → 钩子触发 → 宿主自己
//! 走完整流程。这让命令处理器不必知道"会话通道"这种东西存在。

use std::sync::Arc;

use echo_plugin_workspace::commands::WorkspaceHost;
use echo_plugin_workspace::{RemoteWorkspaceQuery, WorkspaceStore};
use echo_protocol::{BackendEvent, BrowseRoot};

use crate::agent::Agent;

/// 用宿主现有的 agent 集合 + 联邦适配器实现 [`WorkspaceHost`]。
///
/// **为什么是闭包而不是 `Arc<AgentManager>`**：本层的全部工作就是"按 `team_id`
/// 找 agent"与"列出全部 agent"。把 manager 直接塞进来会让单测只能靠造一个
/// manager 才能测命令域；收成一个闭包后，生产传
/// `move || manager.all().map(|r| r.agent).collect()`，单测传
/// `move || vec![agent.clone()]`——**同一份实现，两种装配**。
pub struct AgentWorkspaceHost {
    /// 取全部运行中 agent（生产 = manager 快照；单测 = 单元素）。
    agents: Arc<dyn Fn() -> Vec<Arc<Agent>> + Send + Sync>,
    /// **无 team 的全局事件出口**（进程级汇聚点）。
    ///
    /// 为什么需要它：`RequestBrowseDirectories` 这类命令不带 `team_id`（目录
    /// 选择器是全局 UI），而 `manager.resolve(None)` 在去主智能体之后**没有
    /// 默认人格可回退**（它返回 `None`）。所以这里直接持有组合根的进程级出口
    /// ——与"事件总要有归属"这件事解耦：没有人格归属的事件就走汇聚点。
    global_emit: crate::EventSink,
    /// 跨机查询（`None` = 联邦未接线，插件降级为"远程不可采集"）。
    remote: Option<Arc<dyn RemoteWorkspaceQuery>>,
}

impl AgentWorkspaceHost {
    /// 用"全部 agent 的取数闭包"构造（组合根 / 单测共用）。
    pub fn from_agents(
        agents: impl Fn() -> Vec<Arc<Agent>> + Send + Sync + 'static,
        global_emit: crate::EventSink,
        remote: Option<Arc<dyn RemoteWorkspaceQuery>>,
    ) -> Self {
        Self {
            agents: Arc::new(agents),
            global_emit,
            remote,
        }
    }

    /// 按 `team_id` 找 agent。
    fn find(&self, team_id: &str) -> Option<Arc<Agent>> {
        (self.agents)()
            .into_iter()
            .find(|agent| agent.team_id().as_deref() == Some(team_id))
    }
}

impl WorkspaceHost for AgentWorkspaceHost {
    fn store(&self, team_id: &str) -> Option<Arc<WorkspaceStore>> {
        self.find(team_id).and_then(|agent| agent.workspace_store())
    }

    fn emit(&self, team_id: Option<&str>, event: BackendEvent) {
        // 有明确人格 → 走它的 `Agent::emit`（那里负责 team 标注 + 事件总线 +
        // 进程级汇聚点）。找不到就退到全局出口：**回执不能丢**，前端在等这条
        // `WorkspaceSessions` / `Error`，静默会转圈。
        if let Some(team_id) = team_id {
            if let Some(agent) = self.find(team_id) {
                agent.emit(event);
                return;
            }
            tracing::warn!(
                %team_id,
                "workspace event for unknown team; falling back to the process-level sink"
            );
        }
        // 无人格归属（浏览目录这类全局命令 / 人格刚被删）：进程级出口。
        (self.global_emit)(event);
    }

    fn browse_roots(&self) -> Vec<BrowseRoot> {
        let mut directories: Vec<echo_protocol::WorkspaceDirectory> = Vec::new();
        let mut labels: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // 全部运行中 persona 的工作区目录并集：选择器是给"人"用的，他可能要把
        // 另一个项目的目录加进来，所以得看得见全部（单个 persona 只看自己的）。
        for agent in (self.agents)() {
            let Some(store) = agent.workspace_store() else {
                continue;
            };
            let (sessions, _) = store.snapshot();
            for session in sessions {
                for directory in &session.directories {
                    labels
                        .entry(directory.path().to_string())
                        .or_insert_with(|| session.name.clone());
                    directories.push(directory.clone());
                }
            }
        }
        echo_plugin_workspace::browse_roots(&directories, &labels)
    }

    fn remote_query(&self) -> Option<Arc<dyn RemoteWorkspaceQuery>> {
        self.remote.clone()
    }
}
