//! 工具管理面命令域的**宿主能力实现**（`RequestToolsList` / `ToggleTool`）。
//!
//! 与 `skills_host` 同构：清单投影与命令路由在模块里，宿主只提供**只有它
//! 知道**的三件事——per-persona 注册表、启停的持久化与逐人格重算、事件出口。

use std::sync::Arc;

use agent_module_management::protocol::BackendEvent;
use agent_module_tools::tool_commands::ToolsCommandHost;
use agent_module_tools::ToolRegistry;

use crate::agent::registry::AgentRegistry;
use crate::agent::Agent;

/// 宿主适配器。
pub struct AgentToolsCommandHost {
    origin: Arc<Agent>,
    registry: Option<Arc<dyn AgentRegistry>>,
}

impl AgentToolsCommandHost {
    pub fn new(origin: Arc<Agent>) -> Self {
        let registry = origin.registry();
        Self { origin, registry }
    }
}

impl ToolsCommandHost for AgentToolsCommandHost {
    fn registry(&self) -> Arc<ToolRegistry> {
        // 注册表本身就是 `Arc`（`Agent.tools`），共享句柄即同一份表。
        Arc::clone(&self.origin.tools)
    }

    /// 精确解析目标人格的注册表：命令里写谁就查谁，查不到即拒绝。
    ///
    /// **动态**读 `origin.registry()`（而不是构造时快照）：`core_agent` 的
    /// manager 绑定发生在宿主能力注册之后（`main` 尾部），构造时快照会是
    /// `None`——那样所有按 team 的查询都会误报“不存在”。
    fn resolve_registry(&self, team: &str) -> Result<Arc<ToolRegistry>, String> {
        let Some(registry) = self.origin.registry() else {
            return Err(format!("智能体 {team} 不存在或未运行（人格注册表未接线）"));
        };
        match registry.resolve(team) {
            Some(agent) => Ok(Arc::clone(&agent.tools)),
            None => Err(format!("智能体 {team} 不存在或未运行")),
        }
    }

    fn set_enabled(
        &self,
        name: &str,
        enabled: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        let origin = Arc::clone(&self.origin);
        let registry = self.registry.clone();
        let name = name.to_string();
        Box::pin(async move {
            // ① 本 persona 的表先改（表里没有 = not found）。
            if !origin.tools.set_enabled(&name, enabled).await {
                return false;
            }
            // ② 持久化：重启后仍生效（`[agent].disabled_tools`）。
            let snapshot = {
                let mut config = origin.model.config.write().await;
                config.disabled_tools.retain(|n| n != &name);
                if !enabled {
                    config.disabled_tools.push(name.clone());
                }
                config.clone()
            };
            origin.persist_config(&snapshot).await;
            // ③ 全局策略（进程级事实源，所有 persona 重算时读同一份）。
            if let Some(policy) = origin.global_policy() {
                policy.set_tool_enabled(&name, enabled);
            }
            // ④ 逐 persona 重算（此前只作用于管理面，其他人格照旧）。
            if let Some(registry) = registry {
                for running in registry.all() {
                    running.agent.reapply_tool_gating(&name, enabled).await;
                }
            }
            true
        })
    }

    fn emit(&self, event: BackendEvent) {
        self.origin.emit(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注册表是同一份（模块读到的就是模型看到的那张表）。
    #[tokio::test]
    async fn host_exposes_the_persona_tool_registry() {
        let agent = Arc::new(crate::agent::Agent::new(
            Arc::new(crate::agent::tests::MockProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                reply: String::new(),
            }),
            crate::config::AgentConfig::default(),
            crate::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
        ));
        let host = AgentToolsCommandHost::new(agent.clone());
        assert!(Arc::ptr_eq(&host.registry(), &agent.tools));
    }

    /// 表里没有的工具：回 `false`（面板显示 not found）。
    #[tokio::test]
    async fn toggling_an_unknown_tool_reports_false() {
        let agent = Arc::new(crate::agent::Agent::new(
            Arc::new(crate::agent::tests::MockProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                reply: String::new(),
            }),
            crate::config::AgentConfig::default(),
            crate::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
        ));
        let host = AgentToolsCommandHost::new(agent);
        assert!(!host.set_enabled("ghost", true).await);
    }

    /// 未绑定人格注册表时按 team 查询必须**明确拒绝**，不得回落到自己的表。
    #[tokio::test]
    async fn resolve_registry_without_manager_is_an_explicit_error() {
        let agent = Arc::new(crate::agent::Agent::new(
            Arc::new(crate::agent::tests::MockProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                reply: String::new(),
            }),
            crate::config::AgentConfig::default(),
            crate::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
        ));
        let host = AgentToolsCommandHost::new(agent);
        let error = host.resolve_registry("alix").unwrap_err();
        assert!(error.contains("alix"));
        assert!(error.contains("未接线"));
    }
}
