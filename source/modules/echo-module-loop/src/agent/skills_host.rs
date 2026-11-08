//! 技能模块命令域的**宿主能力实现**。
//!
//! `echo-module-skills` 的 `SkillsCommandHandler` 不认识 `Agent`——它只认识
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
//! `.sources.json`、git 子进程**——全在模块里。那些才是能力本体。

use std::sync::Arc;

use echo_module_management::protocol::BackendEvent;
use echo_module_skills::commands::SkillsCommandHost;
use echo_module_skills::SkillRegistry;

use crate::agent::registry::AgentRegistry;
use crate::agent::Agent;

/// 宿主适配器：把"模块要的五件事"接到执行该命令的 `Agent` 上。
///
/// `registry` 是**弱引用**（同 `Agent` 对 `AgentManager` 的做法）：`Agent`
/// 强持有它、它弱持有兄弟人格的查询面，不成环。启停的逐人格重算靠它。
pub struct AgentSkillsCommandHost {
    origin: Arc<Agent>,
    registry: Option<Arc<dyn AgentRegistry>>,
    runtime: std::sync::RwLock<Option<Arc<echo_module_skills::runtime::SkillsRuntime>>>,
}

impl AgentSkillsCommandHost {
    pub fn attach_runtime(&self, runtime: Arc<echo_module_skills::runtime::SkillsRuntime>) {
        if let Ok(mut slot) = self.runtime.write() {
            *slot = Some(runtime);
        }
    }

    pub fn new(origin: Arc<Agent>) -> Self {
        let registry = origin.registry();
        Self {
            origin,
            registry,
            runtime: std::sync::RwLock::new(None),
        }
    }
}

impl SkillsCommandHost for AgentSkillsCommandHost {
    fn skills_dir(&self) -> String {
        self.origin
            .model
            .config
            .try_read()
            .map(|c| c.skills_dir.clone())
            .unwrap_or_default()
    }
    fn skill_dirs(&self) -> Vec<String> {
        self.origin
            .model
            .config
            .try_read()
            .map(|c| c.skills_dirs.clone())
            .unwrap_or_default()
    }
    fn apply_capabilities(
        &self,
        enabled: Vec<String>,
        disabled: Vec<String>,
        global_disabled: Vec<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let runtime = self.runtime.read().ok().and_then(|r| r.clone());
        Box::pin(async move {
            if let Some(runtime) = runtime {
                runtime
                    .apply_capabilities(enabled, disabled, global_disabled)
                    .await;
            }
        })
    }
    fn persist_enabled(
        &self,
        name: String,
        enabled: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let origin = self.origin.clone();
        let registry = self.registry.clone();
        Box::pin(async move {
            let snapshot = {
                let mut config = origin.model.config.write().await;
                config.disabled_skills.retain(|n| n != &name);
                if !enabled {
                    config.disabled_skills.push(name.clone());
                }
                config.clone()
            };
            origin.persist_config(&snapshot).await;
            if let Some(policy) = origin.global_policy() {
                policy.set_skill_enabled(&name, enabled);
            }
            if let Some(registry) = registry {
                for running in registry.all() {
                    running.agent.apply_capabilities(&running.profile).await;
                }
            }
        })
    }
    fn global_disabled(&self) -> Vec<String> {
        self.origin
            .global_policy()
            .map(|p| p.disabled_skills())
            .unwrap_or_else(|| {
                self.origin
                    .model
                    .config
                    .try_read()
                    .map(|c| c.disabled_skills.clone())
                    .unwrap_or_default()
            })
    }
    fn attach_prompt_pipeline(
        &self,
        refresher: echo_defs::prompt::SharedPromptRefresher,
        controller: Arc<dyn echo_defs::prompt::NamedCapabilityController>,
    ) {
        self.origin.attach_prompt_pipeline(refresher, controller);
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
            Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
        ));
        let expected_dir = agent.model.config.try_read().unwrap().skills_dir.clone();
        let host = AgentSkillsCommandHost::new(agent.clone());
        assert_eq!(host.skills_dir(), expected_dir);
        assert_eq!(
            host.skill_dirs(),
            agent.model.config.try_read().unwrap().skills_dirs
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
            Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
        ));
        let host = AgentSkillsCommandHost::new(agent);
        assert!(host.global_disabled().is_empty());
    }
}
