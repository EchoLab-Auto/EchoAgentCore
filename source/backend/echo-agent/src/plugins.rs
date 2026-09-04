//! Agent plugin host — bridges `echo_plugin` registry to the agent's concrete
//! registries (tools, skills, services) so built-in modules mount as plugins.
//!
//! The Agent owns a [`PluginRegistry`]; the composition root registers
//! built-in plugins (tools/skills/adapters/loop/…), and the host implements
//! the narrow sinks plugins use to register their effects. Plugins never
//! import echo-agent; the host is the only adapter.

use std::sync::Arc;

use echo_plugin::{
    MountContext, PluginDescriptor, PluginKind, PluginRegistry, PluginSkillSink, PluginToolHandle,
    PluginToolSink,
};

use crate::skill::Skill;
use crate::tool::ToolRegistry;

/// 内置插件 id（组合根与挂/卸效果共用，避免字符串漂移）。
pub const TOOLS_BUILTIN_PLUGIN_ID: &str = "echo-agent.tools.builtin";
pub const ADAPTER_QQ_PLUGIN_ID: &str = "echo-agent.adapter.qq";
pub const SKILLS_DIR_PLUGIN_ID: &str = "echo-agent.skills.dir";
pub const MANAGEMENT_PANEL_PLUGIN_ID: &str = "echo-agent.management.panel";

/// 编排模式互斥子插件（按 persona 二选一，single 为推导兜底）：
/// - single：单任务编排——无会话管理 UI（会话卡/全局分组隐藏）、
///   回执分支不可见（后端不发射 ReplyBranch* 事件，分支照常执行合并）；
/// - chatbot：多任务并行编排——会话列表/全局会话/可见回执分支全套。
/// 两者均为名义挂载（空闭包），真实效果是 per-persona 白名单推导出的
/// `OrchestrationMode`（见 `TeamMember::orchestration_mode`）。
pub const SINGLE_ORCHESTRATION_PLUGIN_ID: &str = "echo-agent.orchestration.single";
pub const CHATBOT_ORCHESTRATION_PLUGIN_ID: &str = "echo-agent.orchestration.chatbot";

/// 旧三特性插件 id（已合并为 orchestration 互斥子插件，不再注册）：
/// 仅用于配置迁移映射与向后兼容推导。
pub const REPLY_BRANCH_PLUGIN_ID: &str = "echo-agent.branch.reply";
pub const GLOBAL_SESSION_PLUGIN_ID: &str = "echo-agent.session.global";
pub const CHAT_SESSIONS_PLUGIN_ID: &str = "echo-agent.chatbot.sessions";

/// 推导编排模式时视为 chatbot 的全部 id（新 id + 旧三 id）。
pub const CHATBOT_MODE_IDS: [&str; 4] = [
    CHATBOT_ORCHESTRATION_PLUGIN_ID,
    REPLY_BRANCH_PLUGIN_ID,
    GLOBAL_SESSION_PLUGIN_ID,
    CHAT_SESSIONS_PLUGIN_ID,
];

/// 旧三特性 id → chatbot 子插件 id 的归一化（配置迁移与 SaveTeam 防御共用）：
/// 把列表中的旧 id 替换为 `CHATBOT_ORCHESTRATION_PLUGIN_ID`，去重、保序。
/// 返回是否有改动。
pub fn normalize_mode_plugins(list: &mut Vec<String>) -> bool {
    let mut changed = false;
    for item in list.iter_mut() {
        if matches!(
            item.as_str(),
            REPLY_BRANCH_PLUGIN_ID | GLOBAL_SESSION_PLUGIN_ID | CHAT_SESSIONS_PLUGIN_ID
        ) {
            *item = CHATBOT_ORCHESTRATION_PLUGIN_ID.to_string();
            changed = true;
        }
    }
    if changed {
        let mut seen = std::collections::HashSet::new();
        list.retain(|item| seen.insert(item.clone()));
    }
    changed
}

