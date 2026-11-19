//! # agent-module-tools — 工具子系统机制（解耦计划 Phase 3 前置）
//!
//! `Tool` 词汇与接缝在定义层 [`crate::tool`]；本 crate 只持有**运行期
//! 机制** [`ToolRegistry`]——原 `echo-agent::packages::tool` 的独立 crate 化，
//! 使工具类模块（`tools_builtin` / `instance_ops` / `subagent` / 远程模块）
//! 能只依赖机制层注册工具，而不依赖 agent 框架。
//!
//! 三条不变量：
//!
//! 1. **注册可逆**：[`ToolRegistry::register_reversible`] 返回 `Disposer`，
//!    撤销时移除工具并失效定义缓存（dsh「注册即副作用」）；
//! 2. **定义缓存**：模型可见的工具清单缓存为 `Arc<Vec<ToolDefinition>>`，
//!    任何注册变更即失效——避免每次组 prompt 重算；
//! 3. **出口脱敏**：注入 `Redactor` 后，全部执行出口（`execute` /
//!    （`snapshot` / `snapshot_for_session`）同样递归脱敏字符串值；
//!    工具声明的机器字段（[`Tool::snapshot_machine_fields`]：稳定 id /
//!    epoch / 会话键等）是 schema 而非用户内容，跳过其字符串值，其余键
//!    一律脱敏（对象键不改写——schema 靠键匹配）。
//! **依赖纪律**：只依赖 `echo-defs` + `echo-context` + tokio——不认识任何
//! 具体工具实现（内置工具集在 `echo-agent` 的 `packages/tools_builtin`）。

pub use crate::tool::{Tool, ToolDefinition, ToolError, ToolResult};

pub use crate::execute::{
    invalid_tool_arguments, tool_state, ToolCallContext, ToolExecutionHost, ToolExecutor,
    ToolInvocation,
};

use std::collections::HashMap;
use std::sync::Arc;

use crate::disposer::Disposer;
use serde_json::Value;

/// 阻塞段防护：`tokio::task::block_in_place` 在 current_thread runtime 上
/// 直接 panic。multi_thread runtime 走 `block_in_place`（把执行线程让回
/// 调度器）；current_thread runtime 直接就地执行闭包——这些调用点本就处于
/// 同步/阻塞上下文（组装期、模块 mount/unmount），可接受，附 warn 以便定位；
/// 无 runtime（纯同步测试等）直接执行。
fn blocking_section<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        Ok(_) => {
            tracing::warn!(
                "blocking_section on current_thread runtime — running inline \
                 (block_in_place would panic)"
            );
            f()
        }
        Err(_) => f(),
    }
}

/// Registry of tools, keyed by name.
///
/// The map is `tokio::sync::RwLock`-protected so reversible registrations can
/// mutate it from `&Arc<Self>` (shared ownership), while static assembly uses
/// the same lock via `try_write` (no contention at boot).
/// 一个**外部模块**工具：工具实例 + 模块自报的包 id。
///
/// 为什么把 `package` 单独带出来：包 id 是模块贡献的元数据（
/// `ToolContribution.package`），不在 `Tool` trait 上——模块工具是
/// `RemoteTool`，注册进注册表时要一并记下它属于哪个包（前端按包分组勾选）。
pub struct ModuleTool {
    pub tool: Arc<dyn Tool>,
    pub package: Option<String>,
}

impl ModuleTool {
    pub fn new(tool: Arc<dyn Tool>, package: Option<String>) -> Self {
        Self { tool, package }
    }
}

/// [`ToolRegistry::register_module_tools`] 的逐条结果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModuleToolRegistration {
    /// 成功注册的工具名（按输入顺序）。
    pub registered: Vec<String>,
    /// 因与宿主保留名重名而跳过的模块工具：`(名字, 模块自报的包 id)`。
    pub skipped: Vec<(String, Option<String>)>,
}

impl ModuleToolRegistration {
    /// 是否有工具被跳过（调用方据此决定要不要 warn）。
    pub fn has_skips(&self) -> bool {
        !self.skipped.is_empty()
    }
}

