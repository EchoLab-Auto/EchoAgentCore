//! # echo-tool — 工具子系统机制（解耦计划 Phase 3 前置）
//!
//! `Tool` 词汇与接缝在定义层 [`echo_defs::tool`]；本 crate 只持有**运行期
//! 机制** [`ToolRegistry`]——原 `echo-agent::packages::tool` 的独立 crate 化，
//! 使工具类插件（`tools_builtin` / `instance_ops` / `subagent` / 远程插件）
//! 能只依赖机制层注册工具，而不依赖 agent 框架。
//!
//! 三条不变量：
//!
//! 1. **注册可逆**：[`ToolRegistry::register_reversible`] 返回 `Disposer`，
//!    撤销时移除工具并失效定义缓存（dsh「注册即副作用」）；
//! 2. **定义缓存**：模型可见的工具清单缓存为 `Arc<Vec<ToolDefinition>>`，
//!    任何注册变更即失效——避免每次组 prompt 重算；
//! 3. **出口脱敏**：注入 `Redactor` 后，全部执行出口（`execute` /
//!    `execute_rich`）的结果文本统一过卡口。
//!
//! **依赖纪律**：只依赖 `echo-defs` + `echo-context` + tokio——不认识任何
//! 具体工具实现（内置工具集在 `echo-agent` 的 `packages/tools_builtin`）。

pub use echo_defs::tool::{Tool, ToolDefinition, ToolError, ToolResult};

pub mod execute;
pub use execute::{
    invalid_tool_arguments, tool_state, ToolCallContext, ToolExecutionHost, ToolExecutor,
    ToolInvocation,
};

use std::collections::HashMap;
use std::sync::Arc;

use echo_context::Disposer;
use serde_json::Value;

/// 阻塞段防护：`tokio::task::block_in_place` 在 current_thread runtime 上
/// 直接 panic。multi_thread runtime 走 `block_in_place`（把执行线程让回
/// 调度器）；current_thread runtime 直接就地执行闭包——这些调用点本就处于
/// 同步/阻塞上下文（组装期、插件 mount/unmount），可接受，附 warn 以便定位；
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
/// 一个**外部插件**工具：工具实例 + 插件自报的包 id。
///
/// 为什么把 `package` 单独带出来：包 id 是插件贡献的元数据（
/// `ToolContribution.package`），不在 `Tool` trait 上——插件工具是
/// `RemoteTool`，注册进注册表时要一并记下它属于哪个包（前端按包分组勾选）。
pub struct PluginTool {
    pub tool: Arc<dyn Tool>,
    pub package: Option<String>,
}

impl PluginTool {
    pub fn new(tool: Arc<dyn Tool>, package: Option<String>) -> Self {
        Self { tool, package }
    }
}

/// [`ToolRegistry::register_plugin_tools`] 的逐条结果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PluginToolRegistration {
    /// 成功注册的工具名（按输入顺序）。
    pub registered: Vec<String>,
    /// 因与宿主保留名重名而跳过的插件工具：`(名字, 插件自报的包 id)`。
    pub skipped: Vec<(String, Option<String>)>,
}