/// mount 有真实包维度效果（工具/技能批量启停）的插件。persona 白名单的
/// 启动期门控按此表遍历（management.panel 无 per-persona 注册表效果，
/// 不在表内）。
pub const GATED_PLUGIN_IDS: [&str; 3] = [
    TOOLS_BUILTIN_PLUGIN_ID,
    ADAPTER_QQ_PLUGIN_ID,
    SKILLS_DIR_PLUGIN_ID,
];

/// Tool sink adaptor: wraps the agent ToolRegistry behind the plugin seam.
pub struct ToolSink {
    registry: Arc<ToolRegistry>,
}

impl ToolSink {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }
}

struct RegisteredTool {
    name: String,
    registry: Arc<ToolRegistry>,
}

impl PluginToolHandle for RegisteredTool {
    fn dispose(self: Arc<Self>) {
        // ToolRegistry only supports removal via reversible-registration
        // disposers; keep a no-op here because builtin tools register at boot
        // and are removed through the registry's own disposer list.
        let _ = (self.name.as_str(), &self.registry);
    }
}

impl PluginToolSink for ToolSink {
    fn register_tool(&self, _name: &str, _description: &str) -> Arc<dyn PluginToolHandle> {
        // Builtin plugins use the ToolRegistry directly through `MountContext`
        // helpers in the composition root; this seam exists for future
        // external tool plugins (data-driven tool registration).
        Arc::new(RegisteredTool {
            name: _name.to_string(),
            registry: Arc::clone(&self.registry),
        })
    }
}

/// Skill sink adaptor: registers a skill directory (data plugin hot-reload).
pub struct SkillSink {
    skill_dir: String,
}

impl SkillSink {
    pub fn new(skill_dir: impl Into<String>) -> Self {
        Self {
            skill_dir: skill_dir.into(),
        }
    }
}

impl PluginSkillSink for SkillSink {
    fn register_skill_dir(&self, dir: &str) -> Result<echo_context::Disposer, String> {
        let _ = dir;
        // Skills are managed by the agent's own hot-reload task keyed on
        // `[agent].skills_dir`; the sink is a placeholder for per-plugin
        // skill directories (Phase 2 of the plugin architecture).
        let _ = &self.skill_dir;
        Ok(echo_context::Disposer::from_fn(|| {}))
    }
}

/// The agent's plugin host: registry + descriptors + runtime toggling.
pub struct PluginHost {
    pub registry: PluginRegistry,
    /// Mount context provided to every plugin at mount time.
    mount_ctx: std::sync::Mutex<Option<MountContext>>,
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginHost {
    pub fn new() -> Self {
        Self {
            registry: PluginRegistry::new(),
            mount_ctx: std::sync::Mutex::new(None),
        }
    }

    /// Attach the mount context (tools/skills/services). Call once at boot
    /// before mounting anything.
    pub fn set_mount_ctx(&self, ctx: MountContext) {
        *self.mount_ctx.lock().unwrap() = Some(ctx);
    }

    fn ctx(&self) -> MountContext {
        self.mount_ctx.lock().unwrap().clone().unwrap_or_default()
    }

    /// Register a plugin and mount it (composition root).
    ///
    /// A plugin whose persisted state is disabled (see
    /// [`PluginRegistry::apply_disabled`]) is registered but **not mounted**:
    /// disabled plugins must never acquire side effects at boot.
    pub fn register_and_mount(&self, plugin: Arc<dyn echo_plugin::Plugin>) -> Result<(), String> {
        let id = plugin.manifest().id.clone();
        self.registry.register(Arc::clone(&plugin))?;
        if !self.registry.is_enabled(&id) {
            return Ok(());
        }
        self.registry
            .mount(&id, &self.ctx())
            .map_err(|e| format!("plugin {id} mount failed: {e}"))?;
        Ok(())
    }

    /// Descriptors for the Panel (sorted by id).
    pub fn descriptors(&self) -> Vec<PluginDescriptor> {
        let mut ds = self.registry.descriptors();
        ds.sort_by(|a, b| a.id.cmp(&b.id));
        ds
    }

    /// Runtime toggle; persists via the configured `disabled_plugins` hook.
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        self.registry.set_enabled(id, enabled, &self.ctx())
    }

