//! Agent plugin host — bridges `echo_plugin` registry to the agent's concrete
//! registries (tools, skills, services) so built-in modules mount as plugins.
//!
//! The Agent owns a [`PluginRegistry`]; the composition root registers
//! built-in plugins (tools/skills/instances/loop/…) and supplies the effects
//! they need at mount time. Plugins never import echo-agent; the host is the
//! only instance.
//!
//! 历史（2026-10 M2）：本文件曾提供 `ToolSink` / `SkillSink` 两个"窄接缝"
//! 给插件注册工具与技能。审计发现它们是死代码（`register_tool` 零调用、
//! `SkillSink` 从未实例化、`register_skill_dir` 无调用方），而两个 trait
//! 把应用领域词汇带进了框架层。随 `MountContext.tools/.skills` 一并删除。

use std::sync::Arc;

use echo_plugin::{MountContext, PluginDescriptor, PluginRegistry};

use crate::skill::Skill;

/// 内置插件 id（组合根与挂/卸效果共用，避免字符串漂移）。
/// 实例类插件的 `kind` 标签。
///
/// 框架不再有 `PluginKind` 枚举（M2 去业务化），词汇表归宿主所有：这里是
/// 「哪些插件按实例插件处理」的**唯一真值**，别处一律引用本常量。
///
/// 为什么值得一个常量：`2d46b7d` 把 wire 名从 `adapter` 改成 `instance` 时，
/// 硬编码比较的地方漏改，导致实例插件永远进不了 persona 门控集合。
pub const INSTANCE_PLUGIN_KIND: &str = "instance";

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
    /// `reconcile` 重入保护：依赖变更回调里挂载插件会再注册服务、再触发
    /// 回调——不加这道闸就是无限递归。
    reconciling: std::sync::atomic::AtomicBool,
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
            reconciling: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Attach the mount context (tools/skills/services). Call once at boot
    /// before mounting anything.
    pub fn set_mount_ctx(&self, ctx: MountContext) {
        *self.mount_ctx.lock() = Some(ctx);
    }

    /// 挂载上下文（测试与 [`Self::reconcile`] 共用；不是公开 API）。
    pub(crate) fn ctx(&self) -> MountContext {
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

    /// 配置化组合的**就绪等待**版本：注册 + 按计划挂载，每个插件都先等它的
    /// `inject` 就绪（最多 `timeout`；见 `echo_plugin::PluginRegistry::
    /// mount_when_ready`）。
    ///
    /// 与 [`Self::mount_plan`] 的唯一差别是"依赖暂时没到"的处理：`mount_plan`
    /// 立即失败（启动即报错），这里等一等再判。对现网无行为差异——正常启动
    /// 时依赖早已注册，第一次检查就命中，一次 `contains` 的开销。
    ///
    /// 返回 `(已注册数, 已挂载 id, 仍在等依赖的 (id, 缺的键), 错误)`。
    /// 「仍在等」不算失败：它们留在 `pending` 里，依赖到了由
    /// [`Self::reconcile`] 挂上。
    pub async fn mount_plan_when_ready(
        &self,
        catalog: Vec<Arc<dyn echo_plugin::Plugin>>,
        plan: &[MountStep],
        timeout: std::time::Duration,
    ) -> (usize, Vec<String>, Vec<(String, Vec<String>)>, Vec<String>) {
        let mut errors: Vec<String> = Vec::new();
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
        let mut waiting = Vec::new();
        for step in plan {
            if !by_id.iter().any(|(id, _)| id == &step.id) {
                continue;
            }
            if !self.registry.is_enabled(&step.id) {
                continue;
            }
            match self
                .registry
                .mount_when_ready(&step.id, &ctx, timeout)
                .await
            {
                Ok(_) => mounted.push(step.id.clone()),
                Err(echo_plugin::PluginMountError::MissingService { key, .. }) => {
                    // 依赖没到：**不算错误**，进 pending 等依赖变更。
                    waiting.push((step.id.clone(), vec![key]));
                }
                Err(e) => errors.push(format!("plugin {} mount failed: {e}", step.id)),
            }
        }
        (registered, mounted, waiting, errors)
    }

    /// 依赖变更后重新核对注册表（`inject` 的响应端）。
    ///
    /// 语义见 `echo_plugin::PluginRegistry::reconcile`：pending 里依赖已齐的
    /// 挂上、已挂载但依赖消失的卸下。**重入保护**：挂载动作会注册服务、再触发
    /// 变更通知，没有这道闸就是递归。
    ///
    /// 组合根可以在 `ctx.on_change` 回调里直接调它——这是"声明式注入"真正
    /// 生效的地方（不是启动期检查一次，而是依赖变化时自动追平）。
    pub fn reconcile(&self) -> echo_plugin::ReconcileReport {
        use std::sync::atomic::Ordering;
        if self.reconciling.swap(true, Ordering::SeqCst) {
            return echo_plugin::ReconcileReport::default();
        }
        struct Reset<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _reset = Reset(&self.reconciling);
        self.registry.reconcile(&self.ctx())
    }

    /// 注入**插件事件出口**（组合根装配期一次）。
    ///
    /// 插件 `emit` 的事件（含注册表自己发的 `plugin.mounted` /
    /// `plugin.unmounted`）经它出去；组合根把它接到进程级事件汇聚点，
    /// 于是前端能看到"插件在干什么"。未注入 = 静默丢弃（降级不是错误）。
    pub fn set_event_sink(&self, sink: echo_plugin::EventSink) {
        self.registry.set_event_sink(sink);
    }

    /// 把一条事件**投递给订阅了它的已挂载插件**（返回成功投递数）。
    ///
    /// 谁来调：宿主在广播领域事件时顺手投一份（本轮**未接线**——目前没有
    /// 订阅方；接线点是 `Agent::emit` 的汇聚处，等第一个真实订阅者出现再做，
    /// 免得变成"每条事件都空转一圈"）。
    pub fn deliver_plugin_event(&self, event: &echo_plugin::PluginEvent) -> usize {
        self.registry.deliver_event(event)
    }

    /// 眼下还缺依赖的插件（`id → 缺的键`）。
    pub fn pending_inject(&self) -> Vec<(String, Vec<String>)> {
        self.registry.pending_inject()
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

    /// 记录「定义从哪来 / 怎么启动」（组合根装配期，面板技术信息用）。
    pub fn set_definitions<I, K>(&self, definitions: I)
    where
        I: IntoIterator<Item = (K, echo_plugin::PluginDefinition)>,
        K: Into<String>,
    {
        for (id, definition) in definitions {
            self.registry.set_definition(id, definition);
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

    /// 下发插件的配置（组合根装载 `plugins.toml` 后写入）。
    ///
    /// 这是**内置插件拿到自己那份配置的唯一通路**：`plugins.toml` 里
    /// `kind = "builtin"` 行的 `[plugin.config]` 经这里进入注册表，`mount`
    /// 时按 id 注入 [`echo_plugin::MountContext::config`]。插件因此不必自己去
    /// 找 `plugins.toml`、解析分层合成结果，也不会读到别人的配置。
    ///
    /// 时序：`set_configs` 必须在挂载**之前**（启动期）或重新启用**之前**
    /// （运行期）调用——`mount` 取的是当时登记的值。
    pub fn set_configs<I, K>(&self, configs: I)
    where
        I: IntoIterator<Item = (K, serde_json::Value)>,
        K: Into<String>,
    {
        self.registry.set_configs(configs);
    }

    /// 按 `kind` 标签筛选插件快照。
    ///
    /// `kind` 现在是**宿主自定义的开放标签**（框架不再有枚举），因此入参是
    /// `&str`：调用方用本 crate 自己的常量，框架不设词汇表。
    pub fn plugins_of_kind(&self, kind: &str) -> Vec<PluginDescriptor> {
        self.descriptors()
            .into_iter()
            .filter(|d| d.kind == kind)
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
                echo_plugin::PluginManifest::builtin(id, id, "1", "tool", id, "")
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

    // ── 声明式注入：就绪等待 + 依赖变更追平（plugin-framework v0.6）──────

    fn inject_plugin(
        id: &str,
        service: &str,
        mounted: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Arc<dyn echo_plugin::Plugin> {
        use std::sync::atomic::Ordering;
        Arc::new(
            echo_plugin::BuiltinPlugin::new(
                echo_plugin::PluginManifest::builtin(id, id, "1", "tool", "", ""),
                move |_| {
                    mounted.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![])
                },
            )
            .with_inject(echo_plugin::Inject::new([service])),
        )
    }

    /// 启动期：依赖没到 → 插件进 pending（**不算失败**），依赖到了由一个
    /// `reconcile` 挂上。这是"声明式注入"在宿主侧的完整闭环。
    #[tokio::test]
    async fn pending_inject_mounts_once_the_service_arrives() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let host = PluginHost::new();
        let ctx = std::sync::Arc::new(echo_context::Ctx::default());
        host.set_mount_ctx(echo_plugin::MountContext::new().with_ctx(ctx.clone()));

        let mounted = Arc::new(AtomicUsize::new(0));
        let catalog: Vec<Arc<dyn echo_plugin::Plugin>> =
            vec![inject_plugin("i.loop", "loop", mounted.clone())];

        let (_, up, waiting, errors) = host
            .mount_plan_when_ready(
                catalog,
                &[MountStep::new("i.loop", true)],
                std::time::Duration::ZERO,
            )
            .await;
        assert!(up.is_empty());
        assert!(errors.is_empty(), "缺依赖不该算错误: {errors:?}");
        assert_eq!(
            waiting,
            vec![("i.loop".to_string(), vec!["loop".to_string()])]
        );
        assert_eq!(mounted.load(Ordering::SeqCst), 0);
        assert_eq!(
            host.pending_inject(),
            vec![("i.loop".to_string(), vec!["loop".to_string()])]
        );

        // 服务到位 → reconcile 追平。
        let keep = ctx.register::<u8>("loop", 1u8);
        let report = host.reconcile();
        assert_eq!(report.mounted, vec!["i.loop".to_string()]);
        assert_eq!(mounted.load(Ordering::SeqCst), 1);
        assert!(host.pending_inject().is_empty());

        // 服务消失 → reconcile 卸下。
        keep.dispose();
        let report = host.reconcile();
        assert_eq!(report.unmounted, vec!["i.loop".to_string()]);
        assert_eq!(
            host.pending_inject(),
            vec![("i.loop".to_string(), vec!["loop".to_string()])]
        );
    }

    /// `reconcile` 的重入保护：在服务变更回调里调用它（真实用法）不能递归。
    #[test]
    fn reconcile_is_reentrancy_safe_from_a_change_callback() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let host = Arc::new(PluginHost::new());
        let ctx = Arc::new(echo_context::Ctx::default());
        host.set_mount_ctx(echo_plugin::MountContext::new().with_ctx(ctx.clone()));

        let mounted = Arc::new(AtomicUsize::new(0));
        host.registry
            .register(inject_plugin("i.cb", "svc", mounted.clone()))
            .unwrap();
        // 首次挂载失败 → pending。
        assert!(host.registry.mount("i.cb", &host.ctx()).is_err());

        // 订阅在回调里 reconcile（与 main.rs 的接法一致）。
        let host_for_cb = host.clone();
        let _sub = ctx.on_change(move |_| {
            host_for_cb.reconcile();
        });

        // 注册服务触发回调 → 回调里 reconcile → 插件挂上；挂载过程里
        // 若再触发回调，重入位会让内层直接返回（否则这里会栈溢出）。
        let _keep = ctx.register::<u8>("svc", 1u8);
        assert_eq!(mounted.load(Ordering::SeqCst), 1);
        assert!(host.pending_inject().is_empty());

        // 幂等：再核一次不动。
        assert!(host.reconcile().is_noop());
    }

    // ── 插件事件通道（plugin-framework v0.7）─────────────────────────────

    /// 宿主注入出口后：插件 `emit` 的事件带自己的 id 到达宿主；
    /// 注册表自己发的 lifecycle 事件（`plugin.mounted`）也走同一个出口。
    #[test]
    fn plugin_events_reach_the_host_sink_with_ids() {
        use std::sync::Mutex;
        let host = PluginHost::new();
        let seen: Arc<Mutex<Vec<echo_plugin::PluginEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        host.set_event_sink(Arc::new(move |event| sink_seen.lock().unwrap().push(event)));
        host.set_mount_ctx(echo_plugin::MountContext::new());
        host.register_and_mount(Arc::new(echo_plugin::BuiltinPlugin::new(
            echo_plugin::PluginManifest::builtin("ev.p", "p", "1", "tool", "", ""),
            |mctx: &echo_plugin::MountContext| {
                mctx.emit("tool.done", serde_json::json!({"ok": true}));
                Ok(vec![])
            },
        )))
        .expect("mount ok");

        let events = seen.lock().unwrap().clone();
        let mounted = events
            .iter()
            .find(|e| e.name == "plugin.mounted")
            .expect("lifecycle 事件应到达宿主");
        assert_eq!(mounted.payload["id"], "ev.p");
        let emitted = events
            .iter()
            .find(|e| e.name == "tool.done")
            .expect("插件 emit 应到达宿主");
        assert_eq!(emitted.plugin, "ev.p", "出口自动填来源 id");
        assert_eq!(emitted.payload["ok"], true);
    }

    /// 未注入出口 = 静默丢弃：挂载照常成功（降级不是错误）。
    #[test]
    fn missing_event_sink_does_not_break_mounting() {
        let host = PluginHost::new();
        host.set_mount_ctx(echo_plugin::MountContext::new());
        host.register_and_mount(Arc::new(echo_plugin::BuiltinPlugin::new(
            echo_plugin::PluginManifest::builtin("ev.bare", "b", "1", "tool", "", ""),
            |mctx: &echo_plugin::MountContext| {
                mctx.emit("nobody.listens", serde_json::Value::Null);
                Ok(vec![])
            },
        )))
        .expect("没有出口也不该失败");
        assert_eq!(host.descriptors().len(), 1);
    }

    /// 宿主 → 插件的投递：只有订阅了的已挂载插件收到。
    #[test]
    fn host_delivers_events_only_to_subscribers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let host = PluginHost::new();
        host.set_mount_ctx(echo_plugin::MountContext::new());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        host.register_and_mount(Arc::new(
            echo_plugin::BuiltinPlugin::new(
                echo_plugin::PluginManifest::builtin("ev.sub", "s", "1", "tool", "", ""),
                |_| Ok(vec![]),
            )
            .with_subscriptions([echo_plugin::ALL_EVENTS])
            .with_event_handler(move |_: &echo_plugin::PluginEvent| {
                h.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        ))
        .expect("mount ok");

        assert_eq!(
            host.deliver_plugin_event(&echo_plugin::PluginEvent::host(
                "tool.called",
                serde_json::Value::Null
            )),
            1
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // descriptor 里能看到订阅声明（面板据此显示）
        assert_eq!(
            host.descriptors()[0].subscribed_events,
            vec![echo_plugin::ALL_EVENTS.to_string()]
        );
    }
}
