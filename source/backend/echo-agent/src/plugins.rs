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
/// 工作区会话管理插件（workspace）：Panel 侧多会话/多目录管理 + git 状态，
/// 模型侧 `workspace` 工具与激活会话的系统提示注入。
pub const WORKSPACE_PLUGIN_ID: &str = "echo-agent.workspace";
/// 旧选单插件 id（menu 已降级为普通编排工具，插件维度移除）：
/// 配置加载时从白名单剔除（见 `normalize_mode_plugins`）。
pub const LEGACY_MENU_PLUGIN_ID: &str = "echo-agent.menu";

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
/// orchestration.single → `loop.single`，loop.runner → 剔除（模式插件取代）；
/// echo-agent.menu → 剔除（选单降级为普通编排工具，插件维度移除）。
/// 去重、保序。返回是否有改动。
pub fn normalize_mode_plugins(list: &mut Vec<String>) -> bool {
    let mut changed = false;
    let mut normalized: Vec<String> = Vec::with_capacity(list.len());
    for item in list.iter() {
        let is_parallel = LEGACY_CHATBOT_MODE_IDS.contains(&item.as_str());
        let is_single = item == LEGACY_SINGLE_MODE_ID;
        let is_legacy_runner = item == LEGACY_LOOP_RUNNER_PLUGIN_ID;
        let is_legacy_menu = item == LEGACY_MENU_PLUGIN_ID;
        if !is_parallel && !is_single && !is_legacy_runner && !is_legacy_menu {
            normalized.push(item.clone());
            continue;
        }
        changed = true;
        let replacement = if is_parallel {
            Some(PARALLEL_LOOP_PLUGIN_ID)
        } else if is_single {
            Some(SINGLE_LOOP_PLUGIN_ID)
        } else {
            // 旧驱动插件 id / 旧选单插件 id：已被取代或移除，配置里不再保留。
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

pub const GATED_PLUGIN_IDS: [&str; 5] = [
    TOOLS_BUILTIN_PLUGIN_ID,
    SKILLS_DIR_PLUGIN_ID,
    CHECKLIST_PLUGIN_ID,
    ADAPTER_QQ_PLUGIN_ID,
    WORKSPACE_PLUGIN_ID,
];

/// 全部内置插件 id（与 `scripts/update.sh` 的插件校验清单一致）。
/// 供插件黑名单移除迁移物化白名单时使用。
pub const BUILTIN_PLUGIN_IDS: [&str; 10] = [
    TOOLS_BUILTIN_PLUGIN_ID,
    ADAPTER_QQ_PLUGIN_ID,
    SKILLS_DIR_PLUGIN_ID,
    CHECKLIST_PLUGIN_ID,
    WORKSPACE_PLUGIN_ID,
    MANAGEMENT_PANEL_PLUGIN_ID,
    SINGLE_LOOP_PLUGIN_ID,
    PARALLEL_LOOP_PLUGIN_ID,
    "echo-agent.orchestration",
    "echo-agent.provider.llm",
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

/// 插件黑名单移除迁移（2026-09-11）：把 `disabled_plugins` 语义物化进
/// `enabled_plugins` 白名单，使既有配置的门控行为不变——
/// - 黑名单为空：no-op（返回 false）
/// - 白名单为空（历史语义 = 全部启用）：物化为「全部内置插件 − 黑名单 −
///   parallel 模式 id」（空白名单历史推导单会话，物化不得翻成并行）
/// - 白名单非空：剔除黑名单项；黑名单含 parallel id 时同时剔除白名单里的
///   parallel id（历史上黑名单优先，避免物化后循环模式翻成并行）
///
/// 返回是否有改动。仅由加载期迁移调用（`migrate_orchestration_mode_plugins`）。
pub fn convert_plugin_blacklist_to_whitelist(
    enabled: &mut Vec<String>,
    disabled: &[String],
) -> bool {
    if disabled.is_empty() {
        return false;
    }
    let disabled_parallel = disabled
        .iter()
        .any(|d| PARALLEL_MODE_IDS.iter().any(|id| id == d));
    let mut materialized: Vec<String> = if enabled.is_empty() {
        BUILTIN_PLUGIN_IDS
            .iter()
            .filter(|id| !PARALLEL_MODE_IDS.contains(*id))
            .map(|id| (*id).to_string())
            .collect()
    } else {
        std::mem::take(enabled)
    };
    materialized.retain(|id| !disabled.iter().any(|d| d == id));
    if disabled_parallel {
        materialized.retain(|id| !PARALLEL_MODE_IDS.contains(&id.as_str()));
    }
    // 去重保序（白名单语义按集合，去重避免重复项污染配置）
    let mut seen = std::collections::HashSet::new();
    materialized.retain(|id| seen.insert(id.clone()));
    *enabled = materialized;
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
    fn normalize_mode_plugins_drops_legacy_menu() {
        // 旧选单插件 id 被剔除（选单降级为普通编排工具），其余保序
        let mut list = vec![
            LEGACY_MENU_PLUGIN_ID.to_string(),
            "echo-agent.tools.builtin".to_string(),
        ];
        assert!(normalize_mode_plugins(&mut list));
        assert_eq!(list, vec!["echo-agent.tools.builtin".to_string()]);
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

    // ── 插件黑名单移除迁移（黑名单 → 白名单物化）──

    #[test]
    fn blacklist_conversion_materializes_all_plugins_minus_blacklisted() {
        // 空白名单 + 黑名单两项（base 人格的真实配置形态）→ 物化全部内置
        // 插件减去黑名单项与 parallel 模式 id（保持单会话推导）
        let mut enabled: Vec<String> = Vec::new();
        let disabled = vec![
            "echo-agent.tools.builtin".to_string(),
            "echo-agent.skills.dir".to_string(),
        ];
        assert!(convert_plugin_blacklist_to_whitelist(
            &mut enabled,
            &disabled
        ));
        assert!(!enabled.iter().any(|id| id == "echo-agent.tools.builtin"));
        assert!(!enabled.iter().any(|id| id == "echo-agent.skills.dir"));
        assert!(enabled.iter().any(|id| id == "echo-agent.checklist"));
        assert!(enabled.iter().any(|id| id == "echo-agent.adapter.qq"));
        // 物化不得引入 parallel 模式 id（空白名单历史推导单会话）
        assert!(!enabled
            .iter()
            .any(|id| PARALLEL_MODE_IDS.iter().any(|p| p == id)));
    }

    #[test]
    fn blacklist_conversion_refines_existing_whitelist() {
        // 白名单非空：只剔除黑名单项
        let mut enabled = vec![
            "echo-agent.tools.builtin".to_string(),
            "echo-agent.adapter.qq".to_string(),
        ];
        assert!(convert_plugin_blacklist_to_whitelist(
            &mut enabled,
            &["echo-agent.tools.builtin".to_string()]
        ));
        assert_eq!(enabled, vec!["echo-agent.adapter.qq".to_string()]);
    }

    #[test]
    fn blacklist_conversion_keeps_single_mode_when_parallel_was_denied() {
        // 历史上黑名单优先：白名单含 parallel + 黑名单含 parallel → 单会话；
        // 物化后白名单不得再含 parallel（循环模式推导只看白名单）
        let mut enabled = vec![
            PARALLEL_LOOP_PLUGIN_ID.to_string(),
            "echo-agent.tools.builtin".to_string(),
        ];
        assert!(convert_plugin_blacklist_to_whitelist(
            &mut enabled,
            &[PARALLEL_LOOP_PLUGIN_ID.to_string()]
        ));
        assert!(!enabled.iter().any(|id| id == PARALLEL_LOOP_PLUGIN_ID));
        assert!(enabled.iter().any(|id| id == "echo-agent.tools.builtin"));
        // 白名单含 parallel、黑名单不含 → 保持并行
        let mut enabled = vec![PARALLEL_LOOP_PLUGIN_ID.to_string()];
        assert!(convert_plugin_blacklist_to_whitelist(
            &mut enabled,
            &["echo-agent.adapter.qq".to_string()]
        ));
        assert_eq!(enabled, vec![PARALLEL_LOOP_PLUGIN_ID.to_string()]);
    }

    #[test]
    fn blacklist_conversion_is_noop_without_blacklist() {
        let mut enabled: Vec<String> = Vec::new();
        assert!(!convert_plugin_blacklist_to_whitelist(&mut enabled, &[]));
        assert!(enabled.is_empty());
    }

    #[test]
    fn profile_allows_plugin_is_whitelist_only() {
        use crate::config::TeamMember;
        // 空白名单 = 全部允许（黑名单字段即使有值也不再参与判定）
        let member = TeamMember {
            disabled_plugins: vec![TOOLS_BUILTIN_PLUGIN_ID.to_string()],
            ..Default::default()
        };
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