pub struct ToolRegistry {
    tools: tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>,
    /// Cache of the definition list sent to the LLM; invalidated on any
    /// registration change.
    cached_definitions: tokio::sync::RwLock<Option<Arc<Vec<ToolDefinition>>>>,
    /// Names of tools disabled at runtime (excluded from the LLM definition
    /// list; calls fail with NotFound). Persisted via `[agent].disabled_tools`.
    disabled: tokio::sync::RwLock<std::collections::HashSet<String>>,
    /// Tool -> owning package id (e.g. "echo-agent.instance.qq"). Set by the
    /// composition root when assembling; the frontend uses it to group
    /// checkboxes so a whole package (tools + skills) can be toggled at once.
    packages: tokio::sync::RwLock<std::collections::HashMap<String, String>>,
    /// 注册期出现过的**重名**（name → 首次被覆盖时的说明）。
    ///
    /// 为什么记：`insert` 是静默覆盖——一个模块和一个内置工具叫同一个名字
    /// 时，后者会无声无息地顶掉前者（实测风险：一个外部模块的
    /// `calculator` / `web_search` 与内置工具同名）。重名本身**是事件**，
    /// 必须留痕：组合根装配完读一次，Panel 详情页也可展示。
    conflicts: tokio::sync::RwLock<std::collections::BTreeMap<String, String>>,
    /// 结果脱敏器（安全服务注入；`None` = 透传）。
    /// 出口统一处理：所有工具（内置 / 模块 / 远程 / 子代理直呼 / 联邦）
    /// 的结果文本经此处脱敏后才交给任何消费者（2026-10 敏感信息隔离）。
    ///
    /// 用 `std::sync::RwLock` 而不是 tokio 的：这个槽只存一个 `Arc`，读写都
    /// 只做克隆、**从不跨 await 持锁**。同步快照出口（[`Self::snapshot`]）
    /// 不能 await；此前 `try_read` 失败后退到 tokio 的 `blocking_read`，在
    /// current_thread runtime 上会直接 panic（阻塞了运行时线程）。std 锁
    /// 竞争时阻塞的是 OS 线程，不依赖运行时，因而不存在该 panic 路径。
    redactor: std::sync::RwLock<Option<Arc<dyn crate::sanitize::Redactor>>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("names", &self.names())
            .finish()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: tokio::sync::RwLock::new(HashMap::new()),
            cached_definitions: tokio::sync::RwLock::new(None),
            disabled: tokio::sync::RwLock::new(std::collections::HashSet::new()),
            packages: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            conflicts: tokio::sync::RwLock::new(std::collections::BTreeMap::new()),
            redactor: std::sync::RwLock::new(None),
        }
    }

    /// 注入结果脱敏器（组合根装配期调用；`None` = 关闭）。
    ///
    /// 生效范围：本注册表的**全部**执行出口（`execute` / `execute_rich`）
    /// 与结构化快照出口。锁只做一次 `Arc` 交换、不跨 await；装配期无竞争，
    /// 意外竞争时同步等锁（临界区仅一次赋值，不会长期占用）。
    ///
    /// 中毒处理：持锁线程 panic 留下的 poison **不能**阻止本次注入——否则
    /// 旧脱敏器（或 `None`）会继续生效，等于静默丢掉调用方的安全决策。
    pub fn set_redactor(&self, redactor: Option<Arc<dyn crate::sanitize::Redactor>>) {
        let mut slot = self
            .redactor
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = redactor;
    }

    /// 当前脱敏器（未注入 = None）。
    ///
    /// 锁读取只做一次 `Arc` 克隆，不跨 await 持锁。锁中毒时取回最后一次
    /// 写入的值（`into_inner`）：**不得**把中毒当成"没有脱敏器"而透传
    /// 未脱敏数据。
    fn redactor(&self) -> Option<Arc<dyn crate::sanitize::Redactor>> {
        match self.redactor.read() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => {
                tracing::error!(
                    "tool registry redactor lock poisoned; recovering last injected redactor"
                );
                poisoned.into_inner().clone()
            }
        }
    }

    /// 同步路径的脱敏器（快照出口是同步 API，不能 await）。
    ///
    /// 与 [`Self::redactor`] 同一个 std 锁：读不依赖 tokio 的
    /// `blocking_read`，竞争时阻塞 OS 线程等写锁释放（临界区只有一次 Arc
    /// 克隆），current_thread runtime 上不会 panic。**不得**把锁竞争当成
    /// "没有脱敏器"而原样透传。
    fn redactor_blocking(&self) -> Option<Arc<dyn crate::sanitize::Redactor>> {
        self.redactor()
    }

    /// Register a tool during static assembly.
    ///
    /// `&mut self` keeps the boot-time call sites simple; the map lock is
    /// uncontended at this point (`try_write` succeeds). For reversible
    /// registration use [`register_reversible`](Self::register_reversible).
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        let mut tools = self
            .tools
            .try_write()
            .expect("tools map uncontended at boot");
        let replaced = tools.insert(name.clone(), tool).is_some();
        drop(tools);
        if replaced {
            self.note_conflict(&name, "register");
        }
        self.invalidate_definitions();
    }

    /// 记一次重名覆盖（`context` = 覆盖发生的入口，便于定位是谁顶了谁）。
    fn note_conflict(&self, name: &str, context: &str) {
        tracing::warn!(
            tool = name,
            via = context,
            "工具重名：先注册的实现已被覆盖（后者生效）。同名不代表同义——\n\
             若是内置工具与模块工具撞名，请改名其一；模块作者见\
             document/modules/module-authoring.md"
        );
        match self.conflicts.try_write() {
            Ok(mut conflicts) => {
                conflicts
                    .entry(name.to_string())
                    .or_insert_with(|| context.to_string());
            }
            Err(_) => blocking_section(|| {
                self.conflicts
                    .blocking_write()
                    .entry(name.to_string())
                    .or_insert_with(|| context.to_string());
            }),
        }
    }

    /// 注册期出现过的重名（按名字排序；value = 首次覆盖发生的入口）。
    ///
    /// 组合根在装配完成后调用一次即可知道有没有工具被顶掉；测试用它把
    /// 「默认装配零重名」钉成不变量。
    pub fn name_conflicts(&self) -> std::collections::BTreeMap<String, String> {
        self.conflicts
            .try_read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Register a tool reversibly, returning a disposer that removes it and
    /// invalidates the definitions cache.
    ///
    /// Works from both sync (assembly) and async (module mount) contexts:
    /// inside a runtime the lock is taken without blocking the executor.
    /// Register a tool reversibly, returning a disposer that removes it and
    /// invalidates the definitions cache.
    ///
    /// Uncontended (assembly, test) paths take the lock directly; under
    /// contention inside a multi-thread runtime the lock is taken on the
    /// blocking pool. Disposal uses the same strategy.
    /// 注册一个工具，**生命周期 = 注册表本身**（无撤销句柄）。
    ///
    /// 与 [`Self::register_reversible`] 的分工：那个给"可装卸"的场景（模块
    /// mount/unmount），必须持有返回的 [`Disposer`]；这个给"这个工具属于
    /// 这个 agent"的场景——它不该被提前卸掉，也不该逼调用方存一个句柄。
    ///
    /// 真实踩到过的坑：装配期用 `register_reversible` 却不保存返回值，
    /// `Disposer` 在函数返回时立即 drop，工具**当场消失**（表现为"工具注册
    /// 了但模型看不到 schema"）。注册表寿命语义的入口因此是必要的。
    pub fn register_shared(self: &Arc<Self>, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        let replaced = if let Ok(mut tools) = self.tools.try_write() {
            tools.insert(name.clone(), tool).is_some()
        } else {
            let name_for_blocking = name.clone();
            blocking_section(move || {
                self.tools
                    .blocking_write()
                    .insert(name_for_blocking, tool)
                    .is_some()
            })
        };
        if replaced {
            self.note_conflict(&name, "register_shared");
        }
        self.invalidate_definitions();
    }

    pub fn register_reversible(self: &Arc<Self>, tool: Arc<dyn Tool>) -> Disposer {
        let name = tool.name().to_string();
        let replaced = if let Ok(mut tools) = self.tools.try_write() {
            tools.insert(name.clone(), tool).is_some()
        } else {
            let name_for_blocking = name.clone();
            blocking_section(move || {
                self.tools
                    .blocking_write()
                    .insert(name_for_blocking, tool)
                    .is_some()
            })
        };
        if replaced {
            self.note_conflict(&name, "register_reversible");
        }
        self.invalidate_definitions();
        let registry = self.clone();
        Disposer::from_fn(move || {
            let mut tools = match registry.tools.try_write() {
                Ok(tools) => tools,
                Err(_) => {
                    let registry = registry.clone();
                    let name = name.clone();
                    return blocking_section(move || {
                        registry.tools.blocking_write().remove(&name);
                        registry.invalidate_definitions();
                    });
                }
            };
            tools.remove(&name);
            drop(tools);
            registry.invalidate_definitions();
        })
    }

    fn invalidate_definitions(&self) {
        if let Ok(mut cache) = self.cached_definitions.try_write() {
            *cache = None;
        }
    }

    /// Toggle a tool's runtime enable/disable state. `false` when the tool
    /// does not exist. Invalidates the definition cache when changed.
    pub async fn set_enabled(&self, name: &str, enabled: bool) -> bool {
        if !self.tools.read().await.contains_key(name) {
            return false;
        }
        let mut disabled = self.disabled.write().await;
        let changed = if enabled {
            disabled.remove(name)
        } else {
            disabled.insert(name.to_string())
        };
        drop(disabled);
        if changed {
            self.invalidate_definitions();
        }
        true
    }

    /// Whether a registered tool is currently disabled.
    pub async fn is_disabled(&self, name: &str) -> bool {
        self.disabled.read().await.contains(name)
    }

    /// 同步门控探针：只有“当前已注册且未禁用”才算可用。
    ///
    /// 给系统提示词摘要等同步装配路径使用；模块未挂载时工具不在表里，
    /// 不能靠人格配置猜成可用。任一锁竞争都返回 false（本轮不注入摘要），
    /// 不在 current_thread runtime 上调用 tokio blocking_*，也不放宽权限。
    pub fn is_enabled_now(&self, name: &str) -> bool {
        let Ok(disabled) = self.disabled.try_read() else {
            return false;
        };
        let Ok(tools) = self.tools.try_read() else {
            return false;
        };
        tools.contains_key(name) && !disabled.contains(name)
    }

    /// 是否已注册同名工具（宿主保留名判定）。
    ///
    /// 与 `names().contains(..)` 的区别：直接查 map，O(1) 而不是 O(注册数)——
    /// 模块工具注册是逐条调用的，用 `names()` 会变成 O(n²)。
    pub fn contains_tool(&self, name: &str) -> bool {
        match self.tools.try_read() {
            Ok(tools) => tools.contains_key(name),
            Err(_) => blocking_section(|| self.tools.blocking_read().contains_key(name)),
        }
    }

    /// 注册一批**外部模块**工具，返回逐条结果。
    ///
    /// ## 语义：宿主保留名优先
    ///
    /// [`Self::register`] 的 `insert` 是**覆盖**语义——模块工具与内置/宿主工具
    /// 同名时，模块实现会无声无息地顶掉内置实现（典型：外部模块声明
    /// `calculator` / `web_search`，与内置同名，表现为"功能还在但行为变了"，
    /// 排查成本极高）。
    ///
    /// 因此模块工具走这条路径：**已注册的名字是保留名**，同名模块项被跳过并
    /// 记入返回值的 `skipped`（调用方据此 warn 点名），内置实现保持不变。
    /// 模块仍可提供其它不冲突的工具——冲突只淘汰冲突项，不做整体拒绝。
    ///
    /// 包标签按模块自报的 `package` 打（`None` 表示模块未声明）。
    pub fn register_module_tools(
        &mut self,
        tools: impl IntoIterator<Item = ModuleTool>,
    ) -> ModuleToolRegistration {
        let mut report = ModuleToolRegistration::default();
        for ModuleTool { tool, package } in tools {
            let name = tool.name().to_string();
            if self.contains_tool(&name) {
                report.skipped.push((name, package));
                continue;
            }
            self.register(tool);
            if let Some(pkg) = &package {
                self.set_package(&name, pkg.clone());
            }
            report.registered.push(name);
        }
        report
    }

    /// Assign the owning package id for a tool (metadata for the Panel).
    pub fn set_package(&self, name: &str, package: impl Into<String>) {
        self.packages
            .try_write()
            .map(|mut p| {
                p.insert(name.to_string(), package.into());
            })
            .ok();
    }

    /// Package id of a tool (None = not assigned).
    pub fn package_of(&self, name: &str) -> Option<String> {
        self.packages
            .try_read()
            .ok()
            .and_then(|p| p.get(name).cloned())
    }

    /// 包内成员名（同步尽力：锁竞争时返回空列表，调用方可跳过本轮）。
    pub fn package_names(&self, package: &str) -> Vec<String> {
        self.packages
            .try_read()
            .map(|p| {
                p.iter()
                    .filter(|(_, pkg)| pkg.as_str() == package)
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 同步禁用（try_write；锁竞争时返回 false 且不改动）。
    ///
    /// 供 sync 闭包路径（模块 mount/unmount 的 persona 名单收紧）使用；
    /// async 路径请用 [`Self::set_enabled`]。
    pub fn try_disable(&self, name: &str) -> bool {
        match self.disabled.try_write() {
            Ok(mut disabled) => {
                let changed = disabled.insert(name.to_string());
                drop(disabled);
                if changed {
                    self.invalidate_definitions();
                }
                true
            }
            Err(_) => false,
        }
    }

    /// Build (or reuse) the tool definition list sent to the LLM.
    ///
    /// Disabled tools are excluded: the model never sees them, and calls
    /// against them fail with [`ToolError::NotFound`].
    pub async fn definitions(&self) -> Arc<Vec<ToolDefinition>> {
        // Fast path: read lock.
        if let Some(defs) = self.cached_definitions.read().await.as_ref() {
            return Arc::clone(defs);
        }
        // Slow path: write lock with a re-check, so a concurrent rebuild
        // between the read and write is not wasted.
        let mut cache = self.cached_definitions.write().await;
        if let Some(defs) = cache.as_ref() {
            return Arc::clone(defs);
        }
        let defs: Vec<ToolDefinition> = {
            let tools = self.tools.read().await;
            let disabled = self.disabled.read().await;
            let mut list: Vec<ToolDefinition> = tools
                .values()
                .filter(|t| !disabled.contains(t.name()))
                .map(|t| ToolDefinition {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    parameters: Some(t.parameters()),
                })
                .collect();
            list.sort_by(|a, b| a.name.cmp(&b.name));
            list
        };
        let arc = Arc::new(defs);
        *cache = Some(Arc::clone(&arc));
        arc
    }

    /// Full tool definition list (including disabled tools) for the frontend
    /// browser. Not cached — requested on demand by the Panel.
    /// Returns `(definition, category, enabled, package)`.
    pub async fn full_definitions(&self) -> Vec<(ToolDefinition, String, bool, Option<String>)> {
        let tools = self.tools.read().await;
        let disabled = self.disabled.read().await;
        let packages = self.packages.try_read().ok();
        let mut list: Vec<(ToolDefinition, String, bool, Option<String>)> = tools
            .values()
            .map(|t| {
                let enabled = !disabled.contains(t.name());
                (
                    ToolDefinition {
                        name: t.name().to_string(),
                        description: t.description().to_string(),
                        parameters: Some(t.parameters()),
                    },
                    t.category().to_string(),
                    enabled,
                    packages.as_ref().and_then(|p| p.get(t.name()).cloned()),
                )
            })
            .collect();
        list.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        list
    }

    pub fn names(&self) -> Vec<String> {
        match self.tools.try_read() {
            Ok(tools) => tools.keys().cloned().collect(),
            Err(_) => blocking_section(|| self.tools.blocking_read().keys().cloned().collect()),
        }
    }
    /// Structured state snapshot of a registered tool, if it publishes one.
    ///
    /// **出口脱敏**：注入 [`crate::sanitize::Redactor`] 后，快照里的
    /// `String` **值**递归过卡口（对象键不改写——消费方按键匹配 schema）；
    /// [`Tool::snapshot_machine_fields`] 声明的机器字段（稳定 id / epoch /
    /// 会话键等）只跳过其字符串值，其余键（`name` / `text` / `summary` /
    /// `display` …）一律脱敏。数字 / 布尔 / null 原样保留；未注入 = 透传
    /// （旧行为）。
    ///
    /// 锁序：先在工具表读锁内拷出快照与机器字段声明，读锁释放后才取
    /// 脱敏器——不持有工具表锁去等脱敏器锁。
    pub fn snapshot(&self, name: &str) -> Option<Value> {
        let (snapshot, machine_fields) = {
            match self.tools.try_read() {
                Ok(tools) => match tools.get(name) {
                    Some(tool) => (tool.snapshot(), tool.snapshot_machine_fields()),
                    None => (None, &[] as &'static [&'static str]),
                },
                Err(_) => blocking_section(|| match self.tools.blocking_read().get(name) {
                    Some(tool) => (tool.snapshot(), tool.snapshot_machine_fields()),
                    None => (None, &[] as &'static [&'static str]),
                }),
            }
        };
        self.redact_snapshot(name, snapshot, machine_fields)
    }

    /// 按会话过滤的状态快照（2026-10，带会话维度的工具用；默认同
    /// [`Self::snapshot`]）。
    ///
    /// 与 [`Self::snapshot`] 同口径：会话视图里的字符串值同样在出口
    /// 脱敏（机器字段例外），且只含该会话的数据（不跨会话合并）。
    pub fn snapshot_for_session(&self, name: &str, session_id: &str) -> Option<Value> {
        let (snapshot, machine_fields) = {
            match self.tools.try_read() {
                Ok(tools) => match tools.get(name) {
                    Some(tool) => (
                        tool.snapshot_for_session(session_id),
                        tool.snapshot_machine_fields(),
                    ),
                    None => (None, &[] as &'static [&'static str]),
                },
                Err(_) => blocking_section(|| match self.tools.blocking_read().get(name) {
                    Some(tool) => (
                        tool.snapshot_for_session(session_id),
                        tool.snapshot_machine_fields(),
                    ),
                    None => (None, &[] as &'static [&'static str]),
                }),
            }
        };
        self.redact_snapshot(name, snapshot, machine_fields)
    }

    /// 快照出口共用脱敏：递归改写字符串值，跳过工具声明的机器字段。
    ///
    /// 调用时**不得持有工具表读锁**（快照已拷贝出来；锁序见
    /// [`Self::snapshot`]）。未注入脱敏器 = 原样透传。
    fn redact_snapshot(
        &self,
        tool: &str,
        snapshot: Option<Value>,
        machine_fields: &'static [&'static str],
    ) -> Option<Value> {
        let mut snapshot = snapshot?;
        let Some(redactor) = self.redactor_blocking() else {
            return Some(snapshot);
        };
        let changed = redact_value_strings(redactor.as_ref(), &mut snapshot, machine_fields);
        if changed > 0 {
            // debug 而非 warn：正常使用里用户完全可能把已登记秘密写进
            // 清单项，快照每次刷新都 warn 会持续刷屏；安全动作本身不变。
            tracing::debug!(
                tool,
                changed,
                "tool state snapshot contained sensitive data; redacted at exit"
            );
        }
        Some(snapshot)
    }
    /// 按名字取工具句柄（读侧访问器：执行链拿它问工具自己的声明）。
    ///
    /// 为什么需要它：状态广播要问"这个工具有可见状态吗"（[`Tool::state_key`]），
    /// 那是工具实例上的知识，而注册表的其它方法都只回值不回句柄。
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        match self.tools.try_read() {
            Ok(tools) => tools.get(name).cloned(),
            Err(_) => blocking_section(|| self.tools.blocking_read().get(name).cloned()),
        }
    }

    /// 可被**远程调用**的工具名（[`Tool::remote_capable`] 声明，2026-11 S8）。
    ///
    /// 跳过被禁用的工具：握手宣告的是"本单位现在真的能接的活"，
    /// 停用的模块不该还挂在 `NodeCaps.tools` 里（对端注册出来的代理工具
    /// 会一路活着，直到调用时才报错）。排序保证同配置多节点宣告一致
    /// （便于对账与测试）。
    ///
    /// 同步尽力：任一锁竞争（读不到禁用集合或工具表）时**什么都不宣告**
    /// （fail-closed）。此前把 `disabled.try_read()` 失败当成"没有禁用工具"，
    /// 锁竞争窗口里停用的模块会被重新宣告出去——对端据此注册的代理工具
    /// 一直活到调用时才报错，宣告与执行能力静默分叉。
    pub fn remote_capable_names(&self) -> Vec<String> {
        let Ok(disabled) = self.disabled.try_read() else {
            tracing::warn!(
                "tool registry disabled-set lock busy — advertising no remote tools (fail-closed)"
            );
            return Vec::new();
        };
        let Ok(tools) = self.tools.try_read() else {
            return Vec::new();
        };
        let mut names: Vec<String> = tools
            .iter()
            .filter(|(name, tool)| tool.remote_capable() && !disabled.contains(name.as_str()))
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// 可被远程调用的工具**定义**（名 + 描述 + 参数 schema），2026-11。
    ///
    /// 与 [`Self::remote_capable_names`] 同源同过滤，只是多带描述与 schema：
    /// 握手 `NodeCaps.tool_definitions` 用它，大脑侧据此注册代理工具——
    /// **不再用本机同名工具猜对端参数**（两边版本/模块不同就会猜错）。
    ///
    /// 旧节点不发这个字段（`serde(default)` 得空表）；大脑侧对空表**不再
    /// 猜 schema**（不自动注册代理工具，node:// 显式调用仍可用），所以不
    /// 提升协议版本。锁竞争与 [`Self::remote_capable_names`] 同口径：
    /// fail-closed，本轮不宣告任何定义。
    pub fn remote_capable_definitions(&self) -> Vec<crate::tool::ToolDefinition> {
        let Ok(disabled) = self.disabled.try_read() else {
            tracing::warn!(
                "tool registry disabled-set lock busy — advertising no remote tool definitions (fail-closed)"
            );
            return Vec::new();
        };
        let Ok(tools) = self.tools.try_read() else {
            return Vec::new();
        };
        let mut definitions: Vec<crate::tool::ToolDefinition> = tools
            .iter()
            .filter(|(name, tool)| tool.remote_capable() && !disabled.contains(name.as_str()))
            .map(|(name, tool)| crate::tool::ToolDefinition {
                name: name.clone(),
                description: tool.description().to_string(),
                parameters: Some(tool.parameters()),
            })
            .collect();
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        definitions
    }

    /// 某工具声明的**路径参数名**（[`Tool::path_arguments`]；未声明/未知工具 = 空）。
    ///
    /// 联邦执行端据此决定"要不要做工作区沙箱裁决"——判定依据从模块里的
    /// 工具名清单变成了工具自己的声明（2026-11 S8）。
    pub fn path_arguments_of(&self, name: &str) -> &'static [&'static str] {
        self.get(name)
            .map(|tool| tool.path_arguments())
            .unwrap_or(&[])
    }

    /// Parameter JSON schema of a registered tool, if present.
    pub async fn parameters(&self, name: &str) -> Option<Value> {
        self.tools
            .read()
            .await
            .get(name)
            .map(|tool| tool.parameters())
    }

    /// Self-declared execution timeout of a registered tool for the given
    /// arguments, if the tool publishes one (see [`Tool::timeout_hint`]).
    /// 该工具成功调用是否即产生平台可见输出（见 `Tool::produces_visible_output`）。
    ///
    /// 循环用它决定是否抑制「临时等待回复」；未知工具 = false。
    pub async fn produces_visible_output(&self, name: &str) -> bool {
        self.tools
            .read()
            .await
            .get(name)
            .map(|tool| tool.produces_visible_output())
            .unwrap_or(false)
    }

    pub async fn timeout_hint(&self, name: &str, arguments: &Value) -> Option<std::time::Duration> {
        self.tools
            .read()
            .await
            .get(name)
            .and_then(|tool| tool.timeout_hint(arguments))
    }

    pub async fn execute(&self, name: &str, arguments: Value) -> Result<String, ToolError> {
        let (tool, disabled) = {
            let tools = self.tools.read().await;
            let disabled = self.disabled.read().await;
            (tools.get(name).cloned(), disabled.contains(name))
        };
        if disabled {
            return Err(ToolError::NotFound(name.to_string()));
        }
        let result = match tool {
            Some(tool) => tool.execute(arguments).await,
            None => Err(ToolError::NotFound(name.to_string())),
        };
        self.redact_text_outcome(result).await
    }

    /// 带**调用上下文**执行（`Tool::execute_in` 的注册表入口）。
    ///
    /// 与 [`Self::execute_rich`] 的唯一区别是把上下文递给工具——需要会话 /
    /// 人格 / 取消信号的工具（清单、shell、子代理）靠它取代历史上的魔术参数
    /// 注入。不覆写 `execute_in` 的工具行为完全不变。
    pub async fn execute_rich_in(
        &self,
        cx: &crate::tool::ToolContext,
        name: &str,
        arguments: Value,
    ) -> Result<crate::tool::ToolResult, ToolError> {
        let (tool, disabled) = {
            let tools = self.tools.read().await;
            let disabled = self.disabled.read().await;
            (tools.get(name).cloned(), disabled.contains(name))
        };
        if disabled {
            return Err(ToolError::NotFound(name.to_string()));
        }
        let result = match tool {
            Some(tool) => tool.execute_in(cx, arguments).await,
            None => Err(ToolError::NotFound(name.to_string())),
        };
        self.redact_rich_outcome(result).await
    }

    /// Execute a tool with multimodal output support (text + images).
    pub async fn execute_rich(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<crate::tool::ToolResult, ToolError> {
        let (tool, disabled) = {
            let tools = self.tools.read().await;
            let disabled = self.disabled.read().await;
            (tools.get(name).cloned(), disabled.contains(name))
        };
        if disabled {
            return Err(ToolError::NotFound(name.to_string()));
        }
        let result = match tool {
            Some(tool) => tool.execute_rich(arguments).await,
            None => Err(ToolError::NotFound(name.to_string())),
        };
        self.redact_rich_outcome(result).await
    }

    /// 出口脱敏（文本版）：结果与错误消息都经脱敏器处理后返回。
    /// 未注入脱敏器 = 原样透传。
    async fn redact_text_outcome(
        &self,
        result: Result<String, ToolError>,
    ) -> Result<String, ToolError> {
        let Some(redactor) = self.redactor() else {
            return result;
        };
        match result {
            Ok(text) => {
                let report = redactor.redact(&text);
                Ok(if report.changed { report.text } else { text })
            }
            Err(error) => Err(redact_error(redactor.as_ref(), error)),
        }
    }

    /// 出口脱敏（多模态版）：仅改写文本通道，图片原样。
    async fn redact_rich_outcome(
        &self,
        result: Result<crate::tool::ToolResult, ToolError>,
    ) -> Result<crate::tool::ToolResult, ToolError> {
        let Some(redactor) = self.redactor() else {
            return result;
        };
        match result {
            Ok(mut rich) => {
                let report = redactor.redact(&rich.text);
                if report.changed {
                    rich.text = report.text;
                }
                Ok(rich)
            }
            Err(error) => Err(redact_error(redactor.as_ref(), error)),
        }
    }
    /// Bulk enable/disable every tool owned by a package (package id = 模块 id，
    /// 装配时打标）。同步接口：模块 mount/unmount 闭包是同步上下文，锁竞争
    /// 时退到 blocking pool。返回受影响的工具数。
    pub fn set_package_enabled(&self, package: &str, enabled: bool) -> usize {
        if let (Ok(packages), Ok(mut disabled)) =
            (self.packages.try_read(), self.disabled.try_write())
        {
            let names: Vec<String> = packages
                .iter()
                .filter(|(_, p)| p.as_str() == package)
                .map(|(n, _)| n.clone())
                .collect();
            for name in &names {
                if enabled {
                    disabled.remove(name);
                } else {
                    disabled.insert(name.clone());
                }
            }
            drop(disabled);
            if !names.is_empty() {
                self.invalidate_definitions();
            }
            return names.len();
        }
        // 锁竞争路径：退到 blocking pool（与 register_reversible 同模式）。
        // block_in_place 在当前线程执行闭包，可直接借用 self。
        blocking_section(|| {
            let packages = self.packages.blocking_read();
            let mut disabled = self.disabled.blocking_write();
            let names: Vec<String> = packages
                .iter()
                .filter(|(_, p)| p.as_str() == package)
                .map(|(n, _)| n.clone())
                .collect();
            for name in &names {
                if enabled {
                    disabled.remove(name);
                } else {
                    disabled.insert(name.clone());
                }
            }
            drop(disabled);
            if !names.is_empty() {
                self.invalidate_definitions();
            }
            names.len()
        })
    }

    pub fn len(&self) -> usize {
        match self.tools.try_read() {
            Ok(tools) => tools.len(),
            Err(_) => blocking_section(|| self.tools.blocking_read().len()),
        }
    }
    pub fn is_empty(&self) -> bool {
        match self.tools.try_read() {
            Ok(tools) => tools.is_empty(),
            Err(_) => blocking_section(|| self.tools.blocking_read().is_empty()),
        }
    }
}

