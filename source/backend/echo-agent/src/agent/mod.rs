//! The agent loop: LLM + tools + skills + memory composed into one reply.

mod api_admin;
mod boundary;
pub mod builder;
mod commands;
mod compact;
mod gating;
mod prompt;
mod subagent_runtime;
mod tool_exec;
// `qq_commands` / `workspace_commands` 已按包归位：
// 见 `crate::packages::adapter_qq::commands` / `crate::packages::workspace::commands`。

use boundary::{BoundaryKind, PromptBlock};

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// 进程级事件汇聚点：任意 persona 的 `emit` 都投递到此，组合根保证
/// 所有人格（含运行期新建）共享同一汇聚点，Panel 单连接即可看到全部活动。
pub type EventSink = std::sync::Arc<dyn Fn(BackendEvent) + Send + Sync>;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use tokio::sync::{Mutex, RwLock};

use echo_adapter::AdapterRegistry;

use crate::bridge::BackendHandle;
use crate::command::BackendCommand;
use crate::config::{AgentConfig, ApiProfile};
use crate::event::{AdapterStatus, BackendEvent};
use crate::llm::{create_provider, ChatChunk, ChatMessage, ChatRequest, LlmProvider, ToolCall};
use crate::session::{Session, TrunkStore};
use crate::skill::SkillRegistry;
use crate::tool::ToolRegistry;

pub(crate) const TURN_CANCELLED: &str = "agent turn cancelled by requester";
/// 截断续跑常量（口径单源 = `echo_defs::llm`，2026-10 巡检：与
/// echo-loop 双写已漂移过——统一后两处引用同一常量）。
pub(crate) use echo_defs::llm::{MAX_TRUNCATION_CONTINUES, TRUNCATION_CONTINUE_PROMPT};
/// Graceful-shutdown drain window: how long to wait for in-flight turns to
/// finish before force-cancelling (self-update interruption guard).
const SHUTDOWN_DRAIN_SECS: u64 = 120;
const MAX_CONCURRENT_REPLY_BRANCHES: usize = 8;
/// Upper bound on concurrent "wait reply" generation calls. Every long-running
/// QQ branch spawns a delayed interim reply; without a cap, many parallel
/// branches would each fire an extra LLM request at the same moment.
const MAX_CONCURRENT_WAIT_REPLIES: usize = 4;

/// 循环模式互斥插件 id（见 plugins.rs）：`loop.single`（单会话，默认）与
/// `loop.parallel`（并行多会话）。两者 mount 同一 TurnRunner，模式经
/// `TeamMember::loop_mode` 推导。
pub use crate::plugins::{PARALLEL_LOOP_PLUGIN_ID, SINGLE_LOOP_PLUGIN_ID};

#[derive(Debug, Clone)]
struct ActiveInboundTurn {
    session_id: String,
    message_sequence: u64,
    branch: bool,
    cancel: tokio_util::sync::CancellationToken,
}

pub(crate) struct InboundTurnRegistration {
    pub id: String,
    pub cancel: tokio_util::sync::CancellationToken,
}

impl Default for InboundTurnRegistration {
    /// 并行模式先取闸门再注册：占位值不进入注册表（`id` 为空时
    /// `finish_inbound_turn` 是 no-op，取消令牌不会被使用）。
    fn default() -> Self {
        Self {
            id: String::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }
}

/// The agent: owns provider, tools, skills, sessions, adapters, and emits events.
pub struct Agent {
    provider: RwLock<Arc<dyn LlmProvider>>,
    config: RwLock<AgentConfig>,
    active_model: RwLock<String>,
    /// Shared config store for persisting `[agent]` changes (optional).
    config_store: tokio::sync::Mutex<Option<echo_adapter::ConfigStore>>,
    pub tools: Arc<ToolRegistry>,
    pub skills: Arc<Mutex<SkillRegistry>>,
    pub trunk: TrunkStore,
    /// Platform adapters managed externally (registered by main / config).
    pub adapters: Arc<AdapterRegistry>,
    /// Tokio RwLock: `emit()` runs from async loops and must never block a
    /// worker thread. All access goes through `try_*` (never held across
    /// await points, contention is negligible).
    pub handle: tokio::sync::RwLock<Option<Arc<BackendHandle>>>,
    /// 进程级事件汇聚点（组合根注入）：所有 persona 的 `emit` 都投递到同一
    /// 汇聚点，Panel 单连接即可看到所有人格活动，无需求助"默认人格"镜像。
    /// 设置后优先于 `handle`（`handle` 保留给单 agent 测试与旧路径）。
    event_sink: std::sync::RwLock<Option<EventSink>>,
    /// 敏感信息脱敏器（组合根注入；None = 透传）。
    ///
    /// 覆盖：入站内容（`process_message` / `process_inbound_branch` /
    /// `record_incoming_and_snapshot`）、工具事件与会话落盘的参数、工具结果
    /// （经 `ToolRegistry` 出口与 `run_tool` 双保险）、LLM 请求出口（经
    /// `llm::wrap_redacting` 装饰 provider，见组合根装配）、QQ 出站
    /// （`core::qq_tools` 闸门）。设计见 `document/security-redaction-design.md`。
    redactor: std::sync::RwLock<Option<Arc<dyn echo_defs::sanitize::Redactor>>>,
    /// API 指标存储（余额快照 / token 用量；组合根装配期注入）。
    /// None = 不记录（测试 / 未接线的旧路径）。
    metrics: std::sync::RwLock<Option<Arc<crate::metrics::MetricsStore>>>,
    system_prompt_cache: RwLock<Option<String>>,
    /// Decomposed system-prompt blocks of the most recent turn, kept so the
    /// panel can visualize the exact prompt sections sent to the LLM.
    last_prompt_blocks: tokio::sync::Mutex<Option<Vec<PromptBlock>>>,
    plugin_reload_started: AtomicBool,
    /// 可插拔循环驱动（echo-loop TurnRunner，由 `echo-agent.loop.{single,parallel}`
    /// 插件 mount 时注入）；启用（use_echo_loop）时普通 TUI turn 经 TurnRunner 的
    /// turn/step 状态机 + ToolPipeline 执行，否则使用内置循环（默认）。
    loop_runner: RwLock<Option<std::sync::Arc<echo_loop::runner::TurnRunner>>>,
    use_echo_loop: AtomicBool,
    reply_branch_slots: tokio::sync::Semaphore,
    /// Bounds concurrent interim "wait reply" LLM calls (see
    /// [`MAX_CONCURRENT_WAIT_REPLIES`]).
    wait_reply_slots: tokio::sync::Semaphore,
    active_inbound_turns: DashMap<String, ActiveInboundTurn>,
    next_message_sequence: AtomicU64,
    /// Event bus: `emit` broadcasts through it; consumers (the timeline
    /// projector, the frontend bridge) subscribe as listeners.
    pub event_bus: std::sync::Arc<echo_context::EventBus>,
    /// Keeps the timeline projector's bus subscription alive for the agent's
    /// lifetime; dropped (disposed) together with the agent.
    _timeline_projection: echo_context::Disposer,
    /// Cancels background tasks (identity eviction / save) on shutdown.
    cancel: tokio_util::sync::CancellationToken,
    /// 工作区会话存储（`echo-agent.workspace` 插件；组合根按 persona 注入）。
    /// None = 未挂载（命令会回明确错误）。
    workspace_store: std::sync::RwLock<Option<std::sync::Arc<crate::workspace::WorkspaceStore>>>,
    /// Plugin host: registry + mount context bridging `echo_plugin` to the
    /// agent's concrete registries. 组合根注入**同一个**进程级宿主
    /// （去主智能体 P1：插件宿主不再寄居在某个"默认人格"上）。
    plugin_host: std::sync::RwLock<std::sync::Arc<crate::plugins::PluginHost>>,
    /// Team id (None/empty = default). Sets session tagging after boot.
    team_id: std::sync::Mutex<Option<String>>,
    /// Graceful-drain flag: set during shutdown so new turns are rejected
    /// while in-flight replies finish (self-update continuity).
    draining: AtomicBool,
    capabilities: std::sync::Mutex<Option<crate::config::AgentProfile>>,
    /// Persona 级 API 供应商引用（None = 跟随全局默认配置；
    /// Some(name) = 全局供应商池 `[agent].api_profiles` 中该名字的 profile）。
    /// 保存于 `[agent.teams.{id}].api_profile`；运行期变更经由
    /// [`Self::apply_persona_api`] 重建本 persona 的 provider，不依赖全局
    /// UpdateApiConfig / SwitchApi 激活路径。
    persona_api: tokio::sync::RwLock<Option<String>>,
    /// Subagent 插件运行态：子任务注册表 + spawn 执行闭包（组合根装配；
    /// None = 插件未启用，spawn_subagent 对模型不可见）。
    subagent: std::sync::RwLock<Option<SubagentRuntime>>,
}

/// spawn 执行闭包类型（子任务拉起）。
pub(crate) type SubagentSpawnFn =
    std::sync::Arc<dyn Fn(crate::subagent::SpawnRequest) + Send + Sync>;

/// Subagent 插件注入 Agent 的运行态（见 [`crate::subagent`]）。
pub(crate) struct SubagentRuntime {
    pub store: std::sync::Arc<crate::subagent::SubagentStore>,
    pub spawn: SubagentSpawnFn,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("active_model", &self.active_model)
            .field("adapters", &self.adapters.names())
            .finish_non_exhaustive()
    }
}

/// Process-wide plugin host (best-effort): set once by the composition root.
/// Lets gating checks resolve the global registry from code that runs inside
/// the agent but outside a `&Agent` scope (see `plugin_globally_enabled`).
/// 存储收敛于 [`echo_context::kernel`]（P1 唯一引导单元）。
pub fn plugin_host_global() -> Option<std::sync::Arc<crate::plugins::PluginHost>> {
    echo_context::kernel::get::<std::sync::Arc<crate::plugins::PluginHost>>()
}

pub fn set_plugin_host_global(host: std::sync::Arc<crate::plugins::PluginHost>) {
    let _ = echo_context::kernel::set(host);
}

/// 进程级全局策略（`[agent]` 层的工具/技能启停）：全局开关不属于任何人格，
/// 组合根注入一次，所有 persona 重算门控时读取同一份事实。
#[derive(Debug, Default)]
pub struct GlobalPolicy {
    disabled_tools: std::sync::RwLock<Vec<String>>,
    disabled_skills: std::sync::RwLock<Vec<String>>,
}

impl GlobalPolicy {
    pub fn new(disabled_tools: Vec<String>, disabled_skills: Vec<String>) -> Self {
        Self {
            disabled_tools: std::sync::RwLock::new(disabled_tools),
            disabled_skills: std::sync::RwLock::new(disabled_skills),
        }
    }

