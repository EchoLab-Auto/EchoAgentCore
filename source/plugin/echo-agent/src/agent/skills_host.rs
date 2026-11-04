//! 技能插件命令域的**宿主能力实现**。
//!
//! `echo-plugin-skills` 的 `SkillsCommandHandler` 不认识 `Agent`——它只认识
//! [`SkillsCommandHost`] 的五项能力。本模块就是那五项的实现。
//!
//! ## 为什么这五项留在宿主（而文件格式 / 列表投影搬走了）
//!
//! | 能力 | 宿主才知道的原因 |
//! |---|---|
//! | `registry()` | 技能表是 **per-persona** 的（`Agent` 持有 `PromptService`） |
//! | `skills_dir()` | 写目录来自 `[agent].skills_dir`（宿主的配置类型） |
//! | `reload()` | 重载跨 `skills_dirs` 且要**逐人格生效** + 刷新提示词缓存 |
//! | `set_enabled()` | 启停要持久化 `disabled_skills` + 全局策略 + 逐人格重算 |
//! | `emit()` | 面板事件出口（team 标注 / 事件总线 / 进程汇聚点） |
//!
//! 反过来：**SKILL.md 的 frontmatter 长什么样、目录名校验、列表投影、
//! `.sources.json`、git 子进程**——全在插件里。那些才是能力本体。

use std::sync::Arc;

use echo_plugin_skills::commands::SkillsCommandHost;
use echo_plugin_skills::SkillRegistry;
use echo_protocol::BackendEvent;

use crate::agent::registry::AgentRegistry;
use crate::agent::Agent;

/// 宿主适配器：把"插件要的五件事"接到执行该命令的 `Agent` 上。
///
/// `registry` 是**弱引用**（同 `Agent` 对 `AgentManager` 的做法）：`Agent`
/// 强持有它、它弱持有兄弟人格的查询面，不成环。启停的逐人格重算靠它。
pub struct AgentSkillsCommandHost {
    origin: Arc<Agent>,
    registry: Option<Arc<dyn AgentRegistry>>,
}

impl AgentSkillsCommandHost {
    pub fn new(origin: Arc<Agent>) -> Self {
        let registry = origin.registry();
        Self { origin, registry }
    }
}

impl SkillsCommandHost for AgentSkillsCommandHost {
    fn registry(&self) -> Arc<tokio::sync::Mutex<SkillRegistry>> {
        // `PromptService::skills` 就是本 persona 的那份表（同一 `Arc`，
        // 插件拿到的是共享句柄——不 clone 表内容，只锁着读/改）。
        Arc::clone(&self.origin.prompt.skills)
    }

    fn skills_dir(&self) -> String {
        // 写目录是装配期配置的一部分；命令处理期间不会变。用 try_read
        // 而非 await（这里在同步上下文），拿不到锁时回空串让调用方报可读
        // 错误——比 panic 好。
        match self.origin.model.config.try_read() {
            Ok(config) => config.skills_dir.clone(),
            Err(_) => String::new(),
        }
    }

    fn reload(
        &self,
        write_dir: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + '_>>
    {
        let origin = Arc::clone(&self.origin);
        let write_dir = write_dir.to_string();
        Box::pin(async move { origin.reload_skills(&write_dir).await })
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
            // ① 本 persona 的表先改（表里没有 = not found，直接回 false）。
            let ok = {
                let mut skills = origin.prompt.skills.lock().await;
                skills.set_enabled(&name, enabled)
            };
            if !ok {
                return false;
            }
            origin.prompt.invalidate().await;
            // ② 持久化：重启后仍生效（`[agent].disabled_skills`）。
            let snapshot = {
                let mut config = origin.model.config.write().await;
                config.disabled_skills.retain(|n| n != &name);
                if !enabled {
                    config.disabled_skills.push(name.clone());
                }
                config.clone()
            };
            origin.persist_config(&snapshot).await;
            // ③ 全局策略（进程级）：所有 persona 重算时读同一份事实。
            if let Some(policy) = origin.global_policy() {
                policy.set_skill_enabled(&name, enabled);
            }
            // ④ 全局生效：最终状态 = 全局启停 ∧ 各 persona 名单，逐 persona
            // 重算（此前只作用于管理面，其他人格照旧——2026-10 修复）。
            if let Some(registry) = registry {
                for running in registry.all() {
                    running.agent.reapply_skill_gating(&name, enabled).await;
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

    /// 目录与表句柄都直接来自 agent（同一份，不是副本）。
    #[tokio::test]
    async fn host_exposes_the_persona_registry_and_directory() {
        let agent = Arc::new(crate::agent::Agent::new(
            Arc::new(crate::agent::tests::MockProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                reply: String::new(),
            }),
            crate::config::AgentConfig::default(),
            crate::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
        ));
        let expected_dir = agent.model.config.try_read().unwrap().skills_dir.clone();
        let host = AgentSkillsCommandHost::new(agent.clone());
        assert_eq!(host.skills_dir(), expected_dir);
        assert!(
            Arc::ptr_eq(&host.registry(), &agent.prompt.skills),
            "插件必须拿到同一份技能表（不是副本）"
        );
    }

    /// 表里没有的技能：启停回 `false`（面板据此显示 not found）。
    #[tokio::test]
    async fn enabling_an_unknown_skill_reports_false() {
        let agent = Arc::new(crate::agent::Agent::new(
            Arc::new(crate::agent::tests::MockProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                reply: String::new(),
            }),
            crate::config::AgentConfig::default(),
            crate::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
        ));
        let host = AgentSkillsCommandHost::new(agent);
        assert!(!host.set_enabled("ghost", true).await);
    }
}
