//! Agent plugin host — bridges `echo_plugin` registry to the agent's concrete
//! registries (tools, skills, services) so built-in modules mount as plugins.
//!
//! The Agent owns a [`PluginRegistry`]; the composition root registers
//! built-in plugins (tools/skills/instances/loop/…), and the host implements
//! the narrow sinks plugins use to register their effects. Plugins never
//! import echo-agent; the host is the only instance.

use std::sync::Arc;

use echo_plugin::{
    MountContext, PluginDescriptor, PluginKind, PluginRegistry, PluginSkillSink, PluginToolHandle,
    PluginToolSink,
};

use crate::skill::Skill;
use crate::tool::ToolRegistry;

/// 内置插件 id（组合根与挂/卸效果共用，避免字符串漂移）。
pub const TOOLS_BUILTIN_PLUGIN_ID: &str = "echo-agent.tools.builtin";
pub const SKILLS_DIR_PLUGIN_ID: &str = "echo-agent.skills.dir";
pub const MANAGEMENT_PANEL_PLUGIN_ID: &str = "echo-agent.management.panel";
/// 工作区会话管理插件（workspace）：Panel 侧多会话/多目录管理 + git 状态，
/// 模型侧 `workspace` 工具与激活会话的系统提示注入。
pub const WORKSPACE_PLUGIN_ID: &str = "echo-agent.workspace";
/// Subagent 委派插件 id（包 id 同名：`spawn_subagent` 工具 + subagent 技能随包门控）。
pub const SUBAGENT_PLUGIN_ID: &str = "echo-agent.subagent";
/// 循环模式互斥插件（按 persona 二选一，single 为推导兜底，也是默认）。
///
/// 声明（id / 名称 / 描述 / manifest）已外迁到 [`echo_plugin_loop`]
/// （解耦计划 Phase 3 ⑦）——这里是**兼容别名**，配置与门控读的是同一批
/// 字面量，不另存一份：
/// - `loop.single`：单会话循环（默认）——同一会话内 turn 串行排队，
///   无会话管理 UI（会话卡/全局分组隐藏）、回执分支不可见（后端不发射
///   `ReplyBranch*` 事件，分支照常执行合并）；
/// - `loop.parallel`：并行多会话循环——同一会话可并发分支，
///   会话列表/全局会话/可见回执分支全套。
///   两者 mount 的是同一个 TurnRunner（驱动本体），真实效果是 per-persona
///   白名单推导出的 `LoopMode`（见 `TeamMember::loop_mode`）。
pub use echo_plugin_loop::{
    PARALLEL_PLUGIN_ID as PARALLEL_LOOP_PLUGIN_ID, SINGLE_PLUGIN_ID as SINGLE_LOOP_PLUGIN_ID,
};

/// 受 persona 名单门控的**框架内置**插件 id。
///
/// 实例插件**不在这里**：它们按 `kind == PluginInstance` 在运行期从注册表推导
/// （见 `Agent::gated_plugin_ids`），框架不需要认识任何平台名——新增平台
/// 插件自动纳入门控。
pub const GATED_PLUGIN_IDS: [&str; 4] = [
    TOOLS_BUILTIN_PLUGIN_ID,
    SKILLS_DIR_PLUGIN_ID,
    WORKSPACE_PLUGIN_ID,
    SUBAGENT_PLUGIN_ID,
];

/// LLM Provider 插件 id（名义挂载，重启生效）。
pub const PROVIDER_LLM_PLUGIN_ID: &str = "echo-agent.provider.llm";

/// 框架自身声明的内置插件 id（**不含插件实例**）。
///
/// 实例插件 id 由组合根声明（`core::instance_manifests`，按前缀
/// `echo-agent.instance.<平台名>` 生成）——框架不认识平台名。
/// 与 `scripts/update.sh::expected_ids` 的对齐校验由 `core::instance_manifests`
/// 的单测把两侧并起来比（每项 id 必须出现在 update.sh 的校验清单里）。
pub const BUILTIN_PLUGIN_IDS: [&str; 8] = [
    TOOLS_BUILTIN_PLUGIN_ID,
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
    if profile.enabled_plugins.is_empty() {
        return true;
    }
    let wanted = canonical_plugin_id(plugin_id);
    profile
        .enabled_plugins
        .iter()
        .any(|listed| canonical_plugin_id(listed) == wanted)
}

