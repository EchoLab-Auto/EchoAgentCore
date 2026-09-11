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
/// 任务清单工具插件（checklist）：内置清单工具的包维度门控，
/// 可按 persona 白/黑名单单独启停（默认启用）。
pub const CHECKLIST_PLUGIN_ID: &str = "echo-agent.checklist";
pub const MANAGEMENT_PANEL_PLUGIN_ID: &str = "echo-agent.management.panel";

/// 循环模式互斥插件（按 persona 二选一，single 为推导兜底，也是默认）：
/// - `loop.single`：单会话循环（默认）——同一会话内 turn 串行排队，
///   无会话管理 UI（会话卡/全局分组隐藏）、回执分支不可见（后端不发射
///   `ReplyBranch*` 事件，分支照常执行合并）；
/// - `loop.parallel`：并行多会话循环——同一会话可并发分支，
///   会话列表/全局会话/可见回执分支全套。
/// 两者 mount 的是同一个 TurnRunner（驱动本体），真实效果是 per-persona
/// 白名单推导出的 `LoopMode`（见 `TeamMember::loop_mode`）。
pub const SINGLE_LOOP_PLUGIN_ID: &str = "echo-agent.loop.single";
pub const PARALLEL_LOOP_PLUGIN_ID: &str = "echo-agent.loop.parallel";

/// 旧驱动插件 id（模式插件取代后不再注册；配置加载时从名单剔除）。
pub const LEGACY_LOOP_RUNNER_PLUGIN_ID: &str = "echo-agent.loop.runner";

/// 旧编排模式 id（已由循环模式插件取代）：仅用于配置迁移映射。
pub const LEGACY_CHATBOT_MODE_IDS: [&str; 4] = [
    "echo-agent.orchestration.chatbot",
    "echo-agent.branch.reply",
    "echo-agent.session.global",
    "echo-agent.chatbot.sessions",
];
pub const LEGACY_SINGLE_MODE_ID: &str = "echo-agent.orchestration.single";

/// 推导循环模式时视为「并行」的全部 id（新 id + 旧编排模式 id）。
pub const PARALLEL_MODE_IDS: [&str; 5] = [
    PARALLEL_LOOP_PLUGIN_ID,
    LEGACY_CHATBOT_MODE_IDS[0],
    LEGACY_CHATBOT_MODE_IDS[1],
    LEGACY_CHATBOT_MODE_IDS[2],
    LEGACY_CHATBOT_MODE_IDS[3],
];

/// 名单归一化（配置加载与 SaveTeam 防御共用）：把旧编排模式 id 与旧驱动
/// 插件 id 折叠为循环模式插件 id——chatbot/旧特性 id → `loop.parallel`，
/// orchestration.single → `loop.single`，loop.runner → 剔除（模式插件取代）。
/// 去重、保序。返回是否有改动。
pub fn normalize_mode_plugins(list: &mut Vec<String>) -> bool {
    let mut changed = false;
    let mut normalized: Vec<String> = Vec::with_capacity(list.len());
    for item in list.iter() {
        let is_parallel = LEGACY_CHATBOT_MODE_IDS.contains(&item.as_str());
        let is_single = item == LEGACY_SINGLE_MODE_ID;
        let is_legacy_runner = item == LEGACY_LOOP_RUNNER_PLUGIN_ID;
        if !is_parallel && !is_single && !is_legacy_runner {
            normalized.push(item.clone());
            continue;
        }
        changed = true;
        let replacement = if is_parallel {
            Some(PARALLEL_LOOP_PLUGIN_ID)
        } else if is_single {
            Some(SINGLE_LOOP_PLUGIN_ID)
        } else {
            // 旧驱动插件 id：模式插件取代，配置里不再保留。
            None
        };
        if let Some(id) = replacement {
            if !normalized.iter().any(|existing| existing == id) {
                normalized.push(id.to_string());
            }
        }
    }
    if changed {
        *list = normalized;
    }
    changed
}