    pub fn disabled_tools(&self) -> Vec<String> {
        self.disabled_tools
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn disabled_skills(&self) -> Vec<String> {
        self.disabled_skills
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// 全局启停某工具（enabled=false 记入禁用表）。
    pub fn set_tool_enabled(&self, name: &str, enabled: bool) {
        if let Ok(mut list) = self.disabled_tools.write() {
            list.retain(|n| n != name);
            if !enabled {
                list.push(name.to_string());
            }
        }
    }

    /// 全局启停某技能。
    pub fn set_skill_enabled(&self, name: &str, enabled: bool) {
        if let Ok(mut list) = self.disabled_skills.write() {
            list.retain(|n| n != name);
            if !enabled {
                list.push(name.to_string());
            }
        }
    }
}

pub fn global_policy() -> Option<std::sync::Arc<GlobalPolicy>> {
    echo_context::kernel::get::<std::sync::Arc<GlobalPolicy>>()
}

pub fn set_global_policy(policy: std::sync::Arc<GlobalPolicy>) {
    let _ = echo_context::kernel::set(policy);
}

// ── 联邦命令处理器注册表（federation Phase 4）──
//
// `SaveFederationPeer` 等命令的处理需要组合根持有的 Federation 句柄与
// ConfigStore——不在 Agent 能力面内。组合根装配期经
// [`set_federation_command_handler`] 注入；未注入时命令明确报错（联邦
// 关闭）。签名用 boxed future：handler 由组合根闭包提供，捕获其句柄。
/// 联邦命令处理器签名：接受 Agent 引用做同步分发，返回 `'static`
/// future（实现侧如需跨 await 使用 Agent 能力，应提取所需句柄——
/// emit 直接经引用同步调用即可，handler 本身可以就是同步实现
/// 包一层 ready future）。
pub type FederationCommandHandler = std::sync::Arc<
    dyn Fn(&Agent, echo_protocol::BackendCommand) -> FederationHandlerFuture + Send + Sync,
>;

pub type FederationHandlerFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>;

pub fn federation_command_handler() -> Option<FederationCommandHandler> {
    echo_context::kernel::get::<FederationCommandHandler>()
}

pub fn set_federation_command_handler(handler: FederationCommandHandler) {
    let _ = echo_context::kernel::set(handler);
}

impl Agent {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        config: AgentConfig,
        skills: SkillRegistry,
        tools: ToolRegistry,
        adapters: Arc<AdapterRegistry>,
    ) -> Self {
        // The agent owns exactly ONE conversation context: the trunk. Every
        // inbound message forks a temporary branch from a trunk snapshot and
        // merges back in append-only order (see `process_message`).
        let trunk = TrunkStore::new(config.effective_memory_limit_tokens());
        let eviction_trunk = trunk.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let eviction_cancel = cancel.clone();
        let event_bus: std::sync::Arc<echo_context::EventBus> =
            std::sync::Arc::new(echo_context::EventBus::default());
        // The display timeline is a projection of emitted events; subscribe
        // the projector as an observe listener so `emit` fans out to it. The
        // returned disposer must be held for the agent's lifetime, or the
        // subscription unwinds immediately.
        let timeline_projector: std::sync::Arc<crate::timeline::TimelineProjector> =
            std::sync::Arc::new(crate::timeline::TimelineProjector::new(trunk.clone()));
        let _timeline_projection = timeline_projector.subscribe(&event_bus);
        let agent = Self {
            provider: RwLock::new(provider),
            config: RwLock::new(config),
            active_model: RwLock::new(String::new()),
            config_store: tokio::sync::Mutex::new(None),
            tools: Arc::new(tools),
            skills: Arc::new(Mutex::new(skills)),
            trunk,
            adapters,
            handle: tokio::sync::RwLock::new(None),
            event_sink: std::sync::RwLock::new(None),
            redactor: std::sync::RwLock::new(None),
            metrics: std::sync::RwLock::new(None),
            system_prompt_cache: RwLock::new(None),
            last_prompt_blocks: tokio::sync::Mutex::new(None),
            plugin_reload_started: AtomicBool::new(false),
            loop_runner: RwLock::new(None),
            use_echo_loop: AtomicBool::new(false),
            reply_branch_slots: tokio::sync::Semaphore::new(MAX_CONCURRENT_REPLY_BRANCHES),
            wait_reply_slots: tokio::sync::Semaphore::new(MAX_CONCURRENT_WAIT_REPLIES),
            active_inbound_turns: DashMap::new(),
            next_message_sequence: AtomicU64::new(1),
            event_bus,
            _timeline_projection,
            cancel,
            plugin_host: std::sync::RwLock::new(std::sync::Arc::new(
                crate::plugins::PluginHost::new(),
            )),
            workspace_store: std::sync::RwLock::new(None),
            team_id: std::sync::Mutex::new(None),
            draining: AtomicBool::new(false),
            capabilities: std::sync::Mutex::new(None),
            persona_api: tokio::sync::RwLock::new(None),
            subagent: std::sync::RwLock::new(None),
        };
        // Spawn periodic identity eviction (every 5 minutes). Only stale
        // source labels are dropped; the trunk history is never evicted.
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                tokio::select! {
                    _ = interval.tick() => eviction_trunk.evict_idle(),
                    _ = eviction_cancel.cancelled() => break,
                }
            }
        });
        agent
    }

    /// Graceful shutdown: stop background tasks and flush sessions to disk.
    pub async fn shutdown(&self) {
        // Graceful drain: reject new work, let in-flight turns finish so a
        // self-update restart does not cut off an ongoing reply.
        self.draining.store(true, Ordering::Release);
        self.emit(BackendEvent::Error {
            session_id: None,
            message: "Core 正在重启：等待当前回复完成…".into(),
        });
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(SHUTDOWN_DRAIN_SECS);
        while !self.active_inbound_turns.is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        self.cancel_all_inbound_turns();
        if let Ok(guard) = self.subagent.read() {
            if let Some(runtime) = guard.as_ref() {
                runtime.store.cancel_all();
            }
        }
        self.cancel.cancel();
        self.trunk.save_now().await;
    }

    /// Set the TOML config file that `[agent]` changes are persisted to.
    ///
    /// 仅设置配置持久化路径（`ConfigStore::patch` 读写的 TOML）；
    /// 会话（trunk）持久化路径必须用 [`Self::set_session_persist_path`]
    /// 单独设置——两者文件格式不同（TOML vs JSON 事件溯源），
    /// 绝不可共用同一路径，否则 patch 读 JSON 会 TOML 解析失败。
    pub fn set_config_path(&self, path: impl Into<std::path::PathBuf>) {
        let p: std::path::PathBuf = path.into();
        self.set_config_store(echo_adapter::ConfigStore::new(p));
    }

    /// Attach a shared [`echo_adapter::ConfigStore`] for `[agent]` persistence.
    ///
    /// 只接管 TOML 配置持久化，不动会话路径（会话路径由
    /// [`Self::set_session_persist_path`] 独立设置）。Called once at startup —
    /// uses try_lock since no concurrent access exists yet.
    pub fn set_config_store(&self, store: echo_adapter::ConfigStore) {
        if let Ok(mut slot) = self.config_store.try_lock() {
            *slot = Some(store);
        } else {
            tracing::warn!("config store slot busy, ignoring set_config_store");
        }
    }

    /// 注入脱敏器（组合根装配期；`AgentBuilder::redactor` 调用）。
    ///
    /// 只写一次（装配期无并发）；重复调用以最后一次为准。
    pub(crate) fn set_redactor(&self, redactor: Arc<dyn echo_defs::sanitize::Redactor>) {
        if let Ok(mut slot) = self.redactor.write() {
            *slot = Some(redactor);
        } else {
            tracing::warn!("redactor slot poisoned, ignoring set_redactor");
        }
    }

    /// 当前脱敏器（未注入 / 锁中毒 = None，全部脱敏路径按透传处理）。
    pub(crate) fn redactor(&self) -> Option<Arc<dyn echo_defs::sanitize::Redactor>> {
        self.redactor.read().ok().and_then(|slot| slot.clone())
    }

    /// 注入指标存储（组合根装配期；`AgentBuilder::metrics` 调用）。
    ///
    /// 只写一次（装配期无并发）；重复调用以最后一次为准。
    pub(crate) fn set_metrics(&self, store: Arc<crate::metrics::MetricsStore>) {
        if let Ok(mut slot) = self.metrics.write() {
            *slot = Some(store);
        } else {
            tracing::warn!("metrics slot poisoned, ignoring set_metrics");
        }
    }

    /// 当前指标存储（未注入 / 锁中毒 = None，记录与查询路径按跳过处理）。
    pub(crate) fn metrics(&self) -> Option<Arc<crate::metrics::MetricsStore>> {
        self.metrics.read().ok().and_then(|slot| slot.clone())
    }

    /// 文本脱敏：无脱敏器或未命中时返回原文副本。
    ///
    /// 性能：脱敏器内部为 Aho-Corasick 单遍扫描；未注入时仅一次锁读。
    pub(crate) fn redact_text(&self, text: &str) -> String {
        match self.redactor() {
            Some(redactor) => redactor.redact(text).text,
            None => text.to_string(),
        }
    }

    /// 带命中计数的脱敏（审计日志用）：返回（脱敏后文本，命中数）。
    pub(crate) fn redact_text_tracked(&self, text: &str) -> (String, usize) {
        match self.redactor() {
            Some(redactor) => {
                let report = redactor.redact(text);
                let count = report.hits.len();
                (report.text, count)
            }
            None => (text.to_string(), 0),
        }
    }

    /// Set the session (trunk) persistence path.
    ///
    /// 组合根按 persona 构造 `echo-sessions-{id}.json`（default 沿用
    /// `echo-sessions.json`）；与配置 TOML 分开，避免 JSON/TOML 互写。
    pub fn set_session_persist_path(&self, path: impl Into<std::path::PathBuf>) {
        self.trunk.set_persist_path(path);
    }

    /// Load persisted sessions from disk.
    pub async fn load_sessions(&self) -> usize {
        self.trunk.load_from_file().await
    }

    /// Spawn a periodic session save task.
    pub fn start_session_save_task(self: &Arc<Self>) {
        let agent = Arc::clone(self);
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = interval.tick() => agent.trunk.maybe_save().await,
                    _ = cancel.cancelled() => break,
                }
            }
        });
    }

    /// Watch the configured plugin directory for `plugin.toml` manifests and
    /// hot-(re)mount discovered data plugins (skills/tools). Code plugins
    /// (provider/loop/adapter) still require the binary-reload path: this
    /// watcher only handles data plugins.
    pub async fn start_plugin_reload_task(self: &Arc<Self>) {
        let plugins_dir = self.config.read().await.plugins_dir.clone();
        if plugins_dir.trim().is_empty()
            || !std::path::Path::new(&plugins_dir).exists()
            || self
                .plugin_reload_started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let agent = Arc::clone(self);
        let cancel = self.cancel.clone();
        tracing::info!(path = %plugins_dir, interval_seconds = 5, "plugin hot reload started");
        tokio::spawn(async move {
            let start = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut interval = tokio::time::interval_at(start, std::time::Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let _ = agent.reload_data_plugins(&plugins_dir).await;
                    }
                    _ = cancel.cancelled() => break,
                }
            }
        });
    }

    /// Scan `plugins_dir` for `plugin.toml` files and hot-mount/unmount data
    /// plugins based on manifest changes (simple: unmount all discovered,
    /// remount from current disk state).
    async fn reload_data_plugins(&self, plugins_dir: &str) -> Result<(), String> {
        let dir = std::path::Path::new(plugins_dir);
        if !dir.exists() {
            return Ok(());
        }
        let mut manifests = Vec::new();
        for entry in walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if path.is_file() && path.file_name().and_then(|n| n.to_str()) == Some("plugin.toml") {
                if let Ok(manifest) = echo_plugin::manifest::load_manifest(path) {
                    manifests.push(manifest);
                }
            }
        }
        let id_set: std::collections::HashSet<String> =
            manifests.iter().map(|m| m.id.clone()).collect();
        let host = &self.plugin_host();
        // Unmount discovered plugins that no longer exist.
        for desc in host.descriptors() {
            if !desc.builtin && desc.enabled && !id_set.contains(&desc.id) {
                let _ = host.registry.unmount(&desc.id);
            }
        }
        // Mount newly seen data plugins (skill/tool kinds); manifests whose
        // content changed since the last scan are remounted (diff reload).
        for manifest in manifests {
            let kind = manifest.kind;
            if !matches!(
                kind,
                echo_plugin::PluginKind::Skill | echo_plugin::PluginKind::Tool
            ) {
                continue; // code plugins need binary reload
            }
            if desc_exists(host, &manifest.id) {
                // 已挂载：manifest 内容 diff → 重挂载（unmount + 重注册 + mount）。
                // 此前改 plugin.toml 不会有任何效果（除非删掉再放回），面板
                // 上显示的版本/描述/包归属也一直停留在首次挂载的快照。
                let current = host.descriptors().into_iter().find(|d| d.id == manifest.id);
                let stale = current.is_some_and(|d| data_plugin_stale(&d, &manifest));
                if !stale {
                    continue;
                }
                tracing::info!(id = %manifest.id, "data plugin manifest changed, remounting");
                let _ = host.registry.unmount(&manifest.id);
                let _ = host.registry.unregister(&manifest.id);
            }
            let manifest2 = manifest.clone();
            let plugin =
                std::sync::Arc::new(echo_plugin::BuiltinPlugin::new(manifest, move |_ctx| {
                    Ok(vec![])
                }));
            let _ = &manifest2;
            if let Err(e) = host.register_and_mount(plugin) {
                tracing::warn!(%e, "discovered plugin mount failed");
            }
        }
        Ok(())
    }

    /// 热重载技能：合并发现「出厂层（skills_dirs）+ 用户层（skills_dir，
    /// 同名覆盖）」。传入的 `write_dir` 仅用于判断用户层是否存在（不存在
    /// 只意味着没有用户技能，出厂层仍照常加载）。
    pub(crate) async fn reload_skills(&self, write_dir: &str) -> Result<bool, String> {
        let mut dirs = self.config.read().await.skills_dirs.clone();
        dirs.push(write_dir.to_string());
        if dirs
            .iter()
            .all(|d| d.is_empty() || !std::path::Path::new(d).exists())
        {
            return Ok(false);
        }

        let mut reloaded = tokio::task::spawn_blocking(move || SkillRegistry::discover_many(&dirs))
            .await
            .map_err(|error| format!("skill reload task failed: {error}"))?
            .map_err(|error| error.to_string())?;

        let mut current = self.skills.lock().await;
        reloaded.inherit_enabled_from(&current);
        if *current == reloaded {
            return Ok(false);
        }

        let names = reloaded.names();
        *current = reloaded;
        drop(current);
        *self.system_prompt_cache.write().await = None;
        tracing::info!(skills = ?names, user_dir = %write_dir, "skills hot reloaded");
        Ok(true)
    }

    /// Assign the team id and tag the trunk (sessions created later carry
    /// it in SessionInfo). Call once at boot before any message arrives.
    /// 注入可插拔循环驱动（echo-loop TurnRunner；由循环模式插件 mount 调用）。
    pub fn set_loop_runner(&self, runner: std::sync::Arc<echo_loop::runner::TurnRunner>) {
        if let Ok(mut slot) = self.loop_runner.try_write() {
            *slot = Some(runner);
        } else {
            tracing::warn!("loop runner slot busy, ignoring set_loop_runner");
        }
    }

    /// 是否启用 echo-loop 驱动（plugin mount/unmount 切换；默认 false = 内置循环）。
    pub fn set_use_echo_loop(&self, enabled: bool) {
        self.use_echo_loop.store(enabled, Ordering::Release);
        tracing::info!(enabled, "echo-loop turn driver toggled");
    }

    pub fn set_team_id(&self, team_id: Option<String>) {
        *self.team_id.lock().unwrap() = team_id.clone();
        self.trunk.set_team_id(team_id);
    }

    pub fn team_id(&self) -> Option<String> {
        self.team_id.lock().unwrap().clone()
    }

    /// 该 agent 的循环模式（互斥循环插件推导，见
    /// [`crate::plugins::SINGLE_LOOP_PLUGIN_ID`] /
    /// [`crate::plugins::PARALLEL_LOOP_PLUGIN_ID`]）。
    /// 未配置 capabilities 时默认 `Single`（单会话，见
    /// [`echo_defs::LoopMode`]）。
    pub fn loop_mode(&self) -> echo_defs::LoopMode {
        let guard = self.capabilities.lock().unwrap();
        match guard.as_ref() {
            None => echo_defs::LoopMode::Single,
            Some(cap) => cap.loop_mode(),
        }
    }

    /// 测试专用：直接设置循环模式（绕过 capabilities 白名单推导）。
    #[cfg(test)]
    pub(crate) async fn set_loop_mode_for_test(&self, mode: echo_defs::LoopMode) {
        use crate::config::TeamMember;
        let plugin = match mode {
            echo_defs::LoopMode::Single => crate::plugins::SINGLE_LOOP_PLUGIN_ID,
            echo_defs::LoopMode::Parallel => crate::plugins::PARALLEL_LOOP_PLUGIN_ID,
        };
        self.apply_capabilities(&TeamMember {
            enabled_plugins: vec![plugin.into()],
            ..Default::default()
        })
        .await;
        debug_assert_eq!(self.loop_mode(), mode);
    }

    /// 临时回复分支的可见性是否开启：仅并行模式发射 ReplyBranch* 可见性
    /// 事件；单会话模式分支仍执行并合并，仅面板不可见。
    pub fn shows_reply_branches(&self) -> bool {
        self.loop_mode() == echo_defs::LoopMode::Parallel
    }

    /// Number of active sessions (for the Panel agent overview).
    pub fn session_count(&self) -> usize {
        self.trunk.all().len()
    }

    pub fn attach(&self, handle: Arc<BackendHandle>) {
        if let Ok(mut h) = self.handle.try_write() {
            *h = Some(handle);
        } else {
            tracing::warn!("handle slot busy, ignoring attach");
        }
    }

    /// 进程级插件宿主（组合根注入；未注入时为该 agent 自带的空宿主，
    /// 便于单元测试）。
    pub fn plugin_host(&self) -> std::sync::Arc<crate::plugins::PluginHost> {
        self.plugin_host
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| std::sync::Arc::new(crate::plugins::PluginHost::new()))
    }

    /// 注入共享的进程级插件宿主（组合根对每个 persona 调用一次）。
    pub fn set_plugin_host(&self, host: std::sync::Arc<crate::plugins::PluginHost>) {
        if let Ok(mut slot) = self.plugin_host.write() {
            *slot = host;
        } else {
            tracing::warn!("plugin host slot busy, ignoring set_plugin_host");
        }
    }

    /// 注入进程级事件汇聚点（组合根对每个 persona 调用一次）。
    /// 汇聚点优先于 `handle`：设置后 `emit` 只投递汇聚点，避免双投。
    pub fn attach_event_sink(&self, sink: EventSink) {
        if let Ok(mut slot) = self.event_sink.write() {
            *slot = Some(sink);
        } else {
            tracing::warn!("event sink slot busy, ignoring attach_event_sink");
        }
    }

    /// 注入该 persona 的工作区会话存储（`echo-agent.workspace` 插件）。
    ///
    /// 同时完成三件事（激活 = 进入项目对话，2026-09-14）：
    /// 1. 写入存储槽（命令处理 + 工具 + 提示词注入共用同一实例）；
    /// 2. 接线**变更钩子**——store 任何成功变更（面板命令 / 模型 `use`
    ///    工具）后统一广播 `WorkspaceSessions` 并确保通道会话注册，
    ///    「变更 → 广播」只有一个入口（弱引用，不构成循环）；
    /// 3. 若持久化的 `active` 存在（重启恢复），把对应通道会话补注册，
    ///    Panel 的会话列表/投影在连接后即可见（不广播：事件汇聚点尚未接入）。
    pub fn set_workspace_store(
        self: &std::sync::Arc<Self>,
        store: std::sync::Arc<crate::workspace::WorkspaceStore>,
    ) {
        if let Ok(mut slot) = self.workspace_store.write() {
            *slot = Some(store.clone());
        } else {
            tracing::warn!("workspace store slot busy, ignoring set_workspace_store");
            return;
        }
        let weak = std::sync::Arc::downgrade(self);
        store.set_on_change(std::sync::Arc::new(move || {
            if let Some(agent) = weak.upgrade() {
                agent.workspace_after_change();
            }
        }));
        // 启动补注册：active 持久化 → 通道会话常驻（幂等）。
        self.ensure_workspace_channel_for_active();
    }

    /// 当前 persona 的工作区会话存储（None = 插件未挂载）。
    pub fn workspace_store(&self) -> Option<std::sync::Arc<crate::workspace::WorkspaceStore>> {
        self.workspace_store
            .read()
            .ok()
            .and_then(|slot| slot.clone())
    }

    /// Non-blocking poll for a frontend command (used by the command pump).
    pub fn try_recv_command(&self) -> Option<BackendCommand> {
        self.handle
            .try_read()
            .ok()?
            .as_ref()
            .and_then(|h| h.try_recv_command())
    }

    /// Emit an event to subscribers (TUI, ...). No-op when unattached.
    ///
    /// The event is broadcast through the [`EventBus`](echo_context::EventBus) first (observe mode):
    /// the timeline projector and any other listeners consume it before the
    /// frontend hand-off, so the persisted display timeline and the wire both
    /// derive from the same emission.
    /// 提取一个可克隆、`'static` 的事件出口（联邦命令处理器等需跨
    /// await 边界发事件的调用方使用；语义与 `emit` 完全一致——
    /// annotate_team + 总线 + 汇聚点）。
    pub fn emit_handle(&self) -> std::sync::Arc<dyn Fn(BackendEvent) + Send + Sync> {
        // 安全前提：Agent 本身以 Arc 持有于进程级（supervisor/manager），
        // 其生命周期 ≥ 任何借用它的命令处理。在创建时快照出口。
        let bus = self.event_bus.clone();
        let sink = self.event_sink.read().ok().and_then(|g| g.clone());
        let handle = self.handle.try_read().ok().and_then(|g| g.clone());
        let team = self.team_id();
        std::sync::Arc::new(move |event| {
            // 与 emit 同口径（2026-10 修复）：此前本路径绕过 annotate_team
            // 且留了 `let _ = &team` 死代码——经它发射的事件若携带会话归属
            // 会静默漏标 team（Panel 按 team 过滤时串显）。现共享
            // annotate_team_for（team 快照于创建时，与 sink/handle 一致）。
            let event = Self::annotate_team_for(event, team.clone());
            bus.emit_sync(event.clone(), echo_context::DispatchMode::Observe);
            if let Some(sink) = &sink {
                sink(event);
                return;
            }
            if let Some(h) = &handle {
                h.emit(event);
            }
        })
    }

    pub fn emit(&self, event: BackendEvent) {
        // 中心化注入 team_id：多 team 场景下 Panel 需要知道事件归属哪个
        // team，才能过滤实时消息（避免串线）。
        let event = self.annotate_team(event);
        self.event_bus
            .emit_sync(event.clone(), echo_context::DispatchMode::Observe);
        // 汇聚点优先：组合根把所有 persona 都接到同一个进程级汇聚点，
        // 「谁挂到 Panel」不再取决于是否为默认人格。
        if let Some(sink) = self.event_sink.read().ok().and_then(|g| g.clone()) {
            sink(event);
            return;
        }
        if let Some(h) = self
            .handle
            .try_read()
            .ok()
            .as_ref()
            .and_then(|g| g.as_ref())
        {
            h.emit(event);
        }
    }

    /// 为携带 session 的实时事件标注 team_id（本 agent 的 team id）。
    fn annotate_team(&self, event: BackendEvent) -> BackendEvent {
        Self::annotate_team_for(event, self.team_id())
    }

    /// 把 `team_id` 标注进带会话归属的事件变体（emit 与 emit_handle
    /// 共用；2026-10：emit_handle 此前绕过标注且有死代码）。
    fn annotate_team_for(event: BackendEvent, team_id: Option<String>) -> BackendEvent {
        match event {
            BackendEvent::MessageReceived {
                session_id,
                adapter_name,
                platform,
                user_id,
                user_name,
                channel,
                group_name,
                content,
                images,
                timestamp,
                received_at_ms,
                message_sequence,
                ..
            } => BackendEvent::MessageReceived {
                session_id,
                adapter_name,
                platform,
                user_id,
                user_name,
                channel,
                group_name,
                content,
                images,
                timestamp,
                received_at_ms,
                message_sequence,
                team_id,
            },
            BackendEvent::AgentOutput {
                session_id,
                content,
                branch_id,
                ..
            } => BackendEvent::AgentOutput {
                session_id,
                team_id,
                content,
                branch_id,
            },
            BackendEvent::ToolCall {
                session_id,
                tool_name,
                arguments,
                tool_call_id,
                branch_id,
                started_at_ms,
                ..
            } => BackendEvent::ToolCall {
                session_id,
                team_id,
                tool_name,
                arguments,
                tool_call_id,
                branch_id,
                started_at_ms,
            },
            BackendEvent::ToolResult {
                session_id,
                tool_name,
                result,
                tool_call_id,
                timed_out,
                branch_id,
                elapsed_ms,
                ..
            } => BackendEvent::ToolResult {
                session_id,
                team_id,
                tool_name,
                result,
                tool_call_id,
                timed_out,
                branch_id,
                elapsed_ms,
            },
            BackendEvent::AgentThinking { session_id, .. } => BackendEvent::AgentThinking {
                session_id,
                team_id,
            },
            BackendEvent::ChecklistUpdated {
                session_id, state, ..
            } => BackendEvent::ChecklistUpdated {
                session_id,
                state,
                team_id,
            },
            BackendEvent::AgentReasoning {
                session_id,
                branch_id,
                content,
                ..
            } => BackendEvent::AgentReasoning {
                session_id,
                team_id,
                branch_id,
                content,
            },
            BackendEvent::AgentContentDelta {
                session_id,
                branch_id,
                delta,
                ..
            } => BackendEvent::AgentContentDelta {
                session_id,
                team_id,
                branch_id,
                delta,
            },
            BackendEvent::AgentReasoningDelta {
                session_id,
                branch_id,
                delta,
                ..
            } => BackendEvent::AgentReasoningDelta {
                session_id,
                team_id,
                branch_id,
                delta,
            },
            other => other,
        }
    }

    fn emit_reasoning(&self, session_id: &str, branch_id: &str, reasoning: &Option<String>) {
        if let Some(content) = reasoning
            .as_deref()
            .filter(|content| !content.trim().is_empty())
        {
            self.emit(BackendEvent::AgentReasoning {
                session_id: session_id.to_string(),
                team_id: None, // 由 annotate_team 统一注入
                branch_id: branch_id.to_string(),
                content: content.to_string(),
            });
        }
    }

    pub(crate) fn next_message_sequence(&self) -> u64 {
        self.next_message_sequence.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn register_inbound_turn(
        &self,
        session_id: &str,
        message_sequence: u64,
        branch: bool,
    ) -> InboundTurnRegistration {
        let id = uuid::Uuid::new_v4().to_string();
        let cancel = tokio_util::sync::CancellationToken::new();
        self.active_inbound_turns.insert(
            id.clone(),
            ActiveInboundTurn {
                session_id: session_id.to_string(),
                message_sequence,
                branch,
                cancel: cancel.clone(),
            },
        );
        InboundTurnRegistration { id, cancel }
    }

    pub(crate) fn finish_inbound_turn(&self, turn_id: &str) {
        self.active_inbound_turns.remove(turn_id);
    }

    /// Cancel the newest active turn belonging to the command sender. Branches
    /// win ties so a follow-up cancellation targets the temporary work first.
    pub(crate) fn cancel_inbound_turns(&self, session_id: &str, all: bool) -> usize {
        let mut turns = self
            .active_inbound_turns
            .iter()
            .filter(|turn| turn.session_id == session_id)
            .map(|turn| (turn.message_sequence, turn.branch, turn.cancel.clone()))
            .collect::<Vec<_>>();
        turns.sort_by_key(|(sequence, branch, _)| (*sequence, *branch));
        if !all {
            turns = turns.into_iter().rev().take(1).collect();
        }
        let count = turns.len();
        for (_, _, cancel) in turns {
            cancel.cancel();
        }
        count
    }

    pub(crate) fn cancel_all_inbound_turns(&self) -> usize {
        let turns = self
            .active_inbound_turns
            .iter()
            .map(|turn| turn.cancel.clone())
            .collect::<Vec<_>>();
        let count = turns.len();
        for cancel in turns {
            cancel.cancel();
        }
        count
    }

    pub(crate) async fn cancel_requested_work(&self, session_id: &str, all: bool) -> usize {
        self.cancel_inbound_turns(session_id, all)
    }

    /// Commit an inbound event and capture the branch's point-in-time context
    /// as one operation. The conversation lock is never held while the model
    /// or tools are running.
    pub(crate) async fn record_incoming_and_snapshot(
        &self,
        session: &Session,
        content: &str,
    ) -> Vec<ChatMessage> {
        let _turn = session.turn_lock.lock().await;
        // 入站内容脱敏（2026-10 安全）：会话事件（及其投影出的历史）
        // 永不落明文——用户粘贴的密钥不得进会话日志 / 归档 / 模型上下文。
        let content = self.redact_text(content);
        self.trunk
            .append_event(echo_session::SessionEvent::UserMessage(
                echo_session::event::UserMessage {
                    content: content.clone(),
                    timestamp: chrono::Utc::now().timestamp(),
                    message_sequence: structured_message_sequence(&content),
                    source: None,
                    images: extract_input_images(&content),
                    session: Some(session.id.clone()),
                },
            ));
        let snapshot = session.history.lock().await.clone();
        snapshot
    }

    /// Acquire this session's serial turn slot when the agent runs in
    /// single-session mode (`LoopMode::Single`, the default).
    ///
    /// tokio's `Mutex` is a fair FIFO queue, so queued turns start in arrival
    /// order; the slot is released when the returned guard drops (end of the
    /// turn). Parallel mode returns `None` and never blocks. Cancellation
    /// while queued also resolves to `None` (the caller gives up the turn).
    async fn acquire_turn_slot(
        &self,
        session: &Session,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        if self.loop_mode() != echo_defs::LoopMode::Single {
            return None;
        }
        tokio::select! {
            slot = Arc::clone(&session.turn_queue).lock_owned() => Some(slot),
            _ = cancel.cancelled() => None,
        }
    }

    /// Register a cancellable branch and record its input under the same
    /// conversation lock used by clear and merge operations.
    ///
    /// 单会话模式（默认）：先注册可取消的 turn（排队期间同样可被取消），
    /// 拿到该会话的串行闸门后才记录输入并取快照——排队的第二个 turn 因此
    /// 看到的是第一轮结束后的上下文；闸门随返回值交给调用方，turn 结束
    /// （guard drop）才让位。并行模式：注册与记录一次完成，各分支并发执行
    /// （到达时快照）。
    ///
    /// 返回 `None` = 排队期间被取消，调用方应直接收尾（不执行 turn）。
    #[allow(clippy::type_complexity)]
    pub(crate) async fn register_incoming_branch(
        &self,
        session: &Session,
        content: &str,
        message_sequence: u64,
    ) -> Option<(
        InboundTurnRegistration,
        Vec<ChatMessage>,
        Option<tokio::sync::OwnedMutexGuard<()>>,
    )> {
        let serial = self.loop_mode() == echo_defs::LoopMode::Single;
        let registration = if serial {
            self.register_inbound_turn(&session.id, message_sequence, true)
        } else {
            InboundTurnRegistration::default()
        };
        let slot = if serial {
            match self.acquire_turn_slot(session, &registration.cancel).await {
                Some(slot) => Some(slot),
                None => {
                    self.finish_inbound_turn(&registration.id);
                    return None;
                }
            }
        } else {
            None
        };
        let _turn = session.turn_lock.lock().await;
        let registration = if serial {
            registration
        } else {
            self.register_inbound_turn(&session.id, message_sequence, true)
        };
        self.trunk
            .append_event(echo_session::SessionEvent::UserMessage(
                echo_session::event::UserMessage {
                    content: content.to_string(),
                    timestamp: chrono::Utc::now().timestamp(),
                    message_sequence: Some(message_sequence),
                    source: None,
                    images: extract_input_images(content),
                    session: Some(session.id.clone()),
                },
            ));
        let snapshot = session.history.lock().await.clone();
        Some((registration, snapshot, slot))
    }

    async fn record_assistant_reply(
        &self,
        session: &Session,
        reply: String,
        message_sequence: Option<u64>,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> bool {
        if reply.trim().is_empty() {
            return true;
        }
        let _turn = session.turn_lock.lock().await;
        if cancel.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            return false;
        }
        // The assistant reply is a durable event; the event log re-derives
        // the trunk projection. When the reply answers a sequenced request
        // (concurrent branches), insert it after that request's user event so
        // the projected order matches request order.
        let event =
            echo_session::SessionEvent::AssistantMessage(echo_session::event::AssistantMessage {
                content: reply,
                reasoning_content: None,
                tool_calls: vec![],
                session: Some(session.id.clone()),
            });
        match message_sequence {
            Some(sequence) => self.trunk.insert_event_after_sequence(sequence, event),
            None => self.trunk.append_event(event),
        }
        true
    }

    pub(crate) async fn record_control_reply(
        &self,
        session: &Session,
        reply: String,
        message_sequence: u64,
    ) {
        self.record_assistant_reply(session, reply, Some(message_sequence), None)
            .await;
    }

    pub(crate) async fn process_recorded_message(
        &self,
        session: &Session,
        content: &str,
        history_snapshot: Vec<ChatMessage>,
        cancel: tokio_util::sync::CancellationToken,
        branch_id: &str,
    ) -> Result<String> {
        self.process_recorded_message_with_progress(
            session,
            content,
            history_snapshot,
            cancel,
            None,
            branch_id,
        )
        .await
    }

    pub(crate) async fn process_recorded_message_with_progress(
        &self,
        session: &Session,
        content: &str,
        history_snapshot: Vec<ChatMessage>,
        cancel: tokio_util::sync::CancellationToken,
        visible_reply: Option<tokio::sync::watch::Sender<bool>>,
        branch_id: &str,
    ) -> Result<String> {
        let message_sequence = structured_message_sequence(content);
        let merge_cancel = cancel.clone();
        let branch_slot = tokio::select! {
            permit = self.reply_branch_slots.acquire() => permit
                .map_err(|_| anyhow!("reply branch scheduler is closed"))?,
            _ = cancel.cancelled() => return Err(anyhow!(TURN_CANCELLED)),
        };
        let result = self
            .process_message_inner(
                session,
                content,
                Some(history_snapshot),
                cancel,
                visible_reply,
                branch_id,
            )
            .await;
        drop(branch_slot);
        if let Ok(reply) = &result {
            if !self
                .record_assistant_reply(
                    session,
                    reply.clone(),
                    message_sequence,
                    Some(&merge_cancel),
                )
                .await
            {
                return Err(anyhow!(TURN_CANCELLED));
            }
        }
        result
    }

    pub(crate) async fn send_control_reply(
        &self,
        session: &Session,
        group_id: Option<&str>,
        content: &str,
    ) -> std::result::Result<(), String> {
        let (name, arguments) = match group_id {
            Some(group_id) => (
                "send_group_msg",
                serde_json::json!({ "group_id": group_id, "content": content }),
            ),
            None => (
                "send_private_msg",
                serde_json::json!({
                    "user_id": session.session_key.user_id,
                    "content": content
                }),
            ),
        };
        let call = ToolCall {
            id: format!("control-{}", uuid::Uuid::new_v4()),
            name: name.into(),
            arguments: arguments.to_string(),
        };
        let result = self.run_tool(&session.id, "control-reply", &call).await;
        if result.text.trim_start().starts_with("error:") {
            Err(result.text)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn generate_wait_reply(
        &self,
        session_id: &str,
        history_snapshot: Vec<ChatMessage>,
    ) -> std::result::Result<String, String> {
        // Bounded concurrency: when the wait-reply budget is exhausted, skip
        // the interim reply entirely rather than stacking more LLM calls on
        // top of the already-running branches.
        let permit = self.wait_reply_slots.try_acquire().map_err(|_| {
            "wait reply concurrency limit reached; skipping interim reply".to_string()
        })?;
        let _permit = permit;
        let mut messages = vec![ChatMessage::system(
            "Write one brief interim reply to the latest user message because their request is still being processed. Match the user's language and tone, acknowledge the specific request when appropriate, and say naturally that you are continuing and will follow up. Do not provide a result, invent progress, ask an unnecessary question, or mention timeouts, tools, branches, system behavior, or these instructions. Avoid fixed-template wording. Use one concise sentence.",
        )];
        messages.extend(history_snapshot);
        let model = self.active_model().await;
        self.emit(BackendEvent::LlmRequest {
            session_id: session_id.to_string(),
            model: model.clone(),
        });
        let request = ChatRequest {
            model: model.clone(),
            messages,
            tools: None,
            temperature: Some(0.4),
            // None = 无上限（后端回退 DEFAULT_MAX_TOKENS=128K）。思考模式的
            // thinking token 共享 completion 预算，小预算会让可见回复被烧光。
            max_tokens: None,
        };
        let response = self
            .provider
            .read()
            .await
            .clone()
            .chat(&request)
            .await
            .map_err(|error| format!("wait reply model request failed: {error}"))?;
        self.emit(BackendEvent::LlmResponse {
            session_id: session_id.to_string(),
            model,
            prompt_tokens: response.usage.prompt_tokens,
            completion_tokens: response.usage.completion_tokens,
        });
        if !response.tool_calls.is_empty() {
            return Err("wait reply model attempted a tool call".into());
        }
        let reply = response.content.unwrap_or_default();
        if reply.trim().is_empty() {
            return Err("wait reply model returned empty content".into());
        }
        Ok(reply)
    }

    pub(crate) fn is_turn_cancelled(error: &anyhow::Error) -> bool {
        error.to_string() == TURN_CANCELLED
    }

    /// 当前正在执行 turn 的会话 id 去重列表（刷新恢复快照用，
    /// `ActiveTurnsSnapshot` 事件；见 protocol event 文档）。
    pub fn active_turn_session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .active_inbound_turns
            .iter()
            .map(|t| t.session_id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    #[cfg(test)]
    pub(crate) fn active_inbound_turn_count(&self) -> usize {
        self.active_inbound_turns.len()
    }

    #[cfg(test)]
    pub(crate) fn active_inbound_branch_count(&self) -> usize {
        self.active_inbound_turns
            .iter()
            .filter(|turn| turn.branch)
            .count()
    }

    pub async fn active_model(&self) -> String {
        let model = self.active_model.read().await.clone();
        if model.is_empty() {
            self.provider.read().await.default_model().to_string()
        } else {
            model
        }
    }

    pub async fn active_provider(&self) -> String {
        self.provider.read().await.name().to_string()
    }

    pub async fn set_model(&self, model: String) {
        *self.active_model.write().await = model;
    }

    /// 同步版 [`Self::set_model`]：供组合根启动期（make_agent 为同步闭包）调用。
    pub fn set_model_now(&self, model: String) {
        if let Ok(mut slot) = self.active_model.try_write() {
            *slot = model;
        }
    }

    pub async fn process_message(&self, session: &Session, content: &str) -> Result<String> {
        if self.draining.load(Ordering::Acquire) {
            return Err(anyhow!("Core 正在重启，消息暂未处理，请稍后重试"));
        }
        // 入站内容脱敏（2026-10 安全）：本 turn 及后续记录 / 模型上下文
        // 只使用脱敏后的文本（原 content 的调用方不受影响）。
        let content = self.redact_text(content);
        let content = content.as_str();
        let message_sequence =
            structured_message_sequence(content).unwrap_or_else(|| self.next_message_sequence());
        // `_serial`：单会话模式的排队闸门，持有到本轮结束（drop 即让位）。
        // 排队期间被取消 → None：入站事件已记录，但没有可回复的内容。
        let Some((registration, history_snapshot, _serial)) = self
            .register_incoming_branch(session, content, message_sequence)
            .await
        else {
            return Err(anyhow!(TURN_CANCELLED));
        };
        let branch_id = registration.id.clone();
        // 分支可见性（ReplyBranch* 事件）由能力开关决定：禁用时前端不展示
        // 分支卡/任务卡，但分支照常执行（取消/合并语义不变）。
        let show_branch = self.shows_reply_branches();
        if show_branch {
            self.emit(BackendEvent::ReplyBranchStarted {
                session_id: session.id.clone(),
                branch_id: branch_id.clone(),
                message_sequence,
                task: content.to_string(),
                target: "源会话".into(),
                started_at_ms: chrono::Utc::now().timestamp_millis(),
            });
        }
        let result = self
            .process_recorded_message(
                session,
                content,
                history_snapshot,
                registration.cancel.clone(),
                &branch_id,
            )
            .await;
        let cancelled = registration.cancel.is_cancelled()
            || result.as_ref().err().is_some_and(Self::is_turn_cancelled);
        self.finish_inbound_turn(&branch_id);
        if show_branch {
            self.emit(BackendEvent::ReplyBranchCompleted {
                session_id: session.id.clone(),
                branch_id,
                message_sequence,
                success: result.is_ok() && !cancelled,
                cancelled,
                completed_at_ms: chrono::Utc::now().timestamp_millis(),
            });
        }
        // 取消也要发 AgentCompleted（与 QQ 路径一致）：
        // 前端据此把 activity phase 置 completed —— 否则 busy 卡住、
        // 取消按钮/活动浮条/思考动画永远不中断。
        if cancelled {
            self.emit(BackendEvent::AgentCompleted {
                session_id: session.id.clone(),
            });
        }
        result
    }

    /// Full lifecycle for an inbound platform message. This is the single
    /// implementation of the "register branch → emit lifecycle events →
    /// bounded wait reply → execute → merge → surface outcome" flow; every
    /// adapter (QQ, ...) routes through it so the paths cannot drift apart.
    ///
    /// Runs the branch in the background and returns immediately, matching the
    /// one-way inbound hook contract.
    ///
    /// 单会话模式（默认）：注册（含会话串行闸门）在分支任务内完成——QQ 入站
    /// 不因排队而阻塞，排队中的分支同样可被取消，且本轮拿到的是前一轮结束后
    /// 的上下文快照。并行多会话模式：到达即注册并取快照（与旧行为一致），
    /// 各分支并发执行。
    pub(crate) async fn process_inbound_branch(
        self: &Arc<Self>,
        session: &Session,
        content: &str,
        message_sequence: u64,
        group_id: Option<String>,
        wait_reply_after: std::time::Duration,
    ) {
        if self.draining.load(Ordering::Acquire) {
            tracing::info!(session = %session.id, "inbound message dropped: core draining for restart");
            return;
        }
        // 入站内容脱敏（2026-10 安全）：QQ hook 等外部输入先脱敏再进入
        // 记录 / 模型上下文（覆盖单会话与并行两条路径）。
        let content = self.redact_text(content);
        let session = session.clone();
        // 能力开关：禁用时跳过 ReplyBranch* 可见性事件（分支照常执行）。
        let show_branch = self.shows_reply_branches();
        let serial = self.loop_mode() == echo_defs::LoopMode::Single;
        if serial {
            let agent = Arc::clone(self);
            tokio::spawn(async move {
                let Some((registration, history_snapshot, slot)) = agent
                    .register_incoming_branch(&session, &content, message_sequence)
                    .await
                else {
                    tracing::info!(
                        session = %session.id,
                        message_sequence,
                        "queued inbound branch cancelled before it started"
                    );
                    return;
                };
                agent
                    .run_inbound_turn(
                        session,
                        content,
                        message_sequence,
                        group_id,
                        wait_reply_after,
                        show_branch,
                        registration,
                        history_snapshot,
                        slot,
                    )
                    .await;
            });
            return;
        }
        // 运行期可能从 Parallel 切回 Single（能力热更新），且排队期间可被
        // 取消——`register_incoming_branch` 此时返回 None，不再 expect
        // （2026-10：TOCTOU panic 修复）。
        let Some((registration, history_snapshot, slot)) = self
            .register_incoming_branch(&session, &content, message_sequence)
            .await
        else {
            tracing::info!(
                session = %session.id,
                message_sequence,
                "queued inbound branch cancelled before it started"
            );
            return;
        };
        let agent = Arc::clone(self);
        tokio::spawn(async move {
            agent
                .run_inbound_turn(
                    session,
                    content,
                    message_sequence,
                    group_id,
                    wait_reply_after,
                    show_branch,
                    registration,
                    history_snapshot,
                    slot,
                )
                .await;
        });
    }

    /// 分支主体（两模式共用）：可见性事件 + 有界等待回复 + 执行 + 收尾。
    ///
    /// `_serial` 在单会话模式下是本会话的排队闸门，持有到本轮结束（任何
    /// 返回路径都会 drop 让位）。
    #[allow(clippy::too_many_arguments)]
    async fn run_inbound_turn(
        self: Arc<Self>,
        session: Session,
        content: String,
        message_sequence: u64,
        group_id: Option<String>,
        wait_reply_after: std::time::Duration,
        show_branch: bool,
        registration: InboundTurnRegistration,
        history_snapshot: Vec<ChatMessage>,
        _serial: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) {
        let agent = Arc::clone(&self);
        let session_id = session.id.clone();
        let branch_id = registration.id.clone();
        tracing::info!(
            session = %session_id,
            message_sequence,
            branch_id = %branch_id,
            started_at_ms = chrono::Utc::now().timestamp_millis(),
            "temporary inbound reply branch started"
        );
        if show_branch {
            agent.emit(BackendEvent::ReplyBranchStarted {
                session_id: session_id.clone(),
                branch_id: branch_id.clone(),
                message_sequence,
                task: content.clone(),
                target: "源会话".into(),
                started_at_ms: chrono::Utc::now().timestamp_millis(),
            });
        }
        let (visible_reply_tx, visible_reply_rx) = tokio::sync::watch::channel(false);
        let branch_completed = tokio_util::sync::CancellationToken::new();
        let group_id_for_wait = group_id.clone();
        spawn_contextual_wait_reply(
            Arc::clone(&agent),
            session.clone(),
            branch_id.clone(),
            group_id_for_wait,
            history_snapshot.clone(),
            visible_reply_rx,
            branch_completed.clone(),
            wait_reply_after,
        );
        let turn_cancel = registration.cancel.clone();
        let result = agent
            .process_recorded_message_with_progress(
                &session,
                &content,
                history_snapshot,
                turn_cancel.clone(),
                Some(visible_reply_tx),
                &branch_id,
            )
            .await;
        branch_completed.cancel();
        let cancelled = turn_cancel.is_cancelled()
            || result.as_ref().err().is_some_and(Self::is_turn_cancelled);
        agent.finish_inbound_turn(&branch_id);

        tracing::info!(
            session = %session_id,
            message_sequence,
            branch_id = %branch_id,
            success = result.is_ok() && !cancelled,
            cancelled,
            completed_at_ms = chrono::Utc::now().timestamp_millis(),
            "temporary inbound reply branch finished"
        );
        if show_branch {
            agent.emit(BackendEvent::ReplyBranchCompleted {
                session_id: session_id.clone(),
                branch_id: branch_id.clone(),
                message_sequence,
                success: result.is_ok() && !cancelled,
                cancelled,
                completed_at_ms: chrono::Utc::now().timestamp_millis(),
            });
        }
        match result {
            Ok(output) => {
                if !output.trim().is_empty() {
                    agent.emit(BackendEvent::AgentOutput {
                        session_id: session_id.clone(),
                        content: output,
                        branch_id: Some(branch_id.clone()),
                        team_id: None,
                    });
                }
                tracing::info!(
                    session = %session_id,
                    message_sequence,
                    "inbound adapter message processed"
                );
            }
            Err(error) if Self::is_turn_cancelled(&error) => {
                agent.emit(BackendEvent::AgentCompleted {
                    session_id: session_id.clone(),
                });
                tracing::info!(
                    session = %session_id,
                    message_sequence,
                    "inbound adapter task cancelled"
                );
            }
            Err(error) => {
                tracing::warn!(%error, session = %session_id, "agent processing failed");
                agent.emit(BackendEvent::Error {
                    session_id: Some(session_id.clone()),
                    message: error.to_string(),
                });
                // The platform user has no view of backend Error events —
                // send a bounded failure notice so they are not left
                // wondering why no reply arrived.
                let agent = Arc::clone(&agent);
                let session = session.clone();
                let session_id = session_id.clone();
                let group_id = group_id.clone();
                tokio::spawn(async move {
                    let notice = format!("处理失败：{error}");
                    let notice: String = notice.chars().take(500).collect();
                    if let Err(send_error) = agent
                        .send_control_reply(&session, group_id.as_deref(), &notice)
                        .await
                    {
                        tracing::warn!(%send_error, session = %session_id, "failure notice could not be delivered");
                    }
                });
            }
        }
    }

    /// 经 echo-loop TurnRunner 驱动一轮（仅普通输入；`history_snapshot` 由
    /// 调用方提供——与内置循环同语义：以点切快照为基准，不重复读取）。
    ///
    /// 2026-10 巡检补齐（与内置循环逐项对齐，接线前完成）：
    /// - **UI 事件**：`AgentThinking` 起、`LlmRequest`/`LlmResponse`（经 runner
    ///   总线翻译）、`AgentReasoning`（回调）、`AgentCompleted` 收；
    /// - **/context 视图**：写 `last_prompt_blocks`（与内置循环同口径）；
    /// - **动态模型/provider**：模型每 turn 解析、provider 每 step 由
    ///   `ChatExecutor` 解析——运行期 persona API 切换即时生效；
    /// - **在途取消**：模型请求与工具执行均与 turn 取消竞速；
    /// - **工具产出**：图片透传（`ToolOutcome`）、成功 `send_*` 触发
    ///   visible_reply 抑制；
    /// - **超时守卫**：与内置循环同口径（`tool_guard_timeout` + 共享 notice）。
    // 8 个参数：turn 的入参就是这么多（与内置循环 process_message_inner
    // 同构）；合并成结构体属重构，不在本次修复范围内。
    #[allow(clippy::too_many_arguments)]
    async fn process_via_echo_loop(
        &self,
        session: &Session,
        content: &str,
        history_snapshot: Option<Vec<ChatMessage>>,
        turn_cancel: tokio_util::sync::CancellationToken,
        visible_reply: Option<tokio::sync::watch::Sender<bool>>,
        branch_id: &str,
        runner: std::sync::Arc<echo_loop::runner::TurnRunner>,
    ) -> Result<String> {
        let session_id = session.id.clone();
        // turn 开始（与内置循环同口径）：busy 启动 + 思考动画。
        tracing::info!(
            session = %session_id,
            driver = "echo-loop",
            "agent turn started (echo-loop)"
        );
        self.emit(BackendEvent::AgentThinking {
            session_id: session_id.clone(),
            team_id: None,
        });
        // 系统提示词（与内置循环同源同层，2026-09-30 补齐）：base（persona
        // system_prompt 覆盖值）→ 全局 system 技能 → persona 系统技能
        // （capabilities.system_skills）→ … → 工作区会话注入。
        // boundary 传 None 是正确语义：本路径不承接 QQ hook/定时器/QQ 会话
        // （分派处已排除），与内置循环的 else 分支一致。
        let (base, persona_skills, workspace) = {
            let base = self.config.read().await.system_prompt.clone();
            let persona_skills = self
                .capabilities
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.system_skills.clone())
                .unwrap_or_default();
            let workspace = self.workspace_prompt_text();
            (base, persona_skills, workspace)
        };
        let blocks = {
            let skills = self.skills.lock().await;
            crate::agent::prompt::build_prompt_blocks(
                &skills,
                &base,
                content,
                None,
                &persona_skills,
                workspace,
            )
            .await
        };
        let system_prompt = crate::agent::prompt::join_prompt_blocks(&blocks);
        // `/context` 视图（2026-10 巡检）：与内置循环同口径保存最近一次
        // 提示词块——此前 echo 路径不写，切到 /context 显示陈旧数据。
        *self.last_prompt_blocks.lock().await = Some(blocks);
        let history = history_snapshot.unwrap_or_else(|| session.history.blocking_lock().clone());
        // 工具 schema：注册表是唯一真源（与内置循环同源）。编排工具
        // （spawn_subagent）同样由注册表提供（组合根装配时注册），这里不得
        // 再追加同名定义——重复工具名会让 API 拒绝整个请求
        // （"Tool names must be unique"）。
        let tools = (*self.tools.definitions().await).clone();

        // 模型名（2026-10 巡检：每 turn 解析——运行期模型/provider 切换在
        // 下一 turn 生效；此前 runner 构造期固化，切换完全失效）。
        let model = self.active_model().await;

        // 推理回调（引用式，生命周期 = run 调用作用域）。
        let reasoning_cb = |sid: String, text: String| {
            self.emit_reasoning(&sid, branch_id, &Some(text));
        };

        // ── 生命周期事件翻译（2026-10 巡检）──
        // 订阅 runner 的事件总线，把 turn/step 事件转发为 Panel 事件
        // （与内置循环同屏同字段）。订阅按 session_id 过滤——runner 可被
        // 多会话并发共享；Disposer 随本作用域保管，函数返回即注销。
        let emit_handle = self.emit_handle();
        let _subscriptions = {
            let bus = runner.bus().clone();
            let mut disposers: Vec<echo_context::Disposer> = Vec::new();
            {
                let emit = emit_handle.clone();
                let sid = session_id.clone();
                let turn = branch_id.to_string();
                disposers.push(bus.observe::<echo_loop::AgentRequest, _>(move |evt| {
                    // session + turn 双过滤：并行模式同会话并发分支时
                    // 各 turn 的订阅互不串收（2026-10 巡检）。
                    if evt.session_id == sid && evt.turn_id.as_deref() == Some(turn.as_str()) {
                        emit(BackendEvent::LlmRequest {
                            session_id: evt.session_id.clone(),
                            model: evt.request.model.clone(),
                        });
                    }
                }));
            }
            {
                let emit = emit_handle.clone();
                let sid = session_id.clone();
                let turn = branch_id.to_string();
                disposers.push(bus.observe::<echo_loop::ModelResponse, _>(move |evt| {
                    if evt.session_id == sid && evt.turn_id.as_deref() == Some(turn.as_str()) {
                        emit(BackendEvent::LlmResponse {
                            session_id: evt.session_id.clone(),
                            model: evt.model.clone(),
                            prompt_tokens: evt.prompt_tokens,
                            completion_tokens: evt.completion_tokens,
                        });
                    }
                }));
            }
            disposers
        };

        // 动态 chat 出口（2026-10 巡检）：每 step 解析**当前** provider 与
        // 预算（persona API 切换即时生效）；参数转 owned、future 仅借 self。
        //
        // 流式接线（2026-10）：主对话 turn 经 `chat_stream` + 增量转发
        // （`AgentContentDelta` / `AgentReasoningDelta`，~40ms 合并批）；
        // `stream_output=false` 时退回整段 `chat`。每 step 一个转发任务，
        // 返回前 flush 收尾（增量先于后续事件到达）。
        let stream_emitter = self.emit_handle();
        let stream_session = session_id.clone();
        let stream_branch = branch_id.to_string();
        let chat_executor: echo_loop::ChatExecutor<'_> = &|mut request: ChatRequest| {
            let this = self;
            let emitter = stream_emitter.clone();
            let sid = stream_session.clone();
            let bid = stream_branch.clone();
            Box::pin(async move {
                request.max_tokens = this.config.read().await.effective_max_tokens();
                let provider = this.provider.read().await.clone();
                if !this.config.read().await.stream_output {
                    return provider.chat(&request).await;
                }
                let (delta_tx, delta_rx) = tokio::sync::mpsc::unbounded_channel();
                let forwarder = spawn_delta_forwarder(delta_rx, emitter, sid, bid);
                let result = provider.chat_stream(&request, delta_tx).await;
                let _ = forwarder.await;
                result
            })
        };

        // 工具执行器（异步版，2026-10 巡检）：超时守卫（与内置循环同一口径）
        // + turn 取消竞速（在途工具可立即中止）+ visible_reply 抑制。
        // run_tool 内部已发 ToolCall/ToolResult 事件并写事件日志；被 drop 的
        // future 已记录 ToolCall 事件，补记中断结果保持事件日志成对。
        // 取消令牌的克隆副本（executor 闭包持有其引用；原令牌稍后 move 进
        // runner.run，#[E0505] 的规避——两者语义一致，同源取消）。
        let cancel_for_tools = turn_cancel.clone();
        let executor: echo_loop::ToolExecutor<'_> =
            &|sid: &str, _branch: &str, call: &crate::llm::ToolCall| {
                let this = self;
                let call = call.clone();
                let sid = sid.to_string();
                let branch = branch_id.to_string();
                let cancel = cancel_for_tools.clone();
                let visible = visible_reply.clone();
                Box::pin(async move {
                    // 工具开始执行时刻（中断补记 elapsed_ms 用；语义 = 开始到中断判定）。
                    let tool_started = std::time::Instant::now();
                    let guard = this.tool_guard_timeout(&call).await;
                    tokio::select! {
                        result = tokio::time::timeout(guard, this.run_tool(&sid, &branch, &call)) => {
                            match result {
                                Ok(tool_result) => {
                                    // 成功的 send_* 即该分支的可见回复：抑制
                                    // 并行的临时等待回复（与内置循环同口径）。
                                    let failed =
                                        tool_result.text.trim_start().starts_with("error:");
                                    if !failed
                                        && matches!(
                                            call.name.as_str(),
                                            "send_private_msg" | "send_group_msg" | "send_backend_message"
                                        )
                                    {
                                        if let Some(visible) = &visible {
                                            let _ = visible.send(true);
                                        }
                                    }
                                    echo_loop::ToolOutcome::with_images(
                                        tool_result.text.clone(),
                                        tool_result.images.clone(),
                                    )
                                }
                                Err(_) => {
                                    let text = tool_timeout_notice(&call.name, guard.as_secs());
                                    this.record_interrupted_tool_result(
                                        &sid,
                                        &branch,
                                        &call,
                                        &text,
                                        true,
                                        tool_started.elapsed().as_millis() as u64,
                                    );
                                    echo_loop::ToolOutcome::text(text)
                                }
                            }
                        }
                        _ = cancel.cancelled() => {
                            this.record_interrupted_tool_result(
                                &sid,
                                &branch,
                                &call,
                                "error: tool execution cancelled",
                                false,
                                tool_started.elapsed().as_millis() as u64,
                            );
                            echo_loop::ToolOutcome::text("error: tool execution cancelled")
                        }
                    }
                })
            };

        // Subagent hook（echo-loop 的 hook 注入接口）：spawn_subagent 的
        // future 不 Send，无法走普通执行器——经 `execute_async` 通道直接进
        // 管线（模型可见的 schema 由注册表统一提供，不再经
        // extra_tool_definitions 追加）。spawn_subagent 本身是同步受理
        // （注册 + 后台拉起），结果立即可得——同步 body 内一次性算好文本，
        // 再包一个 'static ready future 返回（AsyncToolExecutor 的设计意图：
        // harness 编排需要 tokio::spawn 时经通道把结果带回，这里无需 spawn
        // 因此直接 ready）。
        let is_async_subagent = |name: &str| name == crate::subagent::SPAWN_SUBAGENT_TOOL;
        let execute_async_subagent = move |sid: &str,
                                           _branch: &str,
                                           call: &crate::llm::ToolCall|
              -> std::pin::Pin<
            Box<dyn std::future::Future<Output = echo_loop::ToolOutcome> + Send>,
        > {
            let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
            let text = if !self.allows_dynamic_tool(crate::subagent::SPAWN_SUBAGENT_TOOL) {
                format!(
                    "error: tool '{}' is not registered (subagent 插件未启用或不在本 persona 白名单)",
                    crate::subagent::SPAWN_SUBAGENT_TOOL
                )
            } else {
                match self.subagent_runtime() {
                    None => "error: subagent runtime not attached".to_string(),
                    Some((store, spawn)) => {
                        let tool = crate::subagent::SpawnSubagentTool::new(store, spawn);
                        let cancel = self
                            .active_inbound_turns
                            .get(branch_id)
                            .map(|turn| turn.cancel.clone())
                            .unwrap_or_default();
                        match tool.spawn(&args, sid, cancel, branch_id) {
                            Ok(receipt) => receipt,
                            Err(error) => format!("error: {error}"),
                        }
                    }
                }
            };
            Box::pin(async move { echo_loop::ToolOutcome::text(text) })
        };

        let result = {
            let subagent_hooks = echo_loop::SubagentToolHooks {
                is_async_tool: Some(&is_async_subagent),
                execute_async: Some(&execute_async_subagent),
                // schema 由注册表提供（见上）；hook 只负责异步执行分派。
                extra_tool_definitions: None,
            };
            let extras = echo_loop::runner::RunExtras {
                tools: Some(tools),
                on_reasoning: Some(&reasoning_cb),
                subagent_hooks,
                model: Some(model),
                chat: Some(chat_executor),
                turn_id: Some(branch_id.to_string()),
            };
            runner
                .run(
                    &session_id,
                    content.to_string(),
                    system_prompt,
                    history,
                    turn_cancel,
                    executor,
                    extras,
                )
                .await
        };

        match result {
            Ok(reply) => {
                // turn 完成（与内置循环同口径）：busy 收尾。
                self.emit(BackendEvent::AgentCompleted {
                    session_id: session_id.clone(),
                });
                Ok(reply)
            }
            Err(echo_loop::runner::LoopError::Cancelled) => Err(anyhow!(TURN_CANCELLED)),
            Err(other) => Err(anyhow!(other.to_string())),
        }
    }

    async fn process_message_inner(
        &self,
        session: &Session,
        content: &str,
        history_snapshot: Option<Vec<ChatMessage>>,
        turn_cancel: tokio_util::sync::CancellationToken,
        visible_reply: Option<tokio::sync::watch::Sender<bool>>,
        branch_id: &str,
    ) -> Result<String> {
        // ── 可插拔循环驱动（echo-loop）──
        // loop.runner 插件启用且持有 TurnRunner 时，普通输入（非 QQ hook/
        // 定时器/QQ 会话——边界与投递语义仍由内置循环保证）经 TurnRunner 的
        // turn/step 状态机 + ToolPipeline 执行；否则走内置循环（默认行为）。
        if self.use_echo_loop.load(Ordering::Acquire) {
            if let Some(runner) = self.loop_runner.read().await.clone() {
                let boundary = {
                    let is_qq_hook = content.contains("<qq_message_hook>");
                    let is_timer = content.contains(crate::input_marker::TIMER_EVENT_OPEN);
                    let is_qq_session = session.session_key.platform.eq_ignore_ascii_case("qq");
                    is_qq_hook || is_timer || is_qq_session
                };
                if !boundary {
                    return self
                        .process_via_echo_loop(
                            session,
                            content,
                            history_snapshot,
                            turn_cancel,
                            visible_reply.clone(),
                            branch_id,
                            runner,
                        )
                        .await;
                }
            }
        }
        let queued_at = std::time::Instant::now();
        let turn_started_at = chrono::Utc::now();
        let turn_id = uuid::Uuid::new_v4().to_string();
        let message_sequence = structured_message_sequence(content);
        let session_id = session.id.clone();
        tracing::info!(
            turn_id = %turn_id,
            message_sequence = message_sequence.unwrap_or_default(),
            session = %session_id,
            started_at = %turn_started_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            queue_wait_ms = queued_at.elapsed().as_millis() as u64,
            "agent turn started"
        );
        self.emit(BackendEvent::AgentThinking {
            session_id: session_id.clone(),
            team_id: None,
        });

        // Distinguish input origin by content markers, NOT by the session's
        // platform. A QQ session can receive backend/TUI input (no hook), and
        // that must be answered in the backend — never pushed to QQ.
        let is_qq_hook = content.contains("<qq_message_hook>");
        let is_timer = content.contains(crate::input_marker::TIMER_EVENT_OPEN);
        let is_qq_session = session.session_key.platform.eq_ignore_ascii_case("qq");
        let boundary = if is_qq_hook {
            Some(BoundaryKind::QqHook)
        } else if is_timer {
            Some(BoundaryKind::Timer)
        } else if is_qq_session {
            Some(BoundaryKind::BackendInput)
        } else {
            None
        };
        // Build the system prompt as named blocks so the panel can visualize
        // token usage per section; keep the last build for `/context`.
        let (base, persona_skills, workspace) = {
            let base = self.config.read().await.system_prompt.clone();
            let persona_skills = self
                .capabilities
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.system_skills.clone())
                .unwrap_or_default();
            let workspace = self.workspace_prompt_text();
            (base, persona_skills, workspace)
        };
        let blocks = {
            let skills = self.skills.lock().await;
            crate::agent::prompt::build_prompt_blocks(
                &skills,
                &base,
                content,
                boundary,
                &persona_skills,
                workspace,
            )
            .await
        };
        let system_prompt = crate::agent::prompt::join_prompt_blocks(&blocks);
        *self.last_prompt_blocks.lock().await = Some(blocks);

        let mut messages = vec![ChatMessage::system(system_prompt)];
        if let Some(history_snapshot) = history_snapshot {
            messages.extend(history_snapshot);
        } else {
            let history = session.history.lock().await;
            messages.extend(history.iter().cloned());
        }

        // 注册表是模型可见工具的唯一真源（spawn_subagent 由组合根装配时注册，
        // 随 echo-agent.subagent 插件门控）。这里不得再追加同名动态定义：
        // 同名工具出现两次会让 API 拒绝整个请求（"Tool names must be unique"）。
        let tools = (*self.tools.definitions().await).clone();
        // max_tool_iterations == 0 still allows one direct reply (without tools).
        let max_iterations = self.config.read().await.max_tool_iterations.max(1);
        // None/0 = 无上限（后端回退 DEFAULT_MAX_TOKENS=128K）。
        let max_tokens = self.config.read().await.effective_max_tokens();
        let mut truncation_continues = 0usize;

        for _ in 0..max_iterations {
            if turn_cancel.is_cancelled() {
                return Err(anyhow!(TURN_CANCELLED));
            }
            let model = self.active_model().await;
            self.emit(BackendEvent::LlmRequest {
                session_id: session_id.clone(),
                model: model.clone(),
            });
            let request = ChatRequest {
                model: model.clone(),
                messages: messages.clone(),
                tools: Some(tools.clone()),
                temperature: None,
                max_tokens,
            };
            let provider = self.provider.read().await.clone();
            let log_model_error = |error: crate::llm::LlmError| -> crate::llm::LlmError {
                tracing::warn!(
                    turn_id = %turn_id,
                    message_sequence = message_sequence.unwrap_or_default(),
                    session = %session_id,
                    %error,
                    elapsed_ms = queued_at.elapsed().as_millis() as u64,
                    "agent turn model request failed"
                );
                error
            };
            // 流式接线（2026-10）：与 echo-loop 路径同口径（增量转发 +
            // 返回前 flush 收尾）；`stream_output=false` 时退回整段。
            let stream_enabled = self.config.read().await.stream_output;
            let response = if stream_enabled {
                let (delta_tx, delta_rx) = tokio::sync::mpsc::unbounded_channel();
                let forwarder = spawn_delta_forwarder(
                    delta_rx,
                    self.emit_handle(),
                    session_id.clone(),
                    branch_id.to_string(),
                );
                let result = tokio::select! {
                    response = provider.chat_stream(&request, delta_tx) => {
                        response.map_err(log_model_error)?
                    }
                    _ = turn_cancel.cancelled() => return Err(anyhow!(TURN_CANCELLED)),
                };
                let _ = forwarder.await;
                result
            } else {
                tokio::select! {
                    response = provider.chat(&request) => response.map_err(log_model_error)?,
                    _ = turn_cancel.cancelled() => return Err(anyhow!(TURN_CANCELLED)),
                }
            };
            self.emit_reasoning(&session_id, branch_id, &response.reasoning_content);
            self.emit(BackendEvent::LlmResponse {
                session_id: session_id.clone(),
                model,
                prompt_tokens: response.usage.prompt_tokens,
                completion_tokens: response.usage.completion_tokens,
            });

            if response.truncated() {
                // 输出被 token 上限截断：残片（含未写完的工具调用）不能当作
                // 正常收尾——丢弃半截工具调用，已生成文本入栈，让模型续写。
                // 历史上这里直接返回空 reply，表现为"agent 自己断掉"。
                truncation_continues += 1;
                tracing::warn!(
                    turn_id = %turn_id,
                    session = %session_id,
                    truncation_continues,
                    stop_reason = ?response.stop_reason,
                    "model output truncated at token limit; continuing"
                );
                if truncation_continues > MAX_TRUNCATION_CONTINUES {
                    return Err(anyhow!(
                        "model output was truncated at the token limit {MAX_TRUNCATION_CONTINUES} times in one turn; giving up"
                    ));
                }
                if let Some(text) = &response.content {
                    if !text.trim().is_empty() {
                        messages.push(ChatMessage::assistant_with_reasoning(
                            text.clone(),
                            response.reasoning_content.clone(),
                        ));
                    }
                }
                messages.push(ChatMessage::user(TRUNCATION_CONTINUE_PROMPT));
                continue;
            }

            if response.tool_calls.is_empty() {
                let reply = response.content.unwrap_or_default();
                self.emit(BackendEvent::AgentCompleted {
                    session_id: session_id.clone(),
                });
                tracing::info!(
                    turn_id = %turn_id,
                    message_sequence = message_sequence.unwrap_or_default(),
                    session = %session_id,
                    completed_at = %chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    elapsed_ms = queued_at.elapsed().as_millis() as u64,
                    "agent turn completed"
                );
                return Ok(reply);
            }

            // LLM wants tools — execute them and continue the loop.
            messages.push(assistant_with_tool_calls(
                &response.tool_calls,
                &response.content,
                &response.reasoning_content,
            ));
            for call in &response.tool_calls {
                let tool_started = std::time::Instant::now();
                tracing::info!(
                    turn_id = %turn_id,
                    message_sequence = message_sequence.unwrap_or_default(),
                    session = %session_id,
                    tool_call_id = %call.id,
                    tool = %call.name,
                    "agent tool call started"
                );
                // Guard every tool with a timeout so a hung tool (e.g. an
                // unresponsive HTTP call) cannot stall the branch forever.
                // 超时只是中止单个工具调用，**不中断 turn**：结果以 notice
                // 形式喂回模型（非 error 前缀），loop 继续，模型可重试或
                // 直接继续作答。守卫口径与 echo-loop 路径共用，见
                // `tool_guard_timeout`（配置 base + 工具 timeout_hint）。
                let tool_timeout = self.tool_guard_timeout(call).await;
                let mut timed_out = false;
                let result = tokio::select! {
                    result = self.run_tool(&session_id, branch_id, call) => result,
                    _ = tokio::time::sleep(tool_timeout) => {
                        timed_out = true;
                        let text = tool_timeout_notice(&call.name, tool_timeout.as_secs());
                        // run_tool was dropped mid-flight: it already recorded
                        // the ToolCall event, so record the matching ToolResult
                        // or the durable log keeps a dangling call.
                        self.record_interrupted_tool_result(
                            &session_id,
                            branch_id,
                            call,
                            &text,
                            true,
                            tool_started.elapsed().as_millis() as u64,
                        );
                        crate::tool::ToolResult::text(text)
                    }
                    _ = turn_cancel.cancelled() => {
                        self.record_interrupted_tool_result(
                            &session_id,
                            branch_id,
                            call,
                            "error: tool execution cancelled",
                            false,
                            tool_started.elapsed().as_millis() as u64,
                        );
                        return Err(anyhow!(TURN_CANCELLED));
                    }
                };
                // 失败判定：error 前缀 或 超时（超时不算成功，也不算 delivery 送达）。
                let failed = result.text.trim_start().starts_with("error:");
                let success = !failed && !timed_out;
                tracing::info!(
                    turn_id = %turn_id,
                    message_sequence = message_sequence.unwrap_or_default(),
                    session = %session_id,
                    tool_call_id = %call.id,
                    tool = %call.name,
                    success,
                    timed_out,
                    elapsed_ms = tool_started.elapsed().as_millis() as u64,
                    "agent tool call completed"
                );
                // A successful send tool call is the branch's visible reply;
                // it suppresses the interim wait-reply for parallel branches.
                if success
                    && matches!(
                        call.name.as_str(),
                        "send_private_msg" | "send_group_msg" | "send_backend_message"
                    )
                {
                    if let Some(visible_reply) = &visible_reply {
                        let _ = visible_reply.send(true);
                    }
                }
                messages.push(ChatMessage::tool_with_images(
                    echo_defs::media::compact_embedded_media(&result.text, &result.images),
                    &call.id,
                    result.images.clone(),
                ));
            }
        }

        tracing::warn!(
            turn_id = %turn_id,
            message_sequence = message_sequence.unwrap_or_default(),
            session = %session_id,
            elapsed_ms = queued_at.elapsed().as_millis() as u64,
            max_iterations,
            "agent turn exhausted tool iterations"
        );
        Err(anyhow!(
            "reached max tool iterations ({}) without a final reply",
            max_iterations
        ))
    }

    /// 模型可见的工具参数预检：schema 必需字段缺失时返回纠正性错误文案
    /// （说清"你发了什么、应该发什么"），返回 None 表示通过预检。
    async fn tool_arguments_error(
        &self,
        tool_name: &str,
        raw_arguments: &str,
        args: &serde_json::Value,
    ) -> Option<String> {
        crate::agent::tool_exec::tool_arguments_error(&self.tools, tool_name, raw_arguments, args)
            .await
    }

    /// 工具统一分派入口（注册表工具 + 编排工具 spawn_subagent 特判）。
    /// pub：集成测试经此直接驱动分派（不绕 LLM）。
    pub async fn run_tool(
        &self,
        session_id: &str,
        branch_id: &str,
        call: &ToolCall,
    ) -> crate::tool::ToolResult {
        // 执行开始时刻：epoch 毫秒进事件（面板「运行中实时计时」用），
        // Instant 用于本地测「结束减开始」的耗时。
        let started = std::time::Instant::now();
        let started_at_ms = chrono::Utc::now().timestamp_millis();
        // 参数出口脱敏（2026-10 安全）：事件 / 会话落盘 / 面板都不得出现
        // 明文秘密。执行仍用原始参数（功能正确性不受影响）——被替换的
        // 只有"记录面"。
        let logged_arguments = self.redact_text(&call.arguments);
        self.emit(BackendEvent::ToolCall {
            session_id: session_id.to_string(),
            team_id: None,
            tool_name: call.name.clone(),
            arguments: logged_arguments.clone(),
            tool_call_id: call.id.clone(),
            branch_id: branch_id.to_string(),
            started_at_ms: Some(started_at_ms),
        });
        // The tool call is a durable event: the model-visible loop (call +
        // result) must be reconstructable from the log after a reload.
        self.trunk
            .append_event(echo_session::SessionEvent::ToolCall(
                echo_session::event::ToolCallEvent {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: logged_arguments,
                    session: Some(session_id.to_string()),
                    started_at_ms: Some(started_at_ms),
                },
            ));
        // 参数解析与预检：给模型可纠正的错误反馈。非法 JSON 或缺少必需
        // 字段时，错误文案必须说清"你发了什么、应该发什么"——一句模糊的
        // "command required" 只会让模型原样重试，形成空调用退化循环。
        let result = match serde_json::from_str::<serde_json::Value>(&call.arguments) {
            Err(error) => crate::tool::ToolResult::text(format!(
                "error: 工具参数不是合法 JSON（{error}）。你发送的原始参数: {}。请修正为合法 JSON 后重新调用 {}。",
                crate::llm::truncate(&call.arguments, 200),
                call.name,
            )),
            Ok(args) => match call.name.as_str() {
            crate::subagent::SPAWN_SUBAGENT_TOOL => {
                // 异步编排工具：插件未挂载 / persona 不允许时按未注册工具报错。
                if !self.allows_dynamic_tool(crate::subagent::SPAWN_SUBAGENT_TOOL) {
                    Err(format!(
                        "tool '{}' is not registered (subagent 插件未启用或不在本 persona 白名单)",
                        crate::subagent::SPAWN_SUBAGENT_TOOL
                    ))
                } else {
                    match self.tool_arguments_error(
                        crate::subagent::SPAWN_SUBAGENT_TOOL,
                        &call.arguments,
                        &args,
                    ).await {
                        Some(message) => Err(message),
                        None => {
                            let (store, spawn) = self
                                .subagent_runtime()
                                .expect("allows_dynamic_tool checked the runtime is attached");
                            let tool = crate::subagent::SpawnSubagentTool::new(store, spawn);
                            let cancel = self
                                .active_inbound_turns
                                .get(branch_id)
                                .map(|turn| turn.cancel.clone())
                                .unwrap_or_default();
                            tool.spawn(&args, session_id, cancel, branch_id)
                                .map(crate::tool::ToolResult::text)
                        }
                    }
                }
            }
            other => {
                // Every dynamic tool must live in the single dispatch
                match self.tool_arguments_error(other, &call.arguments, &args).await {
                    Some(message) => Err(message),
                    None => {
                        // 会话/归属上下文注入（2026-10）：在 schema 校验
                        // 之后、执行之前——checklist 按会话隔离状态（P0
                        // 修复：此前全局共享，跨会话互看互覆）；shell
                        // 三件套按归属校验（替代线程本地方案，跨 await
                        // 不可靠问题顺带消除）。不进 ToolCall 事件（事件
                        // 用原始参数）。
                        let mut args = args;
                        match other {
                            "checklist" => {
                                args["__session_id"] = serde_json::json!(session_id);
                            }
                            "shell_start" | "shell_exec" | "shell_stop" => {
                                args["__team_id"] = serde_json::json!(self.team_id());
                            }
                            _ => {}
                        }
                        self.tools
                            .execute_rich(other, args)
                            .await
                            .map_err(|error| error.to_string())
                    }
                }
            }
        }
        .unwrap_or_else(|error| crate::tool::ToolResult::text(format!("error: {error}"))),
        };
        let mut result = result;
        let (result_text, hit_count) = self.redact_text_tracked(&result.text);
        if hit_count > 0 {
            tracing::warn!(
                session = %session_id,
                tool = %call.name,
                hits = hit_count,
                "tool result contained sensitive data; redacted at exit"
            );
        }
        result.text = result_text.clone();
        let result_images = result.images.clone();
        let elapsed_ms = started.elapsed().as_millis() as u64;
        self.emit(BackendEvent::ToolResult {
            session_id: session_id.to_string(),
            team_id: None,
            tool_name: call.name.clone(),
            result: result_text.clone(),
            tool_call_id: call.id.clone(),
            timed_out: false,
            branch_id: branch_id.to_string(),
            elapsed_ms: Some(elapsed_ms),
        });
        self.trunk
            .append_event(echo_session::SessionEvent::ToolResult(
                echo_session::event::ToolResultEvent {
                    tool_call_id: call.id.clone(),
                    result: result_text.clone(),
                    images: result_images.clone(),
                    session: Some(session_id.to_string()),
                    elapsed_ms: Some(elapsed_ms),
                },
            ));
        if call.name == "checklist" {
            // 按会话过滤（2026-10 P0 修复）：此前取全量状态挂当前会话广播
            // ——A 会话触发时 B 会话的数据也会挂到 A 上（面板按会话显示必串）。
            if let Some(state) = self.tools.snapshot_for_session(&call.name, session_id) {
                self.emit(BackendEvent::ChecklistUpdated {
                    session_id: session_id.to_string(),
                    state,
                    team_id: None, // annotate_team 统一补
                });
            }
        }
        result
    }

    /// Record the `ToolResult` for a call whose `run_tool` future was dropped
    /// (timeout or cancellation). `run_tool` appends the `ToolCall` event
    /// before dispatching, so without this the durable log would keep a
    /// dangling call: after a reload the projection synthesizes an assistant
    /// tool_use no result answers, and the provider rejects the request
    /// (HTTP 400). Also emits the backend event so the UI can close out the
    /// pending tool row. `elapsed_ms` = 开始执行到中断判定为止的耗时。
    fn record_interrupted_tool_result(
        &self,
        session_id: &str,
        branch_id: &str,
        call: &ToolCall,
        result: &str,
        timed_out: bool,
        elapsed_ms: u64,
    ) {
        self.emit(BackendEvent::ToolResult {
            session_id: session_id.to_string(),
            team_id: None,
            tool_name: call.name.clone(),
            result: result.to_string(),
            tool_call_id: call.id.clone(),
            timed_out,
            branch_id: branch_id.to_string(),
            elapsed_ms: Some(elapsed_ms),
        });
        self.trunk
            .append_event(echo_session::SessionEvent::ToolResult(
                echo_session::event::ToolResultEvent {
                    tool_call_id: call.id.clone(),
                    result: result.to_string(),
                    images: vec![],
                    session: Some(session_id.to_string()),
                    elapsed_ms: Some(elapsed_ms),
                },
            ));
    }

    /// 工作区会话（workspace 插件）注入提示词的文本：插件对该 persona 启用
    /// 且存在激活会话时返回 Some（名称 + 工作目录清单），否则 None。
    fn workspace_prompt_text(&self) -> Option<String> {
        let store = self.workspace_store()?;
        let plugin_allowed = self
            .capabilities
            .lock()
            .ok()
            .and_then(|cap| cap.clone())
            .map(|cap| {
                crate::plugins::profile_allows_plugin(&cap, crate::plugins::WORKSPACE_PLUGIN_ID)
            })
            .unwrap_or(false);
        if plugin_allowed {
            store.prompt_text()
        } else {
            None
        }
    }

    /// 构建系统提示词块（无输入相关区块的代表性构建，供 `/context` 在无
    /// 最近 turn 时回退使用）。
    async fn build_prompt_blocks(&self) -> Vec<PromptBlock> {
        let base = self.config.read().await.system_prompt.clone();
        let workspace = self.workspace_prompt_text();
        let skills = self.skills.lock().await;
        crate::agent::prompt::build_prompt_blocks(&skills, &base, "", None, &[], workspace).await
    }
    /// Decomposed context blocks for `/context`: the prompt sections of the
    /// most recent turn (falling back to a boundary-free build) plus the
    /// conversation history aggregated per role. The trunk history holds user
    /// / assistant / tool messages only — the system prompt is rebuilt every
    /// turn and is represented by the prompt blocks above.
    pub async fn context_blocks(
        &self,
        history: &[ChatMessage],
    ) -> Vec<crate::event::ContextBlockInfo> {
        let prompt = match self.last_prompt_blocks.lock().await.clone() {
            Some(blocks) => blocks,
            // No turn has run yet in this process — build a representative
            // set without input-specific sections.
            None => self.build_prompt_blocks().await,
        };
        let mut blocks: Vec<crate::event::ContextBlockInfo> = prompt
            .iter()
            .map(|block| crate::event::ContextBlockInfo {
                key: block.key.clone(),
                label: block.label.clone(),
                kind: block.kind.clone(),
                tokens: crate::llm::estimate_message_tokens(&ChatMessage::system(
                    block.content.clone(),
                )),
                chars: block.content.chars().count(),
                content: block.content.clone(),
            })
            .collect();
        // Aggregate the history per role.
        let mut by_role: std::collections::BTreeMap<&str, Vec<&ChatMessage>> =
            std::collections::BTreeMap::new();
        for message in history {
            by_role
                .entry(match message.role {
                    crate::llm::ChatRole::System => "system",
                    crate::llm::ChatRole::User => "user",
                    crate::llm::ChatRole::Assistant => "assistant",
                    crate::llm::ChatRole::Tool => "tool",
                })
                .or_default()
                .push(message);
        }
        for (role, messages) in by_role {
            let tokens: usize = messages
                .iter()
                .map(|message| crate::llm::estimate_message_tokens(message))
                .sum();
            let content = messages
                .iter()
                .map(|message| {
                    let preview = message
                        .content
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let preview: String = preview.chars().take(120).collect();
                    format!(
                        "[{}] {}t {}",
                        role,
                        crate::llm::estimate_message_tokens(message),
                        preview
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            blocks.push(crate::event::ContextBlockInfo {
                key: format!("history:{role}"),
                label: format!("对话历史 · {}", role_label(role)),
                kind: "history".into(),
                tokens,
                chars: content.chars().count(),
                content,
            });
        }
        blocks
    }

    /// System prompt string (tests only; the turn loop uses blocks).
    #[cfg(test)]
    async fn build_system_prompt(&self, content: &str) -> String {
        let skills = self.skills.lock().await;
        let matched = skills.find_matching(content);
        // No skill triggered — use cache if available.
        if matched.is_none() {
            if let Some(ref cached) = *self.system_prompt_cache.read().await {
                return cached.clone();
            }
        }
        let base = self.config.read().await.system_prompt.clone();
        let mut prompt = base;
        if !skills.is_empty() {
            prompt.push_str("\n\n# Available skills\n");
            prompt.push_str(&skills.metadata_lines());
        }
        for skill in skills.always_enabled() {
            prompt.push_str(&format!(
                "\n\n# Active skill: {}\n{}",
                skill.metadata.name, skill.instructions
            ));
        }
        if let Some(matched) = matched {
            prompt.push_str(&format!(
                "\n\n# Triggered skill: {}\n{}",
                matched.metadata.name, matched.instructions
            ));
            return prompt; // don't cache triggered-skill prompts
        }
        // Cache the base+metadata prompt.
        let mut cache = self.system_prompt_cache.write().await;
        *cache = Some(prompt.clone());
        prompt
    }

    /// Emit the current adapter list to subscribers.
    fn emit_adapter_list(&self) {
        let adapters: Vec<AdapterStatus> = self
            .adapters
            .list_info()
            .into_iter()
            .map(|info| AdapterStatus {
                name: info.name,
                display_name: info.display_name,
                connected: matches!(info.status, echo_adapter::AdapterConnectionState::Connected),
                running: !matches!(info.status, echo_adapter::AdapterConnectionState::Stopped),
                self_id: info.self_id,
                bind_address: String::new(), // populated by QqAdapter
                started_at: info.started_at,
                persona: info.persona,
                container: info.container,
                webui_url: info.webui_url,
                onebot_url: info.onebot_url,
            })
            .collect();
        self.emit(BackendEvent::AdapterList { adapters });
    }
}

fn role_label(role: &str) -> &'static str {
    match role {
        "system" => "系统",
        "user" => "用户",
        "assistant" => "助手",
        "tool" => "工具",
        _ => "其他",
    }
}

/// Extract the structured `message_sequence` from a marked input, if any.
/// Parsing is delegated to [`crate::input_marker`] (single source of truth).
pub(crate) fn structured_message_sequence(content: &str) -> Option<u64> {
    crate::input_marker::structured_message_sequence(content)
}

/// 模型可见的工具参数预检：schema 声明的必需字段缺失时，返回纠正性错误
/// [`crate::adapter_bridge::format_hook_input`]); this pulls them into the
/// durable `UserMessage` event so session replay keeps the multimodal
/// content. Returns an empty vec for plain text.
/// 单张图片（URL 或 data URI 字符串）进入持久化/模型请求的上限。
///
/// 上限对齐各入口的内嵌上限：QQ 适配器按解码后 ≤10MB 内嵌
/// （`MAX_EMBEDDED_IMAGE_BYTES`，base64 后约 13.4M 字符），Panel 把 data URL
/// 压到 ≤1.5M 字符。这里留一档余量兜底，只拦真正的异常负载——图片本身
/// 走 image 块，体积不影响 token，超限只发生在编码前的极端输入上。
const MAX_INPUT_IMAGE_CHARS: usize = 16 * 1024 * 1024;

pub(crate) fn extract_input_images(content: &str) -> Vec<String> {
    let Some(payload) = crate::input_marker::hook_payload(content) else {
        return Vec::new();
    };
    let value: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    value["images"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    m.as_str()
                        .filter(|s| s.len() <= MAX_INPUT_IMAGE_CHARS)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn desc_exists(host: &crate::plugins::PluginHost, id: &str) -> bool {
    host.descriptors().iter().any(|d| d.id == id)
}

/// 已挂载的描述符与磁盘 manifest 是否有内容 diff（name/version/description/
/// author/package 任一变化都算 stale）——stale 的数据插件需要重挂载。
/// 挂在函数级便于单测（`reload_data_plugins` 本体依赖完整 Agent）。
fn data_plugin_stale(
    current: &echo_plugin::PluginDescriptor,
    manifest: &echo_plugin::PluginManifest,
) -> bool {
    current.name != manifest.name
        || current.version != manifest.version
        || current.description != manifest.description
        || current.author != manifest.author
        || current.package != manifest.package_id()
}

/// After a delay, generate and send one interim reply for a still-running
/// branch, unless the branch already produced a visible reply or finished.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_contextual_wait_reply(
    agent: Arc<Agent>,
    session: crate::session::Session,
    branch_id: String,
    group_id: Option<String>,
    history_snapshot: Vec<crate::llm::ChatMessage>,
    mut visible_reply: tokio::sync::watch::Receiver<bool>,
    branch_completed: tokio_util::sync::CancellationToken,
    delay: std::time::Duration,
) {
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {
                if branch_completed.is_cancelled() || *visible_reply.borrow() {
                    return;
                }
                let generated = tokio::select! {
                    result = agent.generate_wait_reply(&session.id, history_snapshot) => result,
                    changed = visible_reply.changed() => {
                        let _ = changed;
                        return;
                    }
                    _ = branch_completed.cancelled() => return,
                };
                let reply = match generated {
                    Ok(reply) => reply,
                    Err(error) => {
                        tracing::warn!(%error, session = %session.id, "wait reply generation skipped");
                        return;
                    }
                };
                if branch_completed.is_cancelled() || *visible_reply.borrow() {
                    return;
                }
                if let Err(error) = agent
                    .send_control_reply(&session, group_id.as_deref(), &reply)
                    .await
                {
                    tracing::warn!(%error, session = %session.id, "wait reply send failed");
                    if agent.shows_reply_branches() {
                        agent.emit(BackendEvent::ReplyBranchContent {
                            session_id: session.id.clone(),
                            branch_id,
                            content: format!("临时回复发送失败：{error}\n待发送内容：{reply}"),
                        });
                    }
                } else if agent.shows_reply_branches() {
                    agent.emit(BackendEvent::ReplyBranchContent {
                        session_id: session.id.clone(),
                        branch_id,
                        content: reply,
                    });
                }
            }
            changed = visible_reply.changed() => {
                let _ = changed;
            }
            _ = branch_completed.cancelled() => {}
        }
    });
}