/// 实例插件 id 的旧前缀（词汇归定义层 `echo_defs::plugin_id`；
/// 此别名保留既有路径）。
pub use echo_defs::plugin_id::LEGACY_INSTANCE_PLUGIN_PREFIX;

/// 实例插件 id 的现役前缀（别名，见 [`LEGACY_INSTANCE_PLUGIN_PREFIX`]）。
pub use echo_defs::plugin_id::INSTANCE_PLUGIN_PREFIX;

/// 把旧实例插件 id 归一为现役形态；其余 id 原样返回。
///
/// 实现见 `echo_defs::plugin_id::canonical_plugin_id`——技能加载器与
/// 组合根都走同一口径，不各自维护一份映射。
pub use echo_defs::plugin_id::canonical_plugin_id;

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

/// 配置化组合（P4）：**一步挂载决定**——某个内置插件要不要挂载。
///
/// 只表达"挂载与否"，不表达"怎么构造"：内置插件的 mount 闭包要捕获
/// 组合根的运行期句柄（端口 / token / 实例句柄 / broker…），构造只能在
/// 代码里发生；能交给配置的是**集合、顺序、启用状态**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountStep {
    /// 内置插件 id（与 `PluginManifest::id` 一致）。
    pub id: String,
    /// 是否挂载（`false` = 注册但保持未挂载，Panel 显示为已禁用）。
    pub enabled: bool,
}

impl MountStep {
    /// 构造一步决定。
    pub fn new(id: impl Into<String>, enabled: bool) -> Self {
        Self {
            id: id.into(),
            enabled,
        }
    }
}

/// 把「内置目录顺序」与「配置给出的挂载计划」解析成**实际挂载序列**。
///
/// 语义（保守优先，任何配置都不会"顺手关掉一堆东西"）：
///
/// 1. 计划里出现的 id **按计划顺序**排在前面，启用状态取计划值；
/// 2. 计划里出现、目录里没有的 id → 诊断（配置写错了），不挂载；
/// 3. 目录里有、计划里**没提到**的 id → 追加在末尾、默认启用。
///
/// 第 3 条是关键：`plugins.toml` 长期只用来登记**子进程**插件（实例插件
/// 等），绝大多数部署里它一个内置 id 都不提。若"计划未提到 = 不挂载"，
/// 这些部署会一夜之间失去全部内置插件（工具集 / 技能 / 管理面…）。所以
/// 缺省即挂载，配置只能**显式**改变某个内置插件的顺序或启用状态。
///
/// 计划为空（无 `plugins.toml`）→ 目录顺序原样返回，与改造前完全一致。
pub fn resolve_mount_order(
    catalog_ids: &[String],
    plan: &[MountStep],
) -> (Vec<MountStep>, Vec<String>) {
    let mut diagnostics = Vec::new();
    let mut ordered: Vec<MountStep> = Vec::with_capacity(catalog_ids.len());
    let mut seen: Vec<&str> = Vec::new();
    for step in plan {
        if !catalog_ids.iter().any(|id| id == &step.id) {
            diagnostics.push(format!(
                "mount plan names unknown builtin plugin `{}` (ignored)",
                step.id
            ));
            continue;
        }
        if seen.contains(&step.id.as_str()) {
            diagnostics.push(format!(
                "mount plan repeats builtin plugin `{}` (first wins)",
                step.id
            ));
            continue;
        }
        seen.push(&step.id);
        ordered.push(step.clone());
    }
    for id in catalog_ids {
        if !seen.contains(&id.as_str()) {
            ordered.push(MountStep::new(id.clone(), true));
        }
    }
    (ordered, diagnostics)
}

