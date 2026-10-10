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
/// 工作区会话管理插件（workspace）：Panel 侧多会话/多目录管理 + git 状态，
/// 模型侧 `workspace` 工具与激活会话的系统提示注入。
pub const WORKSPACE_PLUGIN_ID: &str = "echo-agent.workspace";
/// Subagent 委派插件 id（包 id 同名：`spawn_subagent` 工具 + subagent 技能随包门控）。
pub const SUBAGENT_PLUGIN_ID: &str = "echo-agent.subagent";
/// 循环模式互斥插件（按 persona 二选一，single 为推导兜底，也是默认）：
/// - `loop.single`：单会话循环（默认）——同一会话内 turn 串行排队，
///   无会话管理 UI（会话卡/全局分组隐藏）、回执分支不可见（后端不发射
///   `ReplyBranch*` 事件，分支照常执行合并）；
/// - `loop.parallel`：并行多会话循环——同一会话可并发分支，
///   会话列表/全局会话/可见回执分支全套。
///   两者 mount 的是同一个 TurnRunner（驱动本体），真实效果是 per-persona
///   白名单推导出的 `LoopMode`（见 `TeamMember::loop_mode`）。
pub const SINGLE_LOOP_PLUGIN_ID: &str = "echo-agent.loop.single";
pub const PARALLEL_LOOP_PLUGIN_ID: &str = "echo-agent.loop.parallel";

pub const GATED_PLUGIN_IDS: [&str; 5] = [
    TOOLS_BUILTIN_PLUGIN_ID,
    SKILLS_DIR_PLUGIN_ID,
    ADAPTER_QQ_PLUGIN_ID,
    WORKSPACE_PLUGIN_ID,
    SUBAGENT_PLUGIN_ID,
];

/// 全部内置插件 id（与 `scripts/update.sh` 的插件校验清单一致；
/// 由 `source/core/tests/update_script_plugins.rs` 守护两者不漂移）。
/// LLM Provider 插件 id（名义挂载，重启生效）。
pub const PROVIDER_LLM_PLUGIN_ID: &str = "echo-agent.provider.llm";

/// 供插件黑名单移除迁移物化白名单时使用。
pub const BUILTIN_PLUGIN_IDS: [&str; 9] = [
    TOOLS_BUILTIN_PLUGIN_ID,
    ADAPTER_QQ_PLUGIN_ID,
    SKILLS_DIR_PLUGIN_ID,
    WORKSPACE_PLUGIN_ID,
    SUBAGENT_PLUGIN_ID,
    MANAGEMENT_PANEL_PLUGIN_ID,
    SINGLE_LOOP_PLUGIN_ID,
    PARALLEL_LOOP_PLUGIN_ID,
    PROVIDER_LLM_PLUGIN_ID,
];

/// 某 persona 的白名单是否允许一个插件 id。
///
/// 语义：白名单非空 = 仅列出的插件；空白名单 = 全部允许。
/// 启动期逐人格门控（core main.rs）与运行期 `Agent::apply_capabilities` /
/// `Agent::reapply_plugin_gating` 共用本判定，避免两套规则漂移。
/// （插件黑名单 `disabled_plugins` 已于 2026-09-11 移除，白名单单轨。）
pub fn profile_allows_plugin(profile: &crate::config::TeamMember, plugin_id: &str) -> bool {
    profile.enabled_plugins.is_empty() || profile.enabled_plugins.iter().any(|p| p == plugin_id)
}

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
    fn profile_allows_plugin_is_whitelist_only() {
        use crate::config::TeamMember;
        // 空白名单 = 全部允许
        let member = TeamMember::default();
        assert!(profile_allows_plugin(&member, TOOLS_BUILTIN_PLUGIN_ID));
        // 白名单非空 = 仅列出项
        let member = TeamMember {
            enabled_plugins: vec![SKILLS_DIR_PLUGIN_ID.to_string()],
            ..Default::default()
        };
        assert!(profile_allows_plugin(&member, SKILLS_DIR_PLUGIN_ID));
        assert!(!profile_allows_plugin(&member, TOOLS_BUILTIN_PLUGIN_ID));
    }
}