impl PluginToolRegistration {
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
    /// 为什么记：`insert` 是静默覆盖——一个插件和一个内置工具叫同一个名字
    /// 时，后者会无声无息地顶掉前者（实测风险：`echo-plugin-example` 的
    /// `calculator` / `web_search` 与内置工具同名）。重名本身**是事件**，
    /// 必须留痕：组合根装配完读一次，Panel 详情页也可展示。
    conflicts: tokio::sync::RwLock<std::collections::BTreeMap<String, String>>,
    /// 结果脱敏器（安全服务注入；`None` = 透传）。
    /// 出口统一处理：所有工具（内置 / 插件 / 远程 / 子代理直呼 / 联邦）
    /// 的结果文本经此处脱敏后才交给任何消费者（2026-10 敏感信息隔离）。
    redactor: tokio::sync::RwLock<Option<Arc<dyn echo_defs::sanitize::Redactor>>>,
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
            redactor: tokio::sync::RwLock::new(None),
        }
    }

    /// 注入结果脱敏器（组合根装配期调用；`None` = 关闭）。
    ///
    /// 生效范围：本注册表的**全部**执行出口（`execute` / `execute_rich`）。
    /// 装配期无竞争（`try_write` 直通）；意外竞争时退到阻塞段落。
    pub fn set_redactor(&self, redactor: Option<Arc<dyn echo_defs::sanitize::Redactor>>) {
        match self.redactor.try_write() {
            Ok(mut slot) => *slot = redactor,
            Err(_) => blocking_section(|| {
                *self.redactor.blocking_write() = redactor;
            }),
        }
    }

    /// 当前脱敏器（未注入 = None）。
    async fn redactor(&self) -> Option<Arc<dyn echo_defs::sanitize::Redactor>> {
        self.redactor.read().await.clone()
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
             若是内置工具与插件工具撞名，请改名其一；插件作者见\
             document/plugins/plugin-authoring.md"
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
    /// Works from both sync (assembly) and async (plugin mount) contexts:
    /// inside a runtime the lock is taken without blocking the executor.
    /// Register a tool reversibly, returning a disposer that removes it and
    /// invalidates the definitions cache.
    ///
    /// Uncontended (assembly, test) paths take the lock directly; under
    /// contention inside a multi-thread runtime the lock is taken on the
    /// blocking pool. Disposal uses the same strategy.
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

    /// 是否已注册同名工具（宿主保留名判定）。
    ///
    /// 与 `names().contains(..)` 的区别：直接查 map，O(1) 而不是 O(注册数)——
    /// 插件工具注册是逐条调用的，用 `names()` 会变成 O(n²)。
    pub fn contains_tool(&self, name: &str) -> bool {
        match self.tools.try_read() {
            Ok(tools) => tools.contains_key(name),
            Err(_) => blocking_section(|| self.tools.blocking_read().contains_key(name)),
        }
    }

    /// 注册一批**外部插件**工具，返回逐条结果。
    ///
    /// ## 语义：宿主保留名优先
    ///
    /// [`Self::register`] 的 `insert` 是**覆盖**语义——插件工具与内置/宿主工具
    /// 同名时，插件实现会无声无息地顶掉内置实现（典型：外部插件声明
    /// `calculator` / `web_search`，与内置同名，表现为"功能还在但行为变了"，
    /// 排查成本极高）。
    ///
    /// 因此插件工具走这条路径：**已注册的名字是保留名**，同名插件项被跳过并
    /// 记入返回值的 `skipped`（调用方据此 warn 点名），内置实现保持不变。
    /// 插件仍可提供其它不冲突的工具——冲突只淘汰冲突项，不做整体拒绝。
    ///
    /// 包标签按插件自报的 `package` 打（`None` 表示插件未声明）。
    pub fn register_plugin_tools(
        &mut self,
        tools: impl IntoIterator<Item = PluginTool>,
    ) -> PluginToolRegistration {
        let mut report = PluginToolRegistration::default();
        for PluginTool { tool, package } in tools {
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
    /// 供 sync 闭包路径（插件 mount/unmount 的 persona 名单收紧）使用；
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

    /// All disabled tool names.
    pub async fn disabled_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.disabled.read().await.iter().cloned().collect();
        names.sort();
        names
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
    pub fn snapshot(&self, name: &str) -> Option<Value> {
        match self.tools.try_read() {
            Ok(tools) => tools.get(name).and_then(|tool| tool.snapshot()),
            Err(_) => blocking_section(|| {
                self.tools
                    .blocking_read()
                    .get(name)
                    .and_then(|tool| tool.snapshot())
            }),
        }
    }

    /// 按会话过滤的状态快照（2026-10，带会话维度的工具用；默认同
    /// [`Self::snapshot`]）。
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

    pub fn snapshot_for_session(&self, name: &str, session_id: &str) -> Option<Value> {
        match self.tools.try_read() {
            Ok(tools) => tools
                .get(name)
                .and_then(|tool| tool.snapshot_for_session(session_id)),
            Err(_) => blocking_section(|| {
                self.tools
                    .blocking_read()
                    .get(name)
                    .and_then(|tool| tool.snapshot_for_session(session_id))
            }),
        }
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

    /// Execute a tool with multimodal output support (text + images).
    pub async fn execute_rich(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<echo_defs::tool::ToolResult, ToolError> {
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
        let Some(redactor) = self.redactor().await else {
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
        result: Result<echo_defs::tool::ToolResult, ToolError>,
    ) -> Result<echo_defs::tool::ToolResult, ToolError> {
        let Some(redactor) = self.redactor().await else {
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
    /// Bulk enable/disable every tool owned by a package (package id = 插件 id，
    /// 装配时打标）。同步接口：插件 mount/unmount 闭包是同步上下文，锁竞争
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
fn redact_error(redactor: &dyn echo_defs::sanitize::Redactor, error: ToolError) -> ToolError {
    fn redact_inner(redactor: &dyn echo_defs::sanitize::Redactor, inner: String) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use echo_defs::sanitize::Redactor;

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
    /// 背景：`register` 的 `insert` 是覆盖语义。内置工具外迁为插件后，
    /// 外部插件声明的 `calculator` / `web_search` 与内置同名，于是**内置实现
    /// 被悄悄顶掉**——线上表现是"功能还在但行为变了"，五条 ERROR 日志。
    ///
    /// 契约：
    /// 1. 与已注册工具同名的插件工具被**跳过**（内置实现保留）；
    /// 2. 非冲突的插件工具照常注册，并按插件自报的 package 打标；
    /// 3. 报告逐条列出 registered / skipped，调用方据此 warn 点名。
    #[tokio::test]
    async fn plugin_tools_never_override_host_reserved_names() {
        let mut registry = ToolRegistry::new();
        // 宿主（内置）实现
        registry.register(Arc::new(TaggedTool {
            name: "calculator",
            output: "builtin",
        }));
        registry.set_package("calculator", "echo-agent.tools.builtin");

        let report = registry.register_plugin_tools(vec![
            // 冲突项：与内置同名
            PluginTool::new(
                Arc::new(TaggedTool {
                    name: "calculator",
                    output: "plugin",
                }),
                Some("echo-plugin-example".into()),
            ),
            // 非冲突项
            PluginTool::new(
                Arc::new(TaggedTool {
                    name: "plugin_only",
                    output: "plugin",
                }),
                Some("echo-plugin-example".into()),
            ),
        ]);

        assert_eq!(report.registered, vec!["plugin_only"]);
        assert_eq!(
            report.skipped,
            vec![(
                "calculator".to_string(),
                Some("echo-plugin-example".to_string())
            )]
        );
        assert!(report.has_skips());

        // 内置实现仍然生效（关键断言：不是"功能消失"，也不是"被换掉"）
        assert_eq!(
            registry.execute("calculator", Value::Null).await.unwrap(),
            "builtin"
        );
        // 包标签没被插件覆盖
        assert_eq!(
            registry.package_of("calculator").as_deref(),
            Some("echo-agent.tools.builtin")
        );
        // 非冲突项按插件自报的包打标
        assert_eq!(
            registry.package_of("plugin_only").as_deref(),
            Some("echo-plugin-example")
        );
        // 冲突**不**记进 name_conflicts（没有发生覆盖）
        assert!(registry.name_conflicts().is_empty());
    }

    /// 插件之间同名：先注册者生效，后到者跳过（与宿主保留名同一规则）。
    #[tokio::test]
    async fn duplicate_plugin_tool_names_keep_the_first() {
        let mut registry = ToolRegistry::new();
        let report = registry.register_plugin_tools(vec![
            PluginTool::new(
                Arc::new(TaggedTool {
                    name: "dup",
                    output: "first",
                }),
                Some("pkg-a".into()),
            ),
            PluginTool::new(
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
            "后到的插件不得覆盖先注册的实现"
        );
        assert_eq!(registry.package_of("dup").as_deref(), Some("pkg-a"));
    }

    /// 空输入是 no-op（组合根在无插件时也会调用）。
    #[tokio::test]
    async fn register_plugin_tools_accepts_empty_input() {
        let mut registry = ToolRegistry::new();
        let report = registry.register_plugin_tools(Vec::new());
        assert!(report.registered.is_empty());
        assert!(!report.has_skips());
        assert!(registry.names().is_empty());
    }

    /// 插件未声明 package 时也能注册（`None` 不打标，不影响可用性）。
    #[tokio::test]
    async fn plugin_tool_without_package_is_registered_without_label() {
        let mut registry = ToolRegistry::new();
        let report = registry.register_plugin_tools(vec![PluginTool::new(
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

        assert!(registry.set_enabled("stub", false).await);
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
        let redactor = Arc::new(echo_sanitize::RegistryRedactor::with_builtin_patterns());
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
        let redactor = Arc::new(echo_sanitize::RegistryRedactor::new());
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

    /// 重名不是「无所谓」：后者静默顶掉前者，必须留痕（否则插件能无声劫持
    /// 内置工具——`echo-plugin-example` 与内置 `calculator` / `web_search`
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

    /// 可逆注册（插件挂载路径）同样记录。
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