/// 子进程插件（`plugins.toml` 里的外部条目）的**生命周期控制面**。
///
/// 为什么是 trait 而不是直接依赖 `echo-plugin-host`：本 crate 是框架层，
/// 刻意不认识监督者（宿主实现细节）；而"停/起一个子进程"又必须由持有
/// supervisor 句柄的组合根来做。trait 把两件事分开——类型在框架层，
/// 实现在组合根。
///
/// 进程内插件不走这条路（它们的启停由注册表 mount/unmount 完成）。
pub trait SubprocessPluginControl: Send + Sync {
    /// 停止（可逆）：排空后停在 `stopped`，之后还能 [`Self::start`]。
    fn stop<'a>(
        &'a self,
        id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;

    /// 从停止态重新拉起（重新握手）。
    fn start<'a>(
        &'a self,
        id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;

    /// 当前运行态快照（None = 不认识这个 id）。
    fn status(&self, id: &str) -> Option<SubprocessStatus>;
}

/// 子进程插件运行态快照（面板展示 + 决策）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubprocessStatus {
    /// `pending` / `ready` / `draining` / `stopped` / `failed`。
    pub state: String,
    /// 崩溃后自动重启次数。
    pub restarts: u32,
    /// 连接代际（每次就绪自增）。
    pub generation: u64,
    /// `state == failed` 时的原因。
    pub error: Option<String>,
}