/// 工具超时提示文案（内置循环与 echo 路径**共用同一文案**，2026-10 巡检：
/// 此前双写、易漂移）。
/// 流式增量转发任务（每 step 一个；`AgentContentDelta` / `AgentReasoningDelta`）。
///
/// provider 的 `chat_stream` 把 token 级增量送入 `rx`；本任务按 ~40ms 窗口
/// （或累计 ≥256 字符）合并成批再发射——token 级直发会产生每步几十上百个
/// WS 帧。`rx` 关闭（provider 结束、或调用方 future 被取消丢弃）后做最终
/// flush 并退出；调用方在成功路径上先 `await` 本任务再返回，保证增量先于
/// 本 step 的后续事件（工具卡 / 终值 `AgentOutput`/`AgentReasoning`）送达。
///
/// 事件构造不带 `team_id`（由 emit handle 内的 `annotate_team_for` 统一注入）。
fn spawn_delta_forwarder(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ChatChunk>,
    emit: std::sync::Arc<dyn Fn(BackendEvent) + Send + Sync>,
    session_id: String,
    branch_id: String,
) -> tokio::task::JoinHandle<()> {
    /// 合并窗口：每次至多一个批（前一批未满窗口也送出，保实时观感）。
    const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(40);
    /// 单批字符上限触发条件：大段输出（长句/代码块）免受窗口节流。
    const FLUSH_CHARS: usize = 256;
    tokio::spawn(async move {
        let mut content = String::new();
        let mut reasoning = String::new();
        let flush = |content: &mut String, reasoning: &mut String| {
            // 推理先于正文（上游时序：thinking → text）。
            if !reasoning.is_empty() {
                emit(BackendEvent::AgentReasoningDelta {
                    session_id: session_id.clone(),
                    team_id: None,
                    branch_id: branch_id.clone(),
                    delta: std::mem::take(reasoning),
                });
            }
            if !content.is_empty() {
                emit(BackendEvent::AgentContentDelta {
                    session_id: session_id.clone(),
                    team_id: None,
                    branch_id: branch_id.clone(),
                    delta: std::mem::take(content),
                });
            }
        };
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // 跳过立即触发的第一拍
        loop {
            tokio::select! {
                chunk = rx.recv() => match chunk {
                    Some(chunk) => {
                        if let Some(delta) = chunk.content_delta {
                            content.push_str(&delta);
                        }
                        if let Some(delta) = chunk.reasoning_delta {
                            reasoning.push_str(&delta);
                        }
                        if content.len() + reasoning.len() >= FLUSH_CHARS {
                            flush(&mut content, &mut reasoning);
                        }
                    }
                    None => {
                        flush(&mut content, &mut reasoning);
                        break;
                    }
                },
                _ = ticker.tick() => {
                    if !content.is_empty() || !reasoning.is_empty() {
                        flush(&mut content, &mut reasoning);
                    }
                }
            }
        }
    })
}

fn tool_timeout_notice(tool: &str, secs: u64) -> String {
    format!(
        "notice: tool '{tool}' timed out after {secs}s and its execution was aborted. You may retry this tool (e.g. with a shorter command) or continue the answer directly with the information you already have; do not treat this timeout as a fatal failure."
    )
}

fn assistant_with_tool_calls(
    calls: &[ToolCall],
    content: &Option<String>,
    reasoning_content: &Option<String>,
) -> ChatMessage {
    ChatMessage {
        role: crate::llm::ChatRole::Assistant,
        content: content.clone().unwrap_or_default(),
        reasoning_content: reasoning_content.clone(),
        tool_calls: Some(calls.to_vec()),
        tool_call_id: None,
        images: vec![],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod tests;
