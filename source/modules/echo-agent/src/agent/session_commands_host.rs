//! 会话写命令域的**宿主能力实现**（解耦计划 S6 补口）。
//!
//! 与 `session_view_host` / `provider_host` 同构：**校验、文案、进度、时间线
//! 重发在模块**（`echo_module_session::commands`），宿主只提供"只有它做得了"
//! 的四件事：
//!
//! | 宿主提供 | 唯一性来源 |
//! |---|---|
//! | [`resolve_trunk`](SessionCommandHost::resolve_trunk) | 人格注册表在 `AgentManager`；模块不认识它 |
//! | [`compact`](SessionCommandHost::compact) | 压缩策略在 `echo-module-loop`、provider 在 `Agent.model`——模块不能依赖模块 |
//! | [`emit_context_snapshot`](SessionCommandHost::emit_context_snapshot) | 快照要算 token 预算与上下文块 |
//! | [`forget_session_provider`](SessionCommandHost::forget_session_provider) | 会话级 API 覆盖表在 `Agent.model` |
//!
//! `trunk` / `resolve_trunk` 返回 `Arc<TrunkStore>` 是**语义快照**而非借用：
//! 模块拿到的是可独立持有的句柄，不必认识宿主的存储布局（与 `session_view`
//! 的"接缝不返回引用"同一条纪律）。

use std::sync::Arc;

use echo_module_session::commands::SessionCommandHost;
use echo_module_session::TrunkStore;
use echo_protocol::BackendEvent;

use crate::agent::Agent;

/// 宿主适配器：以"某个 persona"为缺省视角的会话写命令。
pub struct AgentSessionCommandHost {
    agent: Arc<Agent>,
}

impl AgentSessionCommandHost {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self { agent }
    }

    /// 解析目标 persona（`None` / 解析不到 = 本 agent）。
    fn target(&self, team_id: &str) -> Option<Arc<Agent>> {
        self.agent
            .registry()
            .and_then(|manager| manager.resolve(team_id))
    }
}

impl SessionCommandHost for AgentSessionCommandHost {
    fn self_team_id(&self) -> Option<String> {
        self.agent.team_id()
    }

    fn resolve_trunk(&self, team_id: &str) -> Option<Arc<TrunkStore>> {
        self.target(team_id)
            .map(|agent| Arc::new(agent.trunk.clone()))
    }

    fn trunk(&self) -> Arc<TrunkStore> {
        Arc::new(self.agent.trunk.clone())
    }

    fn forget_session_provider(&self, session_id: &str) {
        self.agent.model.session_providers.remove(session_id);
    }

    fn emit(&self, event: BackendEvent) {
        self.agent.emit(event);
    }

    fn emit_context_snapshot<'a>(
        &'a self,
        session_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.agent.emit_context_snapshot(Some(session_id)).await;
        })
    }

    fn compact<'a>(
        &'a self,
        team_id: Option<String>,
        session_id: &'a str,
        keep_recent: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        let agent = Arc::clone(&self.agent);
        let session = session_id.to_string();
        Box::pin(async move {
            // team 路由：有 team_id 就压到目标人格上；解析不到且不是自己
            // → 「不存在」（不回退到自身——那会把 A 的历史按 B 的意图压掉）。
            match team_id.as_deref() {
                Some(id) => match agent.registry().and_then(|manager| manager.resolve(id)) {
                    Some(target) => target.compact_session_history(&session, keep_recent).await,
                    None if agent.team_id().as_deref() != Some(id) => {
                        Err(format!("team {id} 不存在"))
                    }
                    None => agent.compact_session_history(&session, keep_recent).await,
                },
                None => agent.compact_session_history(&session, keep_recent).await,
            }
        })
    }
}
