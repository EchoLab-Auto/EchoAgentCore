//! AgentSupervisor — 多 persona agent 的装配与解析。
//!
//! **单一真源（2026-10 修复）**：persona 注册表由
//! [`echo_agent::AgentManager`] 独占持有；本模块只做组合根友好的薄适配
//! （`get` / `get_exact` / `ids` / `personas`），**不再维护第二份 HashMap**。
//!
//! 修复背景：此前 `AgentSupervisor` 与 `AgentManager` 各持一份注册表，
//! 对「teams 非空但全部被禁用时是否合成 `default` 人格」的处理不一致
//! （supervisor 合成、manager 不合成），导致命令按 team_id 路由时
//! supervisor 有实例、manager 解析不到，Panel 报「智能体 default 不存在」。
//! 合并到单一真源后，此类分叉在结构上不可能再发生。
//!
//! 每个 `[agent.teams.*]` 条目 = 一个独立 `Agent`（独立 trunk / 会话文件 /
//! 系统提示词）。去主智能体（2026-09-13）后没有"默认人格"：命令路由由
//! `Agent::apply_command` 按显式 `team_id` 完成，本模块只负责构建与按 id
//! 解析（`get` / `get_exact`），以及给组合根提供 `personas()` 迭代。

use std::sync::Arc;

use echo_agent::agent_manager::RunningAgent;
use echo_agent::{Agent, AgentConfig, AgentManager, AgentProfile};

/// A persona instance: id + capability profile + the agent itself.
pub struct Persona {
    pub id: String,
    /// 能力画像（组合根用 `apply_capabilities` 重新应用门控）。
    pub profile: AgentProfile,
    pub agent: Arc<Agent>,
}

impl From<RunningAgent> for Persona {
    fn from(running: RunningAgent) -> Self {
        Self {
            id: running.id,
            profile: running.profile,
            agent: running.agent,
        }
    }
}

/// Thin adapter over the single-source-of-truth [`AgentManager`].
pub struct AgentSupervisor {
    manager: Arc<AgentManager>,
}

impl AgentSupervisor {
    /// Build the underlying manager. `make_agent(id, profile)` creates each
    /// Agent (the caller must have set its agent_id and config path).
    pub fn build(
        raw: &AgentConfig,
        make_agent: impl Fn(String, AgentProfile) -> Arc<Agent> + Send + Sync + 'static,
    ) -> Self {
        Self {
            manager: Arc::new(AgentManager::build(raw, make_agent)),
        }
    }

    /// 底层注册表句柄：组合根用它注入 config writer、设为进程级
    /// `global_manager`、注册运行期 agent factory——三者操作的是**同一个**
    /// 注册表（单一真源）。
    pub fn manager(&self) -> Arc<AgentManager> {
        Arc::clone(&self.manager)
    }

    pub fn get(&self, id: &str) -> Option<Persona> {
        self.manager.get(id).map(Persona::from)
    }

    /// 按 id 解析（未知返回 None）——路由用，绝无兜底。
    pub fn get_exact(&self, id: &str) -> Option<Persona> {
        self.get(id)
    }

    pub fn ids(&self) -> Vec<String> {
        self.manager.agent_ids()
    }

    pub fn personas(&self) -> Vec<Persona> {
        self.manager.all().into_iter().map(Persona::from).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_agent::TeamMember;

    /// 全部人格被禁用时：不得合成 `default` 兜底人格（否则 supervisor 与
    /// AgentManager 分叉，命令按 team_id=default 路由时 manager 解析不到，
    /// Panel 报「智能体 default 不存在」）。禁用名单仍须可列出/可重新启用。
    #[test]
    fn all_disabled_teams_leave_no_running_persona_but_keep_profiles() {
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "alix".into(),
            TeamMember {
                enabled: false,
                ..Default::default()
            },
        );
        let supervisor = AgentSupervisor::build(&raw, |_id, _profile| {
            panic!("disabled personas must not be instantiated")
        });
        assert!(supervisor.ids().is_empty());
        assert!(supervisor.personas().is_empty());
        assert_eq!(supervisor.manager().team_ids(), vec!["alix".to_string()]);
    }
}