/// The agent's plugin host: registry + descriptors + runtime toggling.
pub struct PluginHost {
    pub registry: PluginRegistry,
    /// Mount context provided to every plugin at mount time.
    mount_ctx: parking_lot::Mutex<Option<MountContext>>,
    /// 子进程插件生命周期（组合根注入；None = 无外部条目或未接线）。
    subprocess: parking_lot::RwLock<Option<Arc<dyn SubprocessPluginControl>>>,
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
            mount_ctx: parking_lot::Mutex::new(None),
            subprocess: parking_lot::RwLock::new(None),
        }
    }

    /// Attach the mount context (tools/skills/services). Call once at boot
    /// before mounting anything.
    pub fn set_mount_ctx(&self, ctx: MountContext) {
        *self.mount_ctx.lock() = Some(ctx);
    }

    fn ctx(&self) -> MountContext {
        self.mount_ctx.lock().clone().unwrap_or_default()
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

    /// 配置化组合（P4）：按**挂载计划**注册 + 挂载一批内置插件。
    ///
    /// 与逐个 [`Self::register_and_mount`] 的差别只有两点：顺序由计划决定，
    /// 计划里 `enabled = false` 的条目**不挂载**（但照样注册——Panel 的插件
    /// 清单要能看到它、并能就地启用）。已持久化的禁用状态（`core.toml` 的
    /// `disabled_plugins`）在这里再收敛一次：两者任一为禁用即不挂载。
    ///
    /// 返回 `(已注册数, 已挂载 id, 错误)`：单个挂载失败不影响其余（启动
    /// 期"尽力而为"语义，与既有 `mount_all` 一致）；注册冲突（同 id 重复）
    /// 是装配 bug，记进错误但不中断。
    pub fn mount_plan(
        &self,
        catalog: Vec<Arc<dyn echo_plugin::Plugin>>,
        plan: &[MountStep],
    ) -> (usize, Vec<String>, Vec<String>) {
        let mut errors: Vec<String> = Vec::new();
        // 先全部注册：Panel 清单的完整性不依赖配置写了哪些行。
        let mut by_id: Vec<(String, Arc<dyn echo_plugin::Plugin>)> = Vec::new();
        for plugin in catalog {
            let id = plugin.manifest().id.clone();
            if let Err(e) = self.registry.register(Arc::clone(&plugin)) {
                errors.push(e);
                continue;
            }
            by_id.push((id, plugin));
        }
        let registered = by_id.len();
        // 计划禁用 → 收敛进注册表的启用状态（启动期一次性，不写回配置）。
        let disabled: Vec<String> = plan
            .iter()
            .filter(|step| !step.enabled)
            .map(|step| step.id.clone())
            .collect();
        if !disabled.is_empty() {
            self.registry.apply_disabled(&disabled);
        }
        let ctx = self.ctx();
        let mut mounted = Vec::new();
        for step in plan {
            if !by_id.iter().any(|(id, _)| id == &step.id) {
                continue; // 未注册（构造失败或计划里是未知 id）
            }
            if !self.registry.is_enabled(&step.id) {
                continue;
            }
            match self.registry.mount(&step.id, &ctx) {
                Ok(_) => mounted.push(step.id.clone()),
                Err(e) => errors.push(format!("plugin {} mount failed: {e}", step.id)),
            }
        }
        (registered, mounted, errors)
    }

    /// 注入子进程插件控制面（组合根装配期，一次）。
    pub fn set_subprocess_control(&self, control: Arc<dyn SubprocessPluginControl>) {
        *self.subprocess.write() = Some(control);
    }

    /// 子进程插件控制面（None = 未接线 / 无外部条目）。
    pub fn subprocess_control(&self) -> Option<Arc<dyn SubprocessPluginControl>> {
        self.subprocess.read().clone()
    }

    /// 某个插件是不是**外部条目**（`plugins.toml` 的 stdio 条目）。
    ///
    /// 判据：`runtime == subprocess && !builtin`——实例门控壳是
    /// `subprocess + builtin`（manifest 编译在 Core 里），不属于这一类。
    pub fn is_external_entry(&self, id: &str) -> bool {
        self.registry
            .descriptors()
            .into_iter()
            .find(|d| d.id == id)
            .is_some_and(|d| d.runtime == "subprocess" && !d.builtin)
    }

    /// 外部条目的运行态（None = 不是外部条目 / 未接线）。
    pub fn subprocess_status(&self, id: &str) -> Option<SubprocessStatus> {
        self.subprocess_control()?.status(id)
    }

    /// 停止一个外部条目（可逆；进程内插件请走 [`Self::set_enabled`]）。
    pub async fn stop_subprocess(&self, id: &str) -> Result<(), String> {
        let control = self
            .subprocess_control()
            .ok_or_else(|| format!("{id}: 子进程控制面未接线"))?;
        control.stop(id).await
    }

    /// 拉起一个已停止的外部条目。
    pub async fn start_subprocess(&self, id: &str) -> Result<(), String> {
        let control = self
            .subprocess_control()
            .ok_or_else(|| format!("{id}: 子进程控制面未接线"))?;
        control.start(id).await
    }

    /// Descriptors for the Panel（**注册序** = 组合 / 配置行序）。
    ///
    /// 不按 id 排序：`plugins.toml` 的行序就是挂载顺序，是配置表达的一部分
    /// （面板按返回序渲染，用户看到的顺序与自己写的顺序一致）。
    /// 子进程插件的运行态在这里拼进去（`state` / `restarts` / `generation`），
    /// 因为注册表本身不持有进程句柄。
    pub fn descriptors(&self) -> Vec<PluginDescriptor> {
        let mut ds = self.registry.descriptors();
        if let Some(control) = self.subprocess_control() {
            for d in &mut ds {
                if let Some(status) = control.status(&d.id) {
                    d.state = status.state;
                    d.restarts = status.restarts;
                    d.generation = status.generation;
                    // 运行态失败但没别的错误来源时，用它补上（面板的「失败」
                    // 徽标读 error，只有 state 没有 error 会看不出所以然）。
                    if d.error.is_none() {
                        d.error = status.error;
                    }
                }
            }
        }
        ds
    }

    /// Runtime toggle; persists via the configured `disabled_plugins` hook.
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        self.registry.set_enabled(id, enabled, &self.ctx())
    }

    /// 重载一个插件（卸下 → 重新挂载）；**不改**启用状态。
    ///
    /// 语义与 `set_enabled(false)` + `set_enabled(true)` 等价，但不写
    /// `disabled_plugins`、不触发持久化——面板的「重启」按钮走这条，
    /// 免掉"重启一次就在配置里留一条禁用记录"的副作用。
    pub fn restart(&self, id: &str) -> Result<(), String> {
        self.registry.restart(id, &self.ctx())
    }

    /// 把一批插件标记为禁用（**只改注册表状态**，不写配置、不持久化）。
    ///
    /// 用途：`plugins.toml` 里 `enabled = false` 的外部条目——配置选择让它
    /// 不启动，清单里应显示为「已禁用」而不是消失（消失会让人以为配置写错了）。
    pub fn disable_plugins(&self, ids: &[String]) {
        if !ids.is_empty() {
            self.registry.apply_disabled(ids);
        }
    }

    /// 记录插件装配/启动失败原因（组合根在拉起子进程插件后写入）。
    pub fn set_plugin_errors<I, K, V>(&self, errors: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (id, error) in errors {
            self.registry.set_plugin_error(id, error);
        }
    }

    /// 记录插件的配置来源层（组合根装载 `plugins.toml` 后写入）。
    pub fn set_config_origins<I, K, V>(&self, origins: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.registry.set_config_origins(origins);
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

    // ── 配置化组合：挂载计划解析 ─────────────────────────────────────────

    fn catalog() -> Vec<String> {
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    }

    /// 空计划（无 plugins.toml）→ 目录顺序原样、全部启用：与改造前逐字一致。
    #[test]
    fn empty_plan_keeps_catalog_order_and_enables_everything() {
        let (order, diagnostics) = resolve_mount_order(&catalog(), &[]);
        assert!(diagnostics.is_empty());
        assert_eq!(
            order,
            vec![
                MountStep::new("a", true),
                MountStep::new("b", true),
                MountStep::new("c", true)
            ]
        );
    }

    /// **关键安全语义**：计划只提到一个内置 id 时，其余内置仍要挂载。
    ///
    /// 现网 `plugins.toml` 多半只登记子进程插件、一个内置 id 都不提；
    /// 若"未提到 = 不挂载"，这些部署会一夜之间失去工具集/技能/管理面。
    #[test]
    fn unmentioned_builtins_stay_mounted_after_named_ones() {
        let (order, diagnostics) = resolve_mount_order(&catalog(), &[MountStep::new("c", false)]);
        assert!(diagnostics.is_empty());
        assert_eq!(
            order,
            vec![
                MountStep::new("c", false), // 计划里提到的排前面（并禁用）
                MountStep::new("a", true),
                MountStep::new("b", true)
            ]
        );
    }

    /// 计划可重排：顺序决定挂载先后（配置化组合的表达力所在）。
    #[test]
    fn plan_reorders_mounting() {
        let (order, _) = resolve_mount_order(
            &catalog(),
            &[MountStep::new("b", true), MountStep::new("a", true)],
        );
        assert_eq!(
            order,
            vec![
                MountStep::new("b", true),
                MountStep::new("a", true),
                MountStep::new("c", true)
            ]
        );
    }

    /// 写错 id（拼错 / 已下线）→ 诊断而不是静默忽略；重复 → 首个生效。
    #[test]
    fn unknown_and_duplicate_ids_are_reported() {
        let (order, diagnostics) = resolve_mount_order(
            &catalog(),
            &[
                MountStep::new("ghost", true),
                MountStep::new("b", false),
                MountStep::new("b", true),
            ],
        );
        assert!(
            diagnostics.iter().any(|d| d.contains("ghost")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics.iter().any(|d| d.contains("repeats")),
            "{diagnostics:?}"
        );
        // 首个 b（disabled）生效，后一个不覆盖它——重复不制造第二种解释。
        assert_eq!(order[0], MountStep::new("b", false));
        assert_eq!(order.len(), 3, "unknown id must not be inserted: {order:?}");
    }

    // ── 子进程插件控制面（外部条目）────────────────────────────────────

    /// 测试用控制面：记录调用、按需返回状态。
    struct MockControl {
        calls: parking_lot::Mutex<Vec<String>>,
        state: parking_lot::Mutex<Option<SubprocessStatus>>,
        fail: bool,
    }

    impl MockControl {
        fn new(state: Option<SubprocessStatus>, fail: bool) -> Arc<Self> {
            Arc::new(Self {
                calls: parking_lot::Mutex::new(Vec::new()),
                state: parking_lot::Mutex::new(state),
                fail,
            })
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().clone()
        }
    }

    impl SubprocessPluginControl for MockControl {
        fn stop<'a>(
            &'a self,
            id: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>
        {
            self.calls.lock().push(format!("stop:{id}"));
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err("stop failed".to_string())
                } else {
                    Ok(())
                }
            })
        }

        fn start<'a>(
            &'a self,
            id: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>
        {
            self.calls.lock().push(format!("start:{id}"));
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err("start failed".to_string())
                } else {
                    Ok(())
                }
            })
        }

        fn status(&self, _id: &str) -> Option<SubprocessStatus> {
            self.state.lock().clone()
        }
    }

    fn register_plugin(
        host: &PluginHost,
        id: &str,
        builtin: bool,
        runtime: echo_plugin::PluginRuntime,
    ) {
        host.registry
            .register(Arc::new(echo_plugin::BuiltinPlugin::new(
                echo_plugin::PluginManifest::builtin(
                    id,
                    id,
                    "1",
                    echo_plugin::PluginKind::Tool,
                    id,
                    "",
                )
                .with_runtime(runtime)
                .with_builtin_flag(builtin),
                |_| Ok(vec![]),
            )))
            .expect("register");
    }

    /// 外部条目判定：`subprocess + !builtin`——实例门控壳
    /// （`subprocess + builtin`）**不算**，它的生命周期归实例插件。
    #[test]
    fn external_entry_is_subprocess_and_not_builtin() {
        let host = PluginHost::new();
        register_plugin(
            &host,
            "ext.entry",
            false,
            echo_plugin::PluginRuntime::Subprocess,
        );
        register_plugin(
            &host,
            "instance.shell",
            true,
            echo_plugin::PluginRuntime::Subprocess,
        );
        register_plugin(
            &host,
            "inproc.builtin",
            true,
            echo_plugin::PluginRuntime::InProcess,
        );
        assert!(host.is_external_entry("ext.entry"));
        assert!(!host.is_external_entry("instance.shell"));
        assert!(!host.is_external_entry("inproc.builtin"));
        // 未知 id 不 panic、不算外部条目。
        assert!(!host.is_external_entry("ghost"));
    }

    /// `descriptors()` 把运行态拼进去（注册表不持有进程句柄）。
    #[test]
    fn descriptors_merge_subprocess_state() {
        let host = PluginHost::new();
        register_plugin(
            &host,
            "ext.entry",
            false,
            echo_plugin::PluginRuntime::Subprocess,
        );
        // 未接线：state 为空（进程内语义），不出错。
        assert_eq!(host.descriptors()[0].state, "");

        host.set_subprocess_control(MockControl::new(
            Some(SubprocessStatus {
                state: "failed".into(),
                restarts: 3,
                generation: 7,
                error: Some("crash loop".into()),
            }),
            false,
        ));
        let d = &host.descriptors()[0];
        assert_eq!(d.state, "failed");
        assert_eq!(d.restarts, 3);
        assert_eq!(d.generation, 7);
        // 运行态失败原因补进 error（面板「失败」徽标读它）。
        assert_eq!(d.error.as_deref(), Some("crash loop"));
    }

    /// stop / start 转发到控制面；未接线时给可读错误而不是静默 no-op。
    #[tokio::test]
    async fn subprocess_stop_and_start_forward_to_control() {
        let host = PluginHost::new();
        register_plugin(
            &host,
            "ext.entry",
            false,
            echo_plugin::PluginRuntime::Subprocess,
        );
        // 未接线：明确报错（不是"成功但什么都没做"）。
        let err = host
            .stop_subprocess("ext.entry")
            .await
            .expect_err("no control");
        assert!(err.contains("未接线"), "{err}");

        let control = MockControl::new(None, false);
        host.set_subprocess_control(control.clone());
        host.stop_subprocess("ext.entry").await.expect("stop ok");
        host.start_subprocess("ext.entry").await.expect("start ok");
        assert_eq!(control.calls(), vec!["stop:ext.entry", "start:ext.entry"]);
    }

    /// 注册序即输出序（不按 id 重排）：`plugins.toml` 的行序是配置表达。
    #[test]
    fn descriptor_order_follows_registration() {
        let host = PluginHost::new();
        for id in ["zzz", "aaa", "mmm"] {
            register_plugin(&host, id, true, echo_plugin::PluginRuntime::InProcess);
        }
        let ids: Vec<String> = host.descriptors().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["zzz", "aaa", "mmm"]);
    }

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