pub const GATED_PLUGIN_IDS: [&str; 4] = [
    TOOLS_BUILTIN_PLUGIN_ID,
    SKILLS_DIR_PLUGIN_ID,
    CHECKLIST_PLUGIN_ID,
    ADAPTER_QQ_PLUGIN_ID,
];

/// 某 persona 的白/黑名单是否允许一个插件 id。
///
/// 语义：白名单非空 = 仅列出的插件；黑名单优先（命中即拒绝）。
/// 启动期逐人格门控（core main.rs）与运行期 `Agent::apply_capabilities` /
/// `Agent::reapply_plugin_gating` 共用本判定，避免两套规则漂移。
pub fn profile_allows_plugin(profile: &crate::config::TeamMember, plugin_id: &str) -> bool {
    if !profile.enabled_plugins.is_empty()
        && !profile.enabled_plugins.iter().any(|p| p == plugin_id)
    {
        return false;
    }
    if profile.disabled_plugins.iter().any(|p| p == plugin_id) {
        return false;
    }
    true
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
    fn normalize_mode_plugins_maps_legacy_ids() {
        // 单个旧 chatbot id → loop.parallel
        let mut list = vec![LEGACY_CHATBOT_MODE_IDS[0].to_string()];
        assert!(normalize_mode_plugins(&mut list));
        assert_eq!(list, vec![PARALLEL_LOOP_PLUGIN_ID.to_string()]);

        // 旧三特性 id → 去重为一个 loop.parallel（保序：出现在首个旧 id 位置）
        let mut list = vec![
            "echo-agent.tools.builtin".to_string(),
            LEGACY_CHATBOT_MODE_IDS[1].to_string(),
            LEGACY_CHATBOT_MODE_IDS[2].to_string(),
            LEGACY_CHATBOT_MODE_IDS[3].to_string(),
        ];
        assert!(normalize_mode_plugins(&mut list));
        assert_eq!(
            list,
            vec![
                "echo-agent.tools.builtin".to_string(),
                PARALLEL_LOOP_PLUGIN_ID.to_string()
            ]
        );

        // 已有 parallel id + 旧 id → 去重保留首个
        let mut list = vec![
            PARALLEL_LOOP_PLUGIN_ID.to_string(),
            LEGACY_CHATBOT_MODE_IDS[0].to_string(),
        ];
        assert!(normalize_mode_plugins(&mut list));
        assert_eq!(list, vec![PARALLEL_LOOP_PLUGIN_ID.to_string()]);
    }

    #[test]
    fn normalize_mode_plugins_folds_single_and_drops_legacy_runner() {
        // orchestration.single → loop.single
        let mut list = vec![LEGACY_SINGLE_MODE_ID.to_string()];
        assert!(normalize_mode_plugins(&mut list));
        assert_eq!(list, vec![SINGLE_LOOP_PLUGIN_ID.to_string()]);

        // 旧驱动插件 id 被剔除（模式插件取代），其余保序
        let mut list = vec![
            LEGACY_LOOP_RUNNER_PLUGIN_ID.to_string(),
            "echo-agent.tools.builtin".to_string(),
            LEGACY_SINGLE_MODE_ID.to_string(),
        ];
        assert!(normalize_mode_plugins(&mut list));
        assert_eq!(
            list,
            vec![
                "echo-agent.tools.builtin".to_string(),
                SINGLE_LOOP_PLUGIN_ID.to_string()
            ]
        );
    }

    #[test]
    fn normalize_mode_plugins_noop_and_idempotent() {
        // 无旧 id：原样、无改动
        let mut list = vec![
            SINGLE_LOOP_PLUGIN_ID.to_string(),
            "echo-agent.tools.builtin".to_string(),
        ];
        assert!(!normalize_mode_plugins(&mut list));
        assert_eq!(list.len(), 2);

        // 幂等：迁移后的列表再次归一化无改动
        let mut list = vec![LEGACY_CHATBOT_MODE_IDS[2].to_string()];
        assert!(normalize_mode_plugins(&mut list));
        assert!(!normalize_mode_plugins(&mut list));
    }
}