/// 工具错误消息的出口脱敏：保持原变体形态（Display 前缀不重复），
/// 只改写变体内的文本。
fn redact_error(redactor: &dyn crate::sanitize::Redactor, error: ToolError) -> ToolError {
    fn redact_inner(redactor: &dyn crate::sanitize::Redactor, inner: String) -> String {
        let report = redactor.redact(&inner);
        if report.changed {
            report.text
        } else {
            inner
        }
    }
    match error {
        ToolError::InvalidArguments(inner) => {
            ToolError::InvalidArguments(redact_inner(redactor, inner))
        }
        ToolError::Execution(inner) => ToolError::Execution(redact_inner(redactor, inner)),
        ToolError::NotFound(inner) => ToolError::NotFound(redact_inner(redactor, inner)),
    }
}

/// 结构化快照的递归脱敏：改写字符串 **值**，返回被改写的字符串个数。
///
/// - 对象**键**不改写：键是 schema 的一部分，改成占位符会让消费方
///   （面板 / 事件协议）查不到字段；
/// - `machine_fields` 里的键只跳过其 **`String` 值**（稳定 id / epoch /
///   会话键等机器字段；按 UUID 形态全局跳过是不允许的——同形态的用户
///   秘密文本会因此漏出）。这些键下若意外出现对象 / 数组，内容仍递归
///   脱敏；
/// - 数组与嵌套对象逐层下钻；
/// - 数字 / 布尔 / null 原样保留。
///
/// 直接改结构化 `Value`，不做"序列化成文本替换再解析"——后者会破坏
/// 转义与类型边界（例如数字字符串、引号、换行）。
fn redact_value_strings(
    redactor: &dyn crate::sanitize::Redactor,
    value: &mut Value,
    machine_fields: &[&str],
) -> usize {
    match value {
        Value::String(text) => {
            let report = redactor.redact(text);
            if report.changed {
                *text = report.text;
                1
            } else {
                0
            }
        }
        Value::Array(items) => {
            let mut changed = 0;
            for item in items {
                changed += redact_value_strings(redactor, item, machine_fields);
            }
            changed
        }
        Value::Object(map) => {
            let mut changed = 0;
            for (key, item) in map.iter_mut() {
                if machine_fields.contains(&key.as_str()) {
                    // 机器字段：只豁免标量字符串（id / epoch / session），
                    // 防止声明一个键就把其下所有用户内容一并放过。
                    if matches!(item, Value::String(_)) {
                        continue;
                    }
                }
                changed += redact_value_strings(redactor, item, machine_fields);
            }
            changed
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用 Redactor（避免 echo-test-utils 的 dev 循环依赖导致双份 echo-defs）。
    struct TestRedactor {
        inner: std::sync::RwLock<TestRedactorInner>,
    }
    #[derive(Default)]
    struct TestRedactorInner {
        secrets: Vec<(String, String)>,
        builtin_sk: bool,
    }
    impl TestRedactor {
        fn new() -> Self {
            Self {
                inner: std::sync::RwLock::new(TestRedactorInner::default()),
            }
        }
        fn with_builtin_patterns() -> Self {
            Self {
                inner: std::sync::RwLock::new(TestRedactorInner {
                    secrets: Vec::new(),
                    builtin_sk: true,
                }),
            }
        }
    }
    impl crate::sanitize::Redactor for TestRedactor {
        fn name(&self) -> &str {
            "test"
        }
        fn redact(&self, text: &str) -> crate::sanitize::RedactReport {
            let hits = self.scan(text);
            if hits.is_empty() {
                return crate::sanitize::RedactReport {
                    text: text.to_string(),
                    changed: false,
                    hits,
                };
            }
            let mut out = String::new();
            let mut cursor = 0usize;
            for hit in &hits {
                out.push_str(&text[cursor..hit.span.0]);
                out.push_str(&format!("【已隐藏:{}】", hit.label));
                cursor = hit.span.1;
            }
            out.push_str(&text[cursor..]);
            crate::sanitize::RedactReport {
                text: out,
                changed: true,
                hits,
            }
        }
        fn scan(&self, text: &str) -> Vec<crate::sanitize::SecretHit> {
            let inner = self.inner.read().unwrap();
            let mut hits = Vec::new();
            let mut spans: Vec<(usize, usize, String)> = Vec::new();
            for (value, label) in &inner.secrets {
                if let Some(pos) = text.find(value.as_str()) {
                    spans.push((pos, pos + value.len(), label.clone()));
                }
            }
            if inner.builtin_sk {
                let bytes = text.as_bytes();
                let mut i = 0;
                while i + 2 < bytes.len() {
                    if bytes[i] == b's' && bytes[i + 1] == b'k' && bytes.get(i + 2) == Some(&b'-') {
                        let start = i;
                        let mut end = i + 3;
                        while end < bytes.len()
                            && (bytes[end].is_ascii_alphanumeric()
                                || bytes[end] == b'_'
                                || bytes[end] == b'-')
                        {
                            end += 1;
                        }
                        if end > start + 3 {
                            spans.push((start, end, "pattern:openai_key".to_string()));
                            i = end;
                            continue;
                        }
                    }
                    i += 1;
                }
            }
            spans.sort_by_key(|s| s.0);
            let mut last = 0usize;
            for (a, b, label) in spans {
                if a < last {
                    continue;
                }
                last = b;
                let kind = if label.starts_with("pattern:") {
                    crate::sanitize::HitKind::Pattern
                } else {
                    crate::sanitize::HitKind::Registered
                };
                hits.push(crate::sanitize::SecretHit {
                    label,
                    kind,
                    span: (a, b),
                });
            }
            hits
        }
        fn register_secret(&self, value: &str, label: &str) {
            self.inner
                .write()
                .unwrap()
                .secrets
                .push((value.to_string(), label.to_string()));
        }
        fn register_pattern(
            &self,
            _name: &str,
            _pattern: &str,
        ) -> Result<(), crate::sanitize::PatternError> {
            Ok(())
        }
        fn registered_count(&self) -> usize {
            self.inner.read().unwrap().secrets.len()
        }
    }
    use crate::sanitize::Redactor;
    use async_trait::async_trait;

    struct StubTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "stub"
        }
        async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
            Ok("stub".into())
        }
    }

    /// 声明类桩工具：远程面 + 路径参数（S8）。
    struct DeclaringTool {
        name: &'static str,
        remote: bool,
        path_args: &'static [&'static str],
    }

    #[async_trait]
    impl Tool for DeclaringTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "declaring stub"
        }
        async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
            Ok("ok".into())
        }
        fn remote_capable(&self) -> bool {
            self.remote
        }
        fn path_arguments(&self) -> &'static [&'static str] {
            self.path_args
        }
    }

    /// 远程面集合来自**工具声明**，且停用的工具不再宣告（S8）。
    ///
    /// 这条锁住的是"谁有权决定我能被远程调用"：此前是组合根与联邦模块里
    /// 两份写死的名字表，模块无法加入、改名则静默掉出。
    #[test]
    fn remote_capable_names_follow_declarations_and_skip_disabled() {
        let registry: Arc<ToolRegistry> = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(DeclaringTool {
            name: "read_file",
            remote: true,
            path_args: &["path"],
        }));
        registry.register_shared(Arc::new(DeclaringTool {
            name: "alpha",
            remote: false,
            path_args: &[],
        }));
        registry.register_shared(Arc::new(StubTool { name: "plain" }));

        assert_eq!(
            registry.remote_capable_names(),
            vec!["read_file"],
            "只宣告不声明 → 不可远程（fail-closed）"
        );

        // 模块停用 → 立刻从宣告里消失（对端下次握手就不会再注册它）。
        registry.set_package_enabled("pkg-a", false);
        registry.set_package("read_file", "pkg-a");
        registry.set_package_enabled("pkg-a", false);
        assert!(
            registry.remote_capable_names().is_empty(),
            "停用的工具不得继续宣告: {:?}",
            registry.remote_capable_names()
        );

        // 路径参数声明：未声明 / 未知工具都是空切片（调用方据此跳过沙箱）。
        registry.set_package_enabled("pkg-a", true);
        assert_eq!(registry.path_arguments_of("read_file"), ["path"]);
        assert!(registry.path_arguments_of("alpha").is_empty());
        assert!(registry.path_arguments_of("nope").is_empty());
    }

    /// 锁竞争时**不得**把停用集合当成空集（fail-open）：读不到就什么都不宣告。
    ///
    /// 这条守的是握手窗口：`disabled.try_read()` 失败曾被 `unwrap_or(false)`
    /// 吞成"未禁用"，于是模块停用后、只要恰好有写锁在飞，对端就会重新看到
    /// 并注册早已停用的远程工具。
    #[tokio::test]
    async fn remote_surface_fails_closed_when_disabled_lock_is_contended() {
        let registry: Arc<ToolRegistry> = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(DeclaringTool {
            name: "read_file",
            remote: true,
            path_args: &["path"],
        }));
        assert_eq!(registry.remote_capable_names(), vec!["read_file"]);

        let guard = registry.disabled.write().await;
        assert!(
            registry.remote_capable_names().is_empty(),
            "禁用集合不可读时必须 fail-closed（不能假设未禁用）"
        );
        assert!(
            registry.remote_capable_definitions().is_empty(),
            "定义上报同样 fail-closed"
        );
        drop(guard);
        assert_eq!(
            registry.remote_capable_names(),
            vec!["read_file"],
            "锁释放后恢复宣告"
        );
    }

    /// 输出可区分的桩工具：用来断言"同名时到底哪个实现生效"。
    struct TaggedTool {
        name: &'static str,
        output: &'static str,
    }

    #[async_trait]
    impl Tool for TaggedTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "tagged"
        }
        async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
            Ok(self.output.into())
        }
    }

    /// 宿主保留名优先（2026-10 去耦重构后的注册语义，抽成纯函数便于测试）。
    ///
    /// 背景：`register` 的 `insert` 是覆盖语义。内置工具外迁为模块后，
    /// 外部模块声明的 `calculator` / `web_search` 与内置同名，于是**内置实现
    /// 被悄悄顶掉**——线上表现是"功能还在但行为变了"，五条 ERROR 日志。
    ///
    /// 契约：
    /// 1. 与已注册工具同名的模块工具被**跳过**（内置实现保留）；
    /// 2. 非冲突的模块工具照常注册，并按模块自报的 package 打标；
    /// 3. 报告逐条列出 registered / skipped，调用方据此 warn 点名。
    #[tokio::test]
    async fn module_tools_never_override_host_reserved_names() {
        let mut registry = ToolRegistry::new();
        // 宿主（内置）实现
        registry.register(Arc::new(TaggedTool {
            name: "calculator",
            output: "builtin",
        }));
        registry.set_package("calculator", "echo-agent.tools.builtin");

        let report = registry.register_module_tools(vec![
            // 冲突项：与内置同名
            ModuleTool::new(
                Arc::new(TaggedTool {
                    name: "calculator",
                    output: "module",
                }),
                Some("echo-module-example".into()),
            ),
            // 非冲突项
            ModuleTool::new(
                Arc::new(TaggedTool {
                    name: "module_only",
                    output: "module",
                }),
                Some("echo-module-example".into()),
            ),
        ]);

        assert_eq!(report.registered, vec!["module_only"]);
        assert_eq!(
            report.skipped,
            vec![(
                "calculator".to_string(),
                Some("echo-module-example".to_string())
            )]
        );
        assert!(report.has_skips());

        // 内置实现仍然生效（关键断言：不是"功能消失"，也不是"被换掉"）
        assert_eq!(
            registry.execute("calculator", Value::Null).await.unwrap(),
            "builtin"
        );
        // 包标签没被模块覆盖
        assert_eq!(
            registry.package_of("calculator").as_deref(),
            Some("echo-agent.tools.builtin")
        );
        // 非冲突项按模块自报的包打标
        assert_eq!(
            registry.package_of("module_only").as_deref(),
            Some("echo-module-example")
        );
        // 冲突**不**记进 name_conflicts（没有发生覆盖）
        assert!(registry.name_conflicts().is_empty());
    }

    /// 模块之间同名：先注册者生效，后到者跳过（与宿主保留名同一规则）。
    #[tokio::test]
    async fn duplicate_module_tool_names_keep_the_first() {
        let mut registry = ToolRegistry::new();
        let report = registry.register_module_tools(vec![
            ModuleTool::new(
                Arc::new(TaggedTool {
                    name: "dup",
                    output: "first",
                }),
                Some("pkg-a".into()),
            ),
            ModuleTool::new(
                Arc::new(TaggedTool {
                    name: "dup",
                    output: "second",
                }),
                Some("pkg-b".into()),
            ),
        ]);
        assert_eq!(report.registered, vec!["dup"]);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(
            registry.execute("dup", Value::Null).await.unwrap(),
            "first",
            "后到的模块不得覆盖先注册的实现"
        );
        assert_eq!(registry.package_of("dup").as_deref(), Some("pkg-a"));
    }

    /// 空输入是 no-op（组合根在无模块时也会调用）。
    #[tokio::test]
    async fn register_module_tools_accepts_empty_input() {
        let mut registry = ToolRegistry::new();
        let report = registry.register_module_tools(Vec::new());
        assert!(report.registered.is_empty());
        assert!(!report.has_skips());
        assert!(registry.names().is_empty());
    }

    /// 模块未声明 package 时也能注册（`None` 不打标，不影响可用性）。
    #[tokio::test]
    async fn module_tool_without_package_is_registered_without_label() {
        let mut registry = ToolRegistry::new();
        let report = registry.register_module_tools(vec![ModuleTool::new(
            Arc::new(StubTool { name: "unlabeled" }),
            None,
        )]);
        assert_eq!(report.registered, vec!["unlabeled"]);
        assert_eq!(registry.package_of("unlabeled"), None);
        assert!(registry.contains_tool("unlabeled"));
    }

    #[tokio::test]
    async fn package_toggle_bulk_disables_and_restores() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubTool { name: "a1" }));
        registry.register(Arc::new(StubTool { name: "a2" }));
        registry.register(Arc::new(StubTool { name: "b1" }));
        registry.set_package("a1", "pkg-a");
        registry.set_package("a2", "pkg-a");
        registry.set_package("b1", "pkg-b");

        assert_eq!(registry.set_package_enabled("pkg-a", false), 2);
        assert!(registry.is_disabled("a1").await);
        assert!(registry.is_disabled("a2").await);
        assert!(!registry.is_disabled("b1").await, "other package untouched");
        let visible: Vec<String> = registry
            .definitions()
            .await
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert_eq!(visible, vec!["b1"], "definitions exclude disabled package");

        assert_eq!(registry.set_package_enabled("pkg-a", true), 2);
        assert!(!registry.is_disabled("a1").await);
        assert_eq!(registry.definitions().await.len(), 3);
        assert_eq!(registry.set_package_enabled("nonexistent", false), 0);
    }

    #[tokio::test]
    async fn reversible_registration_unregisters_and_invalidates_cache() {
        let registry: Arc<ToolRegistry> = Arc::new(ToolRegistry::new());
        let _keep = registry.register_reversible(Arc::new(StubTool { name: "stub" }));
        assert_eq!(registry.names(), vec!["stub"]);
        let defs = registry.definitions().await;
        assert_eq!(defs.len(), 1);

        let disposer = registry.register_reversible(Arc::new(StubTool { name: "stub2" }));
        assert_eq!(registry.definitions().await.len(), 2, "cache rebuilt");

        disposer.dispose();
        assert_eq!(registry.names(), vec!["stub"]);
        assert_eq!(
            registry.definitions().await.len(),
            1,
            "cache invalidated after dispose"
        );
    }

    #[tokio::test]
    async fn disable_hides_tool_from_definitions_and_blocks_calls() {
        let registry: Arc<ToolRegistry> = Arc::new(ToolRegistry::new());
        let _keep_stub = registry.register_reversible(Arc::new(StubTool { name: "stub" }));
        let _keep_keeper = registry.register_reversible(Arc::new(StubTool { name: "keeper" }));
        assert_eq!(registry.definitions().await.len(), 2);
        assert!(registry.is_enabled_now("stub"));
        assert!(!registry.is_enabled_now("ghost"), "未挂载不是可用");

        assert!(registry.set_enabled("stub", false).await);
        assert!(!registry.is_enabled_now("stub"));
        assert!(registry.is_enabled_now("keeper"));
        let names: Vec<String> = registry
            .definitions()
            .await
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert_eq!(names, vec!["keeper"], "disabled tool hidden from LLM");

        assert!(registry.is_disabled("stub").await);
        assert!(
            registry.execute("stub", Value::Null).await.is_err(),
            "disabled tool calls fail"
        );

        // Re-enable restores visibility and callability.
        assert!(registry.set_enabled("stub", true).await);
        assert!(registry.is_enabled_now("stub"));
        assert_eq!(registry.definitions().await.len(), 2);
        assert_eq!(registry.execute("stub", Value::Null).await.unwrap(), "stub");
        // Unknown tool cannot be toggled.
        assert!(!registry.set_enabled("nope", false).await);
    }

    #[tokio::test]
    async fn full_definitions_reports_categories_and_enabled_state() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubTool { name: "stub" }));
        let registry = Arc::new(registry);
        let list = registry.full_definitions().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0.name, "stub");
        assert_eq!(list[0].1, "builtin");
        assert!(list[0].2);
        registry.set_enabled("stub", false).await;
        let list = registry.full_definitions().await;
        assert!(!list[0].2, "disabled state reported for frontend");
    }

    #[tokio::test]
    async fn static_register_and_reversible_share_the_map() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubTool { name: "static" }));
        let registry: Arc<ToolRegistry> = Arc::new(registry);
        let _keep = registry.register_reversible(Arc::new(StubTool { name: "dynamic" }));
        assert_eq!(registry.definitions().await.len(), 2);
        assert_eq!(
            registry.execute("static", Value::Null).await.unwrap(),
            "stub"
        );
        assert!(registry.execute("missing", Value::Null).await.is_err());
    }

    /// 工具结果出口卡口：注册的秘密从结果文本中被替换（2026-10 安全）。
    #[tokio::test]
    async fn execute_redacts_registered_secret_from_result() {
        const KEY: &str = "sk-executeexecuteredact01";
        struct LeakyTool;
        #[async_trait]
        impl Tool for LeakyTool {
            fn name(&self) -> &str {
                "leaky"
            }
            fn description(&self) -> &str {
                "leaky"
            }
            async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
                Ok(format!(
                    "file contains {KEY}; also pattern sk-abcdefghijklmnopqrstuvwxy"
                ))
            }
        }
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(LeakyTool));
        let redactor = Arc::new(TestRedactor::with_builtin_patterns());
        redactor.register_secret(KEY, "api_key:test");
        registry.set_redactor(Some(redactor));

        let text = registry.execute("leaky", Value::Null).await.unwrap();
        assert!(!text.contains(KEY), "registered secret must be redacted");
        assert!(!text.contains("sk-abcdefghijklmnopqrstuvwxy"));
        assert!(text.contains("【已隐藏:api_key:test】"));
        assert!(text.contains("【已隐藏:pattern:openai_key】"));

        // execute_rich 同一出口。
        let rich = registry.execute_rich("leaky", Value::Null).await.unwrap();
        assert!(!rich.text.contains(KEY));
    }

    /// 错误消息同样过脱敏（结果出口对 Ok/Err 一视同仁）。
    #[tokio::test]
    async fn execute_redacts_tool_error_message() {
        const KEY: &str = "sk-errortoolerrortool0001";
        struct ErrTool;
        #[async_trait]
        impl Tool for ErrTool {
            fn name(&self) -> &str {
                "err-tool"
            }
            fn description(&self) -> &str {
                "err"
            }
            async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
                Err(ToolError::Execution(format!("connect failed for {KEY}")))
            }
        }
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ErrTool));
        let redactor = Arc::new(TestRedactor::new());
        redactor.register_secret(KEY, "api_key:test");
        registry.set_redactor(Some(redactor));

        let error = registry.execute("err-tool", Value::Null).await.unwrap_err();
        let text = error.to_string();
        assert!(!text.contains(KEY));
        assert!(text.contains("【已隐藏:api_key:test】"));
    }

    /// 未注入脱敏器 = 旧行为（透传）。
    #[tokio::test]
    async fn execute_passes_through_without_redactor() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubTool { name: "stub" }));
        registry.set_redactor(None);
        assert_eq!(registry.execute("stub", Value::Null).await.unwrap(), "stub");
    }

    /// 结构化快照桩：值里埋秘密，覆盖嵌套对象 / 数组 / 引号换行 /
    /// 秘密形态的对象键；会话快照按 session_id 返回不同数据。
    const SNAPSHOT_SECRET: &str = "snapshot-secret-7f3a9c";

    struct StatefulTool;

    #[async_trait]
    impl Tool for StatefulTool {
        fn name(&self) -> &str {
            "stateful"
        }
        fn description(&self) -> &str {
            "stateful stub"
        }
        async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
            Ok("ok".into())
        }
        fn snapshot(&self) -> Option<Value> {
            let mut root = serde_json::json!({
                "text": format!("before {SNAPSHOT_SECRET} after"),
                "quoted": format!("line1 \"{SNAPSHOT_SECRET}\"\nline2"),
                "nested": {
                    "token": SNAPSHOT_SECRET,
                    "count": 3,
                    "ok": true,
                    "none": null,
                },
                "items": [SNAPSHOT_SECRET, 7, {"deep": SNAPSHOT_SECRET}],
            });
            // 键名本身命中秘密形态：键不得被改写（schema 契约）。
            root[SNAPSHOT_SECRET] = Value::String("value-kept".into());
            Some(root)
        }
        fn snapshot_for_session(&self, session_id: &str) -> Option<Value> {
            match session_id {
                "session-a" | "session-b" => Some(serde_json::json!({
                    "session": session_id,
                    "items": [{
                        "text": format!("{session_id}:{SNAPSHOT_SECRET}"),
                        "done": false,
                    }],
                })),
                _ => None,
            }
        }
    }

    /// 机器字段桩：UUID 形态的**秘密文本**出现在用户字段里仍必须脱敏，
    /// 而声明为机器字段的稳定 id / epoch / session 必须原样保留。
    const UUID_SECRET: &str = "550e8400-e29b-41d4-a716-446655440000";
    const MACHINE_UUID: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

    struct MachineFieldTool;

    #[async_trait]
    impl Tool for MachineFieldTool {
        fn name(&self) -> &str {
            "machine-field"
        }
        fn description(&self) -> &str {
            "machine field stub"
        }
        async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
            Ok("ok".into())
        }
        fn snapshot(&self) -> Option<Value> {
            Some(serde_json::json!({
                "id": MACHINE_UUID,
                "session": "local:tui::local_user",
                "epoch": MACHINE_UUID,
                // 非机器键：值恰好是 UUID 形态的秘密 → 必须脱敏。
                "note": UUID_SECRET,
                "items": [{
                    "id": MACHINE_UUID,
                    "text": format!("rotate {UUID_SECRET}"),
                }],
            }))
        }
        fn snapshot_for_session(&self, _session_id: &str) -> Option<Value> {
            self.snapshot()
        }
        fn snapshot_machine_fields(&self) -> &'static [&'static str] {
            &["id", "list_id", "item_id", "epoch", "session", "session_id"]
        }
    }

    /// 递归断言：快照的字符串**值**里不得再出现秘密（对象键允许保留）。
    fn assert_no_secret_in_values(value: &Value) {
        match value {
            Value::String(text) => assert!(
                !text.contains(SNAPSHOT_SECRET),
                "snapshot leaked secret: {text:?}"
            ),
            Value::Array(items) => items.iter().for_each(assert_no_secret_in_values),
            Value::Object(map) => map.values().for_each(assert_no_secret_in_values),
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    /// 全量快照：嵌套对象 / 数组里的字符串值全部脱敏；数字、布尔、null
    /// 保持原样；对象键不改写（否则面板按键取字段会失效）。
    #[test]
    fn snapshot_redacts_full_view_recursively_and_keeps_schema() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(StatefulTool));
        let redactor = Arc::new(TestRedactor::new());
        redactor.register_secret(SNAPSHOT_SECRET, "api_key:test");
        registry.set_redactor(Some(redactor));

        let snapshot = registry.snapshot("stateful").expect("snapshot");
        assert_no_secret_in_values(&snapshot);
        assert_eq!(
            snapshot["text"],
            Value::String("before 【已隐藏:api_key:test】 after".into())
        );
        assert_eq!(
            snapshot["quoted"],
            Value::String("line1 \"【已隐藏:api_key:test】\"\nline2".into()),
            "引号与换行必须原样保留，只替换秘密本身"
        );
        assert_eq!(
            snapshot["nested"]["token"],
            Value::String("【已隐藏:api_key:test】".into())
        );
        assert_eq!(snapshot["nested"]["count"], Value::from(3));
        assert_eq!(snapshot["nested"]["ok"], Value::Bool(true));
        assert_eq!(snapshot["nested"]["none"], Value::Null);
        assert_eq!(
            snapshot["items"][0],
            Value::String("【已隐藏:api_key:test】".into())
        );
        assert_eq!(snapshot["items"][1], Value::from(7));
        assert_eq!(
            snapshot["items"][2]["deep"],
            Value::String("【已隐藏:api_key:test】".into())
        );
        // 对象键不改写：秘密形态的键仍在，其值也未被动过。
        assert_eq!(
            snapshot[SNAPSHOT_SECRET],
            Value::String("value-kept".into()),
            "对象键是 schema，不得替换"
        );
        assert!(snapshot.get("【已隐藏:api_key:test】").is_none());
    }

    /// schema 感知脱敏：UUID 形态的**秘密文本**仍被脱敏，而工具声明的
    /// 机器字段（id / epoch / session）原样保留。
    ///
    /// 反向约束：不能按「值长得像 UUID」全局跳过——那样同形态秘密文本
    /// 会漏出去；只有**键名**命中声明才豁免。
    #[test]
    fn snapshot_redacts_uuid_shaped_secret_text_but_keeps_machine_ids() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(MachineFieldTool));
        let redactor = Arc::new(TestRedactor::new());
        redactor.register_secret(UUID_SECRET, "uuid-secret");
        registry.set_redactor(Some(redactor));

        let snapshot = registry.snapshot("machine-field").expect("snapshot");
        assert_eq!(snapshot["id"], MACHINE_UUID);
        assert_eq!(snapshot["session"], "local:tui::local_user");
        assert_eq!(snapshot["epoch"], MACHINE_UUID);
        assert_eq!(snapshot["items"][0]["id"], MACHINE_UUID);
        assert_eq!(snapshot["note"], "【已隐藏:uuid-secret】");
        assert_eq!(
            snapshot["items"][0]["text"],
            "rotate 【已隐藏:uuid-secret】"
        );
        assert!(
            !snapshot.to_string().contains(UUID_SECRET),
            "UUID 形态的秘密文本不得因形状被跳过"
        );
    }

    /// 按会话快照：同样脱敏，且只含请求会话的数据（不跨会话合并）。
    #[test]
    fn snapshot_for_session_redacts_and_keeps_sessions_isolated() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(StatefulTool));
        let redactor = Arc::new(TestRedactor::new());
        redactor.register_secret(SNAPSHOT_SECRET, "api_key:test");
        registry.set_redactor(Some(redactor));

        let a = registry
            .snapshot_for_session("stateful", "session-a")
            .expect("session-a snapshot");
        assert_no_secret_in_values(&a);
        assert_eq!(a["session"], "session-a");
        assert_eq!(a["items"][0]["text"], "session-a:【已隐藏:api_key:test】");
        assert_eq!(a["items"][0]["done"], Value::Bool(false));

        let b = registry
            .snapshot_for_session("stateful", "session-b")
            .expect("session-b snapshot");
        assert_no_secret_in_values(&b);
        assert_eq!(b["session"], "session-b");
        let b_text = b["items"][0]["text"].as_str().expect("text");
        assert!(b_text.starts_with("session-b:"));
        assert!(!b_text.contains("session-a"), "不得混入其他会话的数据");
        assert!(
            registry.snapshot_for_session("stateful", "nope").is_none(),
            "无该会话状态时保持 None（不发空事件）"
        );
    }

    /// 未注入脱敏器：快照原样透传（含秘密与全部键；旧行为）。
    #[test]
    fn snapshot_passes_through_without_redactor() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(StatefulTool));
        registry.set_redactor(None);

        let snapshot = registry.snapshot("stateful").expect("snapshot");
        assert_eq!(
            snapshot["text"],
            Value::String(format!("before {SNAPSHOT_SECRET} after"))
        );
        assert_eq!(snapshot["nested"]["count"], Value::from(3));
        assert_eq!(
            snapshot[SNAPSHOT_SECRET], "value-kept",
            "未注入：键与值都原样"
        );
        let a = registry
            .snapshot_for_session("stateful", "session-a")
            .expect("session snapshot");
        assert_eq!(
            a["items"][0]["text"],
            format!("session-a:{SNAPSHOT_SECRET}")
        );
    }

    /// 脱敏器锁竞争不得被静默当成"没有脱敏器"：std 读锁会等写锁释放，
    /// 拿到后照常脱敏。
    #[test]
    fn snapshot_redacts_when_redactor_lock_is_contended() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(StatefulTool));
        let redactor = Arc::new(TestRedactor::new());
        redactor.register_secret(SNAPSHOT_SECRET, "api_key:test");
        registry.set_redactor(Some(redactor));

        // 主线程占住脱敏器写锁 → 工作线程的读必然阻塞。
        let guard = registry.redactor.write().expect("redactor lock");
        let worker = {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || registry.snapshot("stateful"))
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(guard);

        let snapshot = worker.join().expect("worker panicked").expect("snapshot");
        assert_no_secret_in_values(&snapshot);
        assert_eq!(
            snapshot["text"],
            Value::String("before 【已隐藏:api_key:test】 after".into())
        );
    }

    /// current_thread runtime 上的真实锁竞争回归。
    ///
    /// 旧实现 `try_read` 失败后调用 tokio 的 `blocking_read`，在
    /// current_thread runtime 上直接 panic（阻塞了驱动任务的线程）。std
    /// 锁的读只阻塞 OS 线程，写锁释放后照常返回快照。
    #[test]
    fn snapshot_redacts_under_current_thread_runtime_contention() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(StatefulTool));
        let redactor = Arc::new(TestRedactor::new());
        redactor.register_secret(SNAPSHOT_SECRET, "api_key:test");
        registry.set_redactor(Some(redactor));

        let guard = registry.redactor.write().expect("redactor lock");
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("current_thread runtime");
                let snapshot = runtime.block_on(async move { registry.snapshot("stateful") });
                let _ = sender.send(snapshot);
            })
        };
        // 写锁仍被占着：工作线程只可能阻塞在 std 读锁上。
        // - Timeout = 正常竞争；
        // - Disconnected = 工作线程 panic（旧 blocking_read 回归）；
        // - Ok = 不应发生（写锁未释放，读不可能成功）。
        match receiver.recv_timeout(std::time::Duration::from_millis(150)) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Ok(snapshot) => panic!("写锁持有期间读锁不应成功: {snapshot:?}"),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("current_thread 快照线程在锁竞争下 panic（blocking_read 回归）")
            }
        }
        drop(guard);

        let snapshot = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("释放写锁后应拿到快照")
            .expect("snapshot");
        worker.join().expect("worker panicked");
        assert_no_secret_in_values(&snapshot);
        assert_eq!(
            snapshot["text"],
            Value::String("before 【已隐藏:api_key:test】 after".into())
        );
    }

    /// 脱敏器锁中毒不得被当成"没有脱敏器"而透传：取回最后注入的
    /// redactor，快照照常脱敏；中毒后重新注入也必须生效。
    #[test]
    fn snapshot_recovers_poisoned_redactor_slot_without_leaking() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(StatefulTool));
        let redactor = Arc::new(TestRedactor::new());
        redactor.register_secret(SNAPSHOT_SECRET, "api_key:test");
        registry.set_redactor(Some(redactor));

        let poisoner = {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                let _guard = registry.redactor.write().expect("redactor lock");
                panic!("poison the redactor slot");
            })
        };
        assert!(poisoner.join().is_err(), "测试前提：持锁线程必须 panic");
        assert!(registry.redactor.is_poisoned(), "测试前提：锁已中毒");

        let snapshot = registry.snapshot("stateful").expect("snapshot");
        assert_no_secret_in_values(&snapshot);
        assert_eq!(
            snapshot["text"],
            Value::String("before 【已隐藏:api_key:test】 after".into()),
            "中毒不能退化成透传"
        );

        // 中毒后重新注入：新 redactor 必须生效（不能被 poison 挡住）。
        let fresh = Arc::new(TestRedactor::new());
        fresh.register_secret(SNAPSHOT_SECRET, "fresh");
        registry.set_redactor(Some(fresh));
        let snapshot = registry
            .snapshot("stateful")
            .expect("snapshot after re-inject");
        let text = snapshot["text"].as_str().expect("text");
        assert!(!text.contains(SNAPSHOT_SECRET), "重新注入后仍泄漏: {text}");
        assert!(
            text.contains("【已隐藏:fresh】"),
            "新 redactor 未生效: {text}"
        );
    }

    /// 工具表锁竞争同样走 blocking 读路径，快照仍照常脱敏。
    #[test]
    fn snapshot_redacts_when_tools_lock_is_contended() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(StatefulTool));
        let redactor = Arc::new(TestRedactor::new());
        redactor.register_secret(SNAPSHOT_SECRET, "api_key:test");
        registry.set_redactor(Some(redactor));

        let guard = registry.tools.blocking_write();
        let worker = {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || registry.snapshot_for_session("stateful", "session-a"))
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(guard);

        let snapshot = worker.join().expect("worker panicked").expect("snapshot");
        assert_no_secret_in_values(&snapshot);
        assert_eq!(
            snapshot["items"][0]["text"],
            "session-a:【已隐藏:api_key:test】"
        );
    }

    /// 反向注册的 Disposer 按**名**注销——同名重注册必须先删旧再插新
    /// （联邦远程代理工具保活句柄「先删后插」模式的行为契约；2026-10
    /// 修复：旧句柄若在重注册之后才 drop，会把同名的**新**条目一并顶掉，
    /// 多 peer 连续重连后远程工具全部消失）。
    #[tokio::test]
    async fn drop_before_reregister_keeps_same_name_alive() {
        let registry: Arc<ToolRegistry> = Arc::new(ToolRegistry::new());
        // 首次注册 + 保活句柄。
        let d1 = registry.register_reversible(Arc::new(StubTool {
            name: "A100-40g__bash",
        }));
        assert_eq!(registry.names(), vec!["A100-40g__bash"]);
        // 重注册（对端重连）：先删旧（drop d1）再插新——条目必须存活。
        drop(d1);
        let _d2 = registry.register_reversible(Arc::new(StubTool {
            name: "A100-40g__bash",
        }));
        assert_eq!(
            registry.names(),
            vec!["A100-40g__bash"],
            "先删后插：重注册条目必须存活"
        );
        let defs = registry.definitions().await;
        assert!(defs.iter().any(|d| d.name == "A100-40g__bash"));
    }

    /// 重名不是「无所谓」：后者静默顶掉前者，必须留痕（否则模块能无声劫持
    /// 内置工具——外部模块自报的同名工具与内置 `calculator` / `web_search`
    /// 同名，正是这个坑）。
    #[tokio::test]
    async fn duplicate_registration_is_recorded_as_conflict() {
        let mut registry = ToolRegistry::new();
        assert!(registry.name_conflicts().is_empty());

        registry.register(Arc::new(StubTool { name: "calculator" }));
        assert!(registry.name_conflicts().is_empty(), "首次注册不算冲突");
        // 同名再来一次 = 覆盖
        registry.register(Arc::new(StubTool { name: "calculator" }));
        let conflicts = registry.name_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts.get("calculator").map(String::as_str),
            Some("register")
        );
    }

    /// 可逆注册（模块挂载路径）同样记录。
    #[tokio::test]
    async fn reversible_duplicate_is_recorded_as_conflict() {
        let registry = Arc::new(ToolRegistry::new());
        let first = registry.register_reversible(Arc::new(StubTool { name: "web_search" }));
        let _second = registry.register_reversible(Arc::new(StubTool { name: "web_search" }));
        let conflicts = registry.name_conflicts();
        assert_eq!(
            conflicts.get("web_search").map(String::as_str),
            Some("register_reversible")
        );
        drop(first);
    }

    /// 不同名字永不产生冲突记录。
    #[tokio::test]
    async fn distinct_names_never_conflict() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubTool { name: "a" }));
        registry.register(Arc::new(StubTool { name: "b" }));
        assert!(registry.name_conflicts().is_empty());
    }
}