    pub fn plugins_of_kind(&self, kind: PluginKind) -> Vec<PluginDescriptor> {
        self.descriptors()
            .into_iter()
            .filter(|d| d.kind == kind.as_str())
            .collect()
    }
}

/// Mount a skill plugin: registers a `Skill` into the skill registry.
/// The caller supplies the full metadata (used by builtin skill plugins
/// that are compiled into the binary).
pub fn register_skill(
    skills: &skill_registry_handle::SkillRegistryHandle,
    name: &str,
    description: &str,
    keywords: Vec<String>,
    always: bool,
    category: &str,
    content: &str,
) {
    let mut guard = skills.lock();
    guard.register(Skill::direct(
        name,
        description,
        keywords,
        always,
        category,
        content,
    ));
}

/// A tiny handle so plugins can register skills without importing the agent's
/// Mutex type directly. Created by the composition root around the agent's
/// skill registry.
pub mod skill_registry_handle {
    use std::sync::Arc;

    use crate::skill::SkillRegistry;

    #[derive(Clone)]
    pub struct SkillRegistryHandle {
        inner: Arc<tokio::sync::Mutex<SkillRegistry>>,
    }

    impl SkillRegistryHandle {
        pub fn new(inner: Arc<tokio::sync::Mutex<SkillRegistry>>) -> Self {
            Self { inner }
        }
        pub fn lock(&self) -> tokio::sync::MutexGuard<'_, SkillRegistry> {
            self.inner.blocking_lock()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_mode_plugins_maps_legacy_ids() {
        // 单个旧 id → chatbot id
        let mut list = vec![REPLY_BRANCH_PLUGIN_ID.to_string()];
        assert!(normalize_mode_plugins(&mut list));
        assert_eq!(list, vec![CHATBOT_ORCHESTRATION_PLUGIN_ID.to_string()]);

        // 多个旧 id → 去重为一个 chatbot id（保序：出现在首个旧 id 位置）
        let mut list = vec![
            "echo-agent.tools.builtin".to_string(),
            REPLY_BRANCH_PLUGIN_ID.to_string(),
            GLOBAL_SESSION_PLUGIN_ID.to_string(),
            CHAT_SESSIONS_PLUGIN_ID.to_string(),
        ];
        assert!(normalize_mode_plugins(&mut list));
        assert_eq!(
            list,
            vec![
                "echo-agent.tools.builtin".to_string(),
                CHATBOT_ORCHESTRATION_PLUGIN_ID.to_string()
            ]
        );

        // 已有 chatbot id + 旧 id → 去重保留首个
        let mut list = vec![
            CHATBOT_ORCHESTRATION_PLUGIN_ID.to_string(),
            REPLY_BRANCH_PLUGIN_ID.to_string(),
        ];
        assert!(normalize_mode_plugins(&mut list));
        assert_eq!(list, vec![CHATBOT_ORCHESTRATION_PLUGIN_ID.to_string()]);
    }

    #[test]
    fn normalize_mode_plugins_noop_and_idempotent() {
        // 无旧 id：原样、无改动
        let mut list = vec![
            SINGLE_ORCHESTRATION_PLUGIN_ID.to_string(),
            "echo-agent.tools.builtin".to_string(),
        ];
        assert!(!normalize_mode_plugins(&mut list));
        assert_eq!(list.len(), 2);

        // 幂等：迁移后的列表再次归一化无改动
        let mut list = vec![GLOBAL_SESSION_PLUGIN_ID.to_string()];
        assert!(normalize_mode_plugins(&mut list));
        assert!(!normalize_mode_plugins(&mut list));
    }
}
