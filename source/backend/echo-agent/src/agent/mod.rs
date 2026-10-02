//! The agent loop: LLM + tools + skills + memory composed into one reply.

mod boundary;
mod commands;
mod compact;
mod prompt;
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
use crate::llm::{create_provider, ChatMessage, ChatRequest, LlmProvider, ToolCall};
use crate::session::{Session, TrunkStore};
use crate::skill::SkillRegistry;
use crate::tool::ToolRegistry;

pub(crate) const TURN_CANCELLED: &str = "agent turn cancelled by requester";
/// 一轮内允许的截断自动续跑次数上限：输出被 max_tokens 截断时把残片入栈
/// 让模型接着写；超过上限按错误上报，不再静默重试。
pub(crate) const MAX_TRUNCATION_CONTINUES: usize = 4;
/// 截断续跑时喂给模型的提示（user 角色，区别于真实用户输入）。
pub(crate) const TRUNCATION_CONTINUE_PROMPT: &str = "[system notice] Your previous output was cut off by the max token limit before the turn was complete. Continue exactly from where you stopped. If you were composing a tool call, discard the partial call and re-issue it in full.";
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
static GLOBAL_PLUGIN_HOST: std::sync::OnceLock<std::sync::Arc<crate::plugins::PluginHost>> =
    std::sync::OnceLock::new();

pub fn plugin_host_global() -> Option<std::sync::Arc<crate::plugins::PluginHost>> {
    GLOBAL_PLUGIN_HOST.get().cloned()
}

pub fn set_plugin_host_global(host: std::sync::Arc<crate::plugins::PluginHost>) {
    let _ = GLOBAL_PLUGIN_HOST.set(host);
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

static GLOBAL_POLICY: std::sync::OnceLock<std::sync::Arc<GlobalPolicy>> =
    std::sync::OnceLock::new();

pub fn global_policy() -> Option<std::sync::Arc<GlobalPolicy>> {
    GLOBAL_POLICY.get().cloned()
}

pub fn set_global_policy(policy: std::sync::Arc<GlobalPolicy>) {
    let _ = GLOBAL_POLICY.set(policy);
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

static FEDERATION_COMMAND_HANDLER: std::sync::OnceLock<FederationCommandHandler> =
    std::sync::OnceLock::new();

pub fn federation_command_handler() -> Option<FederationCommandHandler> {
    FEDERATION_COMMAND_HANDLER.get().cloned()
}

pub fn set_federation_command_handler(handler: FederationCommandHandler) {
    let _ = FEDERATION_COMMAND_HANDLER.set(handler);
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
        // Mount newly seen data plugins (skill/tool kinds).
        for manifest in manifests {
            if desc_exists(host, &manifest.id) {
                continue;
            }
            let kind = manifest.kind;
            if !matches!(
                kind,
                echo_plugin::PluginKind::Skill | echo_plugin::PluginKind::Tool
            ) {
                continue; // code plugins need binary reload
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

    async fn reload_skills(&self, skills_dir: &str) -> Result<bool, String> {
        if !std::path::Path::new(skills_dir).exists() {
            return Ok(false);
        }

        let dir = skills_dir.to_string();
        let mut reloaded = tokio::task::spawn_blocking(move || SkillRegistry::discover(&dir))
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
        tracing::info!(skills = ?names, path = %skills_dir, "skills hot reloaded");
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

    /// Apply per-persona capability configuration（运行期热更新入口）。
    ///
    /// Semantics:
    /// - `enabled_*` allowlists: non-empty => only the listed plugins/tools/
    ///   skills are visible to this agent; empty => everything is available.
    /// - `disabled_*` denylists refine afterwards (keeps old configs working).
    /// - 全局禁用（`[agent].disabled_tools/disabled_skills`、共享注册表中被
    ///   `TogglePlugin` 卸载的插件）优先于 persona 名单。
    ///
    /// 与旧实现的区别：**逐名双向应用**（取消勾选即禁用、重新勾选即恢复，
    /// 此前只有禁用方向，重勾必须重启才生效），且插件维度只作用于本
    /// persona 的工具/技能注册表（不再驱动共享 registry 的
    /// mount/unmount，避免"单人格改动扩散到全员"）。
    pub async fn apply_capabilities(&self, profile: &crate::config::AgentProfile) {
        *self.capabilities.lock().unwrap() = Some(profile.clone());

        // 全局禁用列表：优先管理面（默认 agent）的实时配置——ToggleTool /
        // ToggleSkill 只更新管理面配置，其余 persona 的启动快照会过期。
        let (global_disabled_tools, global_disabled_skills) = self.global_disabled_lists().await;

        // 1) 工具逐名双向：白名单 ∧ 非黑名单 ∧ 非全局禁用。
        for name in self.tools.names() {
            let allowed = persona_tool_allowed(profile, &name)
                && !global_disabled_tools.iter().any(|t| t == &name);
            self.tools.set_enabled(&name, allowed).await;
        }

        // 2) 技能逐名双向：同一语义。
        {
            let mut skills = self.skills.lock().await;
            for name in skills.names() {
                let allowed = persona_skill_allowed(profile, &name)
                    && !global_disabled_skills.iter().any(|s| s == &name);
                skills.set_enabled(&name, allowed);
            }
        }
        *self.system_prompt_cache.write().await = None;

        // 3) 门控插件（包/表维度）：仅在"不允许"时禁用——与启动期逐人格
        //    门控同语义；重新允许的恢复由上面的逐名双向步骤完成。全局状态
        //    （注册表）优先：运行期 TogglePlugin 的卸载不会被本步骤反向覆盖。
        for plugin_id in crate::plugins::GATED_PLUGIN_IDS {
            let allowed = crate::plugins::profile_allows_plugin(profile, plugin_id)
                && self.plugin_globally_enabled(plugin_id);
            if !allowed {
                self.apply_plugin_gating(plugin_id, false);
            }
        }
    }

    /// 全局禁用列表（工具/技能）：读进程级策略（组合根注入）。
    /// 无策略（单元测试）时回退本 agent 的配置快照。
    async fn global_disabled_lists(&self) -> (Vec<String>, Vec<String>) {
        if let Some(policy) = global_policy() {
            return (policy.disabled_tools(), policy.disabled_skills());
        }
        let cfg = self.config.read().await;
        (cfg.disabled_tools.clone(), cfg.disabled_skills.clone())
    }

    /// 本 persona 白/黑名单是否允许某插件（capabilities 未设置 = 允许）。
    pub fn persona_allows_plugin(&self, plugin_id: &str) -> bool {
        self.capabilities
            .lock()
            .ok()
            .and_then(|guard| {
                guard
                    .as_ref()
                    .map(|p| crate::plugins::profile_allows_plugin(p, plugin_id))
            })
            .unwrap_or(true)
    }

    /// 共享注册表中该插件当前是否启用（无全局锚点 = true，以名单为准）。
    fn plugin_globally_enabled(&self, plugin_id: &str) -> bool {
        plugin_host_global()
            .map(|host| host.registry.is_enabled(plugin_id))
            .unwrap_or(true)
    }

    /// 全局插件状态变化后，重新评估该插件在本 persona 的最终效果：
    /// 最终 = 全局启用 ∧ 本 persona 白/黑名单（禁用对全员生效）。
    ///
    /// 全局 mount/unmount 闭包逐 persona 调用，取代旧的无条件批量启停——
    /// 后者会在 mount 时把"名单外"的 persona 一并放开。
    pub fn reapply_plugin_gating(&self, plugin_id: &str, globally_enabled: bool) {
        let allowed = globally_enabled && self.persona_allows_plugin(plugin_id);
        self.apply_plugin_gating(plugin_id, allowed);
    }

    /// 全局工具启停变化后，重新评估本 persona 的最终状态：
    /// 最终 = 全局启用 ∧ 本 persona 名单。返回工具是否存在。
    pub async fn reapply_tool_gating(&self, name: &str, globally_enabled: bool) -> bool {
        let allowed = globally_enabled
            && self
                .capabilities
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(|p| persona_tool_allowed(p, name)))
                .unwrap_or(true);
        self.tools.set_enabled(name, allowed).await
    }

    /// 全局技能启停变化后，重新评估本 persona 的最终状态。返回技能是否存在。
    pub async fn reapply_skill_gating(&self, name: &str, globally_enabled: bool) -> bool {
        let allowed = globally_enabled
            && self
                .capabilities
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(|p| persona_skill_allowed(p, name)))
                .unwrap_or(true);
        let ok = {
            let mut skills = self.skills.lock().await;
            skills.set_enabled(name, allowed)
        };
        if ok {
            *self.system_prompt_cache.write().await = None;
        }
        ok
    }

    /// Persona 级 API：设置本 agent 对全局供应商池的引用（不重建）。
    /// `None` = 跟随全局默认配置；`Some(name)` = 使用池中该 profile。
    pub async fn set_persona_api(&self, name: Option<String>) {
        *self.persona_api.write().await = name;
    }

    /// 同步版 [`Self::set_persona_api`]：供组合根启动期（make_agent 为同步
    /// 闭包）调用；启动期无人持有读锁，try_write 必然成功。
    pub fn set_persona_api_now(&self, name: Option<String>) {
        if let Ok(mut slot) = self.persona_api.try_write() {
            *slot = name;
        }
    }

    /// 当前 persona 级的 API 供应商引用。
    pub async fn persona_api(&self) -> Option<String> {
        self.persona_api.read().await.clone()
    }

    /// Persona 级 API：按引用重建本 agent 的 provider。
    ///
    /// 以传入的**全局配置**（含最新供应商池）为基准：
    /// - `Some(name)` 且池中存在 → 合入该 profile 值（非空覆盖），
    ///   不改变全局 active_api / 顶层字段；
    /// - 其他情况（None 或名字不存在）→ 跟随全局默认（顶层 + active_api）。
    ///
    /// 返回是否成功重建；失败时保留旧 provider 并 emit Error。
    pub async fn apply_persona_api(&self, global: &AgentConfig) -> bool {
        let reference = self.persona_api.read().await.clone();
        let mut resolved = global.clone();
        let ok = match reference.as_deref().filter(|n| !n.is_empty()) {
            Some(name) => resolved.apply_named_profile(name),
            None => {
                resolved.apply_active_profile();
                true
            }
        };
        if !ok {
            self.emit(BackendEvent::Error {
                session_id: None,
                message: format!(
                    "persona API profile not found in pool: {} — falling back to global default",
                    reference.unwrap_or_default()
                ),
            });
            resolved.apply_active_profile();
        }
        resolved.api_key = resolved.effective_api_key();
        resolved.base_url = resolved.effective_base_url();
        // 本 persona 的 config 只更新 API 相关字段（保留权限、预算等其余项）。
        {
            let mut cfg = self.config.write().await;
            cfg.provider = resolved.provider.clone();
            cfg.model = resolved.model.clone();
            cfg.base_url = resolved.base_url.clone();
            cfg.api_key = resolved.api_key.clone();
            cfg.thinking = resolved.thinking;
            cfg.reasoning_effort = resolved.reasoning_effort;
        }
        let provider = match crate::llm::create_provider(&resolved) {
            Ok(p) => p,
            Err(e) => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("persona API provider build failed: {e}"),
                });
                return false;
            }
        };
        *self.provider.write().await = Arc::from(provider);
        self.set_model(resolved.model.clone()).await;
        true
    }

    /// 插件启停对本 agent 注册表的批量效果（**包维度，横跨工具与技能**）。
    ///
    /// Package 是横跨 plugin + tool + skill 的标签：本方法把一次包级
    /// 启停传播到本 agent 的两类注册表——
    /// - 工具：`ToolRegistry::set_package_enabled`（按工具的 package 标签）；
    /// - 技能：`SkillRegistry::set_package_enabled`（按 `SKILL.md` 的
    ///   `package:` frontmatter）——`skills.dir` 插件为**整表**语义，
    ///   其余包按同名 package 精确匹配（如 QQ 包：qq-management /
    ///   qq-transport 随 `echo-agent.adapter.qq` 一起启停）。
    ///
    /// 启用方向会按本 persona 的工具/技能名单**收紧**：整包启用不越过
    /// 白/黑名单（名单外的成员保持禁用），与启动期"名单先应用、包后禁用"
    /// 的组合语义一致。
    pub fn apply_plugin_gating(&self, plugin_id: &str, enabled: bool) {
        // 工具维度：按 package 标签批量启停。
        let affected_tools = self.tools.set_package_enabled(plugin_id, enabled);
        if enabled {
            self.tighten_tools_by_lists(plugin_id);
        }
        if affected_tools > 0 {
            tracing::info!(
                plugin = plugin_id,
                enabled,
                affected = affected_tools,
                "plugin tool package toggled"
            );
        }

        // 技能维度：Package 标签横跨技能，同名包内技能随包启停。
        // （锁竞争时跳过——下一周期/切换时再应用。）
        if let Ok(mut skills) = self.skills.try_lock() {
            let cap = self.capabilities.lock().ok().and_then(|g| g.clone());
            let affected_skills = if plugin_id == crate::plugins::SKILLS_DIR_PLUGIN_ID {
                // 技能目录插件：整表启停。启用时只放开名单内的技能。
                let names = skills.names();
                for name in &names {
                    let ok = cap.as_ref().is_none_or(|p| persona_skill_allowed(p, name));
                    skills.set_enabled(name, enabled && ok);
                }
                names.len()
            } else {
                let affected = skills.set_package_enabled(plugin_id, enabled);
                if enabled {
                    // 收紧：包启用不越过 persona 技能名单。
                    if let Some(cap) = cap.as_ref() {
                        for name in skills.package_names(plugin_id) {
                            if !persona_skill_allowed(cap, &name) {
                                skills.set_enabled(&name, false);
                            }
                        }
                    }
                }
                affected
            };
            if affected_skills > 0 {
                if plugin_id == crate::plugins::SKILLS_DIR_PLUGIN_ID {
                    tracing::info!(
                        enabled,
                        affected = affected_skills,
                        "skills dir plugin toggled"
                    );
                } else {
                    tracing::info!(
                        plugin = plugin_id,
                        enabled,
                        affected = affected_skills,
                        "plugin skill package toggled"
                    );
                }
            }
        }
    }

    /// 按本 persona 的工具名单，把包内"名单外"的成员重新禁用。
    /// 同步尽力（try_write）；未配置 capabilities 时为 no-op。
    fn tighten_tools_by_lists(&self, package: &str) {
        let Some(cap) = self.capabilities.lock().ok().and_then(|g| g.clone()) else {
            return;
        };
        if cap.enabled_tools.is_empty() && cap.disabled_tools.is_empty() {
            return;
        }
        for name in self.tools.package_names(package) {
            if !persona_tool_allowed(&cap, &name) {
                self.tools.try_disable(&name);
            }
        }
    }

    /// Whether a dynamic tool is allowed for this agent
    /// (allowlist first, denylist refinement; empty allowlist = all allowed).
    ///
    /// 单会话模式额外隐藏 `spawn_parallel_task`：一个会话一次只处理一件事，
    /// 并行分支与串行准入语义冲突（改用另一个会话 = 另一条并行通道）。
    pub fn allows_dynamic_tool(&self, name: &str) -> bool {
        if name == "spawn_parallel_task" && self.loop_mode() == echo_defs::LoopMode::Single {
            return false;
        }
        // spawn_subagent 由 subagent 插件实化：插件未挂载（未装配运行态）
        // 或 persona 白名单不含该插件时对模型不可见。
        if name == crate::subagent::SPAWN_SUBAGENT_TOOL {
            let attached = self.subagent.read().map(|g| g.is_some()).unwrap_or(false);
            return attached && self.persona_allows_plugin(crate::plugins::SUBAGENT_PLUGIN_ID);
        }
        let guard = self.capabilities.lock().unwrap();
        let Some(cap) = guard.as_ref() else {
            return true;
        };
        if !cap.enabled_tools.is_empty() && !cap.enabled_tools.iter().any(|t| t == name) {
            return false;
        }
        if cap.disabled_tools.iter().any(|t| t == name) {
            return false;
        }
        true
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

    /// 装配 subagent 插件运行态（插件 mount 时由组合根调用）。
    ///
    /// 同时接线 spawn 执行闭包：子任务以隔离上下文后台执行，完成时经
    /// `<subagent_event>` hook（`crate::subagent::wrap_subagent_event`）作为
    /// 新入站分支通知主 agent——hook 机制由 echo-loop 的
    /// `SubagentToolHooks` 定义，QQ 消息等入站复用同一「结构化 hook →
    /// 新 turn」路径。
    pub fn attach_subagent_runtime(self: &Arc<Self>, store: Arc<crate::subagent::SubagentStore>) {
        let agent = Arc::downgrade(self);
        let spawn: SubagentSpawnFn = Arc::new(move |request| {
            if let Some(agent) = agent.upgrade() {
                agent.spawn_subagent_execution(request);
            }
        });
        if let Ok(mut slot) = self.subagent.write() {
            *slot = Some(SubagentRuntime {
                store: store.clone(),
                spawn,
            });
        } else {
            tracing::warn!("subagent slot busy, ignoring attach_subagent_runtime");
            return;
        }
        store.spawn_sweeper(self.cancel.clone());
    }

    /// 卸载 subagent 插件运行态（插件 unmount）：取消运行中子任务并摘下
    /// 工具可见性（`allows_dynamic_tool` 随即拒绝 spawn_subagent）。
    pub fn detach_subagent_runtime(&self) {
        let runtime = self.subagent.write().ok().and_then(|mut slot| slot.take());
        if let Some(runtime) = runtime {
            runtime.store.cancel_all();
        }
    }

    /// subagent 运行态（None = 插件未启用）。
    pub(crate) fn subagent_runtime(
        &self,
    ) -> Option<(Arc<crate::subagent::SubagentStore>, SubagentSpawnFn)> {
        let guard = self.subagent.read().ok()?;
        let runtime = guard.as_ref()?;
        Some((runtime.store.clone(), runtime.spawn.clone()))
    }

    /// spawn 执行体：后台以隔离上下文跑子任务，完成/失败/超时/取消都经
    /// hook 通知主 agent（恰好一次）。
    fn spawn_subagent_execution(self: &Arc<Self>, request: crate::subagent::SpawnRequest) {
        let agent = Arc::clone(self);
        tokio::spawn(async move {
            let crate::subagent::SpawnRequest {
                task_id,
                session_id,
                task,
                timeout,
                parent_cancel,
                parent_branch_id,
                node,
            } = request;
            let (store, _) = match agent.subagent_runtime() {
                Some(runtime) => runtime,
                None => return,
            };
            // 执行体监听**注册表条目的令牌**（store.cancel_all / 逐项 finish 都取消
            // 它）；它与 parent_cancel 是同一传播链（spawn 时 register 存的就是
            // parent 的 child_token），主 turn 取消同样经 parent 链传导到该令牌。
            let cancel = store
                .cancel_token_of(&task_id)
                .unwrap_or_else(|| parent_cancel.child_token());
            agent.emit(BackendEvent::SubagentStarted {
                session_id: session_id.clone(),
                task: task.clone(),
            });
            // 子 agent 上下文：base 提示词 + 工具集（剥离 spawn_subagent，
            // 单层委派）。
            let base = agent.config.read().await.system_prompt.clone();
            let mut tools = (*agent.tools.definitions().await).clone();
            tools.retain(|d| d.name != crate::subagent::SPAWN_SUBAGENT_TOOL);
            let registry = Arc::clone(&agent.tools);
            // federation Phase 3：`node` 指定时子任务工具视图指向远程
            // 节点——LLM 仍在本机推理，工具定义替换为该 peer 的代理工具
            // 描述（`<peer>:<tool>` 已注册进注册表，执行经 Invoke 路由）。
            // 语义说明（RFC §5）：远程 subagent 的任务文本不感知本机
            // 工作区，工具列表即"在远程能做什么"的完整能力面。
            if let Some(ref peer) = node {
                let prefix = format!("{peer}:");
                let remote_defs: Vec<echo_defs::tool::ToolDefinition> = tools
                    .iter()
                    .filter(|d| d.name.starts_with(&prefix))
                    .cloned()
                    .map(|mut d| {
                        // 剥前缀：模型在远程语境下用本机工具名调用，
                        // 执行端（packages/federation 的路由）按前缀还原。
                        d.name = d.name.trim_start_matches(&prefix).to_string();
                        d
                    })
                    .collect();
                if remote_defs.is_empty() {
                    tracing::warn!(peer = %peer, "remote subagent: no proxy tools available (peer offline or federation disabled)");
                } else {
                    tools = remote_defs;
                }
            }
            let provider = agent.provider.read().await.clone();
            let max_iterations = agent.config.read().await.max_tool_iterations;
            let max_tokens = agent.config.read().await.effective_max_tokens();
            // federation Phase 3 对端观测：远程子任务受理时通知对端
            // （SubagentSpawn），完成/失败/取消时回报（SubagentEvent）——
            // 对端 Panel 后台任务列表据此可见、可强制取消。
            // 通知经进程级 federation 出口（组合根装配时注入）。
            if let Some(ref peer) = node {
                crate::federation::notify_remote_subagent(
                    peer,
                    &task_id,
                    &task,
                    Some(timeout.as_secs()),
                    crate::federation::SubagentStatus::Running,
                    None,
                );
            }
            let run = crate::subagent::run_subagent_turn(
                provider,
                tools,
                registry,
                base,
                task.clone(),
                cancel.clone(),
                max_iterations,
                max_tokens,
                node.as_deref(),
            );
            let outcome = tokio::select! {
                result = run => Ok(result),
                _ = tokio::time::sleep(timeout) => Err("timeout"),
                _ = cancel.cancelled() => Err("cancelled"),
            };
            let (success, cancelled, detail) = match outcome {
                Ok(Ok(reply)) => (true, false, crate::subagent::truncate_result(&reply)),
                Ok(Err(error)) if Agent::is_turn_cancelled(&error) => {
                    (false, true, "子任务已随主任务取消".into())
                }
                Ok(Err(error)) => (false, false, format!("子任务执行失败：{error}")),
                Err("timeout") => (
                    false,
                    false,
                    format!("子任务超时（{}s）", timeout.as_secs()),
                ),
                Err(_) => (false, true, "子任务已随主任务取消".into()),
            };
            let status = if success {
                crate::subagent::SubagentStatus::Completed
            } else if cancelled {
                crate::subagent::SubagentStatus::Cancelled
            } else {
                crate::subagent::SubagentStatus::Failed
            };
            store.finish(&task_id, status);
            if let Some(ref peer) = node {
                let fed_status = match status {
                    crate::subagent::SubagentStatus::Completed => {
                        crate::federation::SubagentStatus::Completed
                    }
                    crate::subagent::SubagentStatus::Cancelled => {
                        crate::federation::SubagentStatus::Cancelled
                    }
                    _ => crate::federation::SubagentStatus::Failed,
                };
                crate::federation::notify_remote_subagent(
                    peer,
                    &task_id,
                    &task,
                    None,
                    fed_status,
                    Some(detail.clone()),
                );
            }
            agent.emit(BackendEvent::SubagentCompleted {
                session_id: session_id.clone(),
                success,
            });
            // hook 回灌主 agent：作为该会话的全新入站分支（主 turn 已结束，
            // 结论需要新的 turn 来消化；与 QQ hook / timer 事件同族）。
            let payload = serde_json::json!({
                "event": "subagent_event",
                "subagent_id": task_id,
                "session_id": session_id,
                "task": crate::llm::truncate(&task, 500),
                "success": success,
                "result": detail,
                "parent_branch_id": parent_branch_id,
                "completed_at_ms": chrono::Utc::now().timestamp_millis(),
            });
            let hook = crate::subagent::wrap_subagent_event(&payload);
            agent.dispatch_subagent_hook(&session_id, hook).await;
        });
    }

    /// 把子任务完成 hook 作为新入站分支注入主会话（内部复用
    /// `process_inbound_branch` 的完整生命周期；`group_id=None` = 后台来源，
    /// 回复只进后台，不推送外部平台）。
    async fn dispatch_subagent_hook(self: &Arc<Self>, session_id: &str, hook: String) {
        let Some(session) = self.trunk.get(session_id) else {
            tracing::warn!(session = %session_id, "subagent hook target session gone");
            return;
        };
        let message_sequence = self.next_message_sequence();
        self.emit(BackendEvent::MessageReceived {
            session_id: session_id.to_string(),
            adapter_name: "subagent".into(),
            platform: "subagent".into(),
            user_id: "subagent".into(),
            user_name: "subagent".into(),
            channel: "direct".into(),
            group_name: None,
            content: hook.clone(),
            images: vec![],
            timestamp: chrono::Utc::now().timestamp(),
            received_at_ms: chrono::Utc::now().timestamp_millis(),
            message_sequence,
            team_id: None,
        });
        self.process_inbound_branch(
            &session,
            &hook,
            message_sequence,
            None,
            std::time::Duration::from_secs(u64::MAX),
        )
        .await;
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
        // 其生命周期 ≥ 任何借用它的命令处理。这里用 Weak 防循环。
        let bus = self.event_bus.clone();
        let sink = self.event_sink.read().ok().and_then(|g| g.clone());
        let handle = self.handle.try_read().ok().and_then(|g| g.clone());
        let team = self.team_id();
        std::sync::Arc::new(move |event| {
            // annotate_team 的轻量版：核心服务代理 team 为 None 时事件
            // 原样（联邦事件无会话归属，无需标注）。
            let _ = &team;
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
        let team_id = self.team_id();
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
                ..
            } => BackendEvent::ToolCall {
                session_id,
                team_id,
                tool_name,
                arguments,
                tool_call_id,
                branch_id,
            },
            BackendEvent::ToolResult {
                session_id,
                tool_name,
                result,
                tool_call_id,
                timed_out,
                branch_id,
                ..
            } => BackendEvent::ToolResult {
                session_id,
                team_id,
                tool_name,
                result,
                tool_call_id,
                timed_out,
                branch_id,
            },
            BackendEvent::AgentThinking { session_id, .. } => BackendEvent::AgentThinking {
                session_id,
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
        self.trunk
            .append_event(echo_session::SessionEvent::UserMessage(
                echo_session::event::UserMessage {
                    content: content.to_string(),
                    timestamp: chrono::Utc::now().timestamp(),
                    message_sequence: structured_message_sequence(content),
                    source: None,
                    images: extract_input_images(content),
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

    /// Process one user message through the agent loop and return the reply.
    pub async fn process_message(&self, session: &Session, content: &str) -> Result<String> {
        if self.draining.load(Ordering::Acquire) {
            return Err(anyhow!("Core 正在重启，消息暂未处理，请稍后重试"));
        }
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
        let content = content.to_string();
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
        let (registration, history_snapshot, slot) = self
            .register_incoming_branch(&session, &content, message_sequence)
            .await
            .expect("parallel mode never cancels before the branch starts");
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
    async fn process_via_echo_loop(
        &self,
        session: &Session,
        content: &str,
        history_snapshot: Option<Vec<ChatMessage>>,
        turn_cancel: tokio_util::sync::CancellationToken,
        branch_id: &str,
        runner: std::sync::Arc<echo_loop::runner::TurnRunner>,
    ) -> Result<String> {
        let session_id = session.id.clone();
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
        let history = history_snapshot.unwrap_or_else(|| session.history.blocking_lock().clone());
        // 工具 schema：注册表是唯一真源（与内置循环同源）。编排工具
        // （spawn_subagent）同样由注册表提供（组合根装配时注册），这里不得
        // 再追加同名定义——重复工具名会让 API 拒绝整个请求
        // （"Tool names must be unique"）。
        let tools = (*self.tools.definitions().await).clone();

        // 推理回调（引用式，生命周期 = run 调用作用域）。
        let reasoning_cb = |sid: String, text: String| {
            self.emit_reasoning(&sid, branch_id, &Some(text));
        };

        // 工具执行：同步闭包内用 block_in_place + 当前运行时 block_on 执行
        // run_tool（run_tool 内部已发 ToolCall/ToolResult 事件并写事件日志）。
        // 外圈超时守卫与内置循环同一口径（`tool_guard_timeout`）——超时只中止
        // 单个工具、不中断 turn，结果以 notice 喂回模型；被 drop 的 future 已
        // 记录 ToolCall 事件，补记中断结果保持事件日志成对。
        let result = {
            let this = self;
            let executor = |sid: &str, _branch: &str, call: &crate::llm::ToolCall| {
                tokio::task::block_in_place(|| {
                    let handle = tokio::runtime::Handle::current();
                    let guard = handle.block_on(this.tool_guard_timeout(call));
                    match handle.block_on(tokio::time::timeout(
                        guard,
                        this.run_tool(sid, branch_id, call),
                    )) {
                        Ok(result) => result.text,
                        Err(_) => {
                            let text = format!(
                                "notice: tool '{}' timed out after {}s and its execution was aborted. You may retry this tool (e.g. with a shorter command) or continue the answer directly with the information you already have; do not treat this timeout as a fatal failure.",
                                call.name,
                                guard.as_secs(),
                            );
                            this.record_interrupted_tool_result(sid, branch_id, call, &text, true);
                            text
                        }
                    }
                })
            };
            // Subagent hook（echo-loop 的 hook 注入接口）：spawn_subagent 的
            // future 不 Send，无法走上面的 block_in_place 同步桥——经
            // `execute_async` 通道直接进管线（模型可见的 schema 由注册表
            // 统一提供，不再经 extra_tool_definitions 追加）。
            // spawn_subagent 本身是同步受理（注册 + 后台拉起），结果立即可得——
            // 同步 body 内一次性算好文本，再包一个 'static ready future 返回
            // （AsyncToolExecutor 的设计意图：harness 编排需要 tokio::spawn 时
            // 经通道把结果带回，这里无需 spawn 因此直接 ready）。
            let is_async_subagent = |name: &str| name == crate::subagent::SPAWN_SUBAGENT_TOOL;
            let execute_async_subagent = move |sid: &str,
                                               _branch: &str,
                                               call: &crate::llm::ToolCall|
                  -> std::pin::Pin<
                Box<dyn std::future::Future<Output = String> + Send>,
            > {
                let args: serde_json::Value =
                    serde_json::from_str(&call.arguments).unwrap_or_default();
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
                Box::pin(async move { text })
            };
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
            };
            runner
                .run(
                    &session_id,
                    content.to_string(),
                    system_prompt,
                    history,
                    turn_cancel,
                    &executor,
                    extras,
                )
                .await
        };

        match result {
            Ok(reply) => Ok(reply),
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
            let response = tokio::select! {
                response = provider.chat(&request) => response.map_err(|error| {
                    tracing::warn!(
                        turn_id = %turn_id,
                        message_sequence = message_sequence.unwrap_or_default(),
                        session = %session_id,
                        %error,
                        elapsed_ms = queued_at.elapsed().as_millis() as u64,
                        "agent turn model request failed"
                    );
                    error
                })?,
                _ = turn_cancel.cancelled() => return Err(anyhow!(TURN_CANCELLED)),
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
                        let text = format!(
                            "notice: tool '{}' timed out after {}s and its execution was aborted. You may retry this tool (e.g. with a shorter command) or continue the answer directly with the information you already have; do not treat this timeout as a fatal failure.",
                            call.name,
                            tool_timeout.as_secs(),
                        );
                        // run_tool was dropped mid-flight: it already recorded
                        // the ToolCall event, so record the matching ToolResult
                        // or the durable log keeps a dangling call.
                        self.record_interrupted_tool_result(&session_id, branch_id, call, &text, true);
                        crate::tool::ToolResult::text(text)
                    }
                    _ = turn_cancel.cancelled() => {
                        self.record_interrupted_tool_result(
                            &session_id,
                            branch_id,
                            call,
                            "error: tool execution cancelled",
                            false,
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
        self.emit(BackendEvent::ToolCall {
            session_id: session_id.to_string(),
            team_id: None,
            tool_name: call.name.clone(),
            arguments: call.arguments.clone(),
            tool_call_id: call.id.clone(),
            branch_id: branch_id.to_string(),
        });
        // The tool call is a durable event: the model-visible loop (call +
        // result) must be reconstructable from the log after a reload.
        self.trunk
            .append_event(echo_session::SessionEvent::ToolCall(
                echo_session::event::ToolCallEvent {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    session: Some(session_id.to_string()),
                },
            ));
        // 参数解析与预检：给模型可纠正的错误反馈。非法 JSON 或缺少必需
        // 字段时，错误文案必须说清"你发了什么、应该发什么"——一句模糊的
        // "command required" 只会让模型原样重试，形成空调用退化循环。
        crate::shell::set_tool_team_id(self.team_id());
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
                    None => self
                        .tools
                        .execute_rich(other, args)
                        .await
                        .map_err(|error| error.to_string()),
                }
            }
        }
        .unwrap_or_else(|error| crate::tool::ToolResult::text(format!("error: {error}"))),
        };
        crate::shell::set_tool_team_id(None);
        let result_text = result.text.clone();
        let result_images = result.images.clone();
        self.emit(BackendEvent::ToolResult {
            session_id: session_id.to_string(),
            team_id: None,
            tool_name: call.name.clone(),
            result: result_text.clone(),
            tool_call_id: call.id.clone(),
            timed_out: false,
            branch_id: branch_id.to_string(),
        });
        self.trunk
            .append_event(echo_session::SessionEvent::ToolResult(
                echo_session::event::ToolResultEvent {
                    tool_call_id: call.id.clone(),
                    result: result_text.clone(),
                    images: result_images.clone(),
                    session: Some(session_id.to_string()),
                },
            ));
        if call.name == "checklist" {
            if let Some(state) = self.tools.snapshot(&call.name) {
                self.emit(BackendEvent::ChecklistUpdated {
                    session_id: session_id.to_string(),
                    state,
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
    /// pending tool row.
    fn record_interrupted_tool_result(
        &self,
        session_id: &str,
        branch_id: &str,
        call: &ToolCall,
        result: &str,
        timed_out: bool,
    ) {
        self.emit(BackendEvent::ToolResult {
            session_id: session_id.to_string(),
            team_id: None,
            tool_name: call.name.clone(),
            result: result.to_string(),
            tool_call_id: call.id.clone(),
            timed_out,
            branch_id: branch_id.to_string(),
        });
        self.trunk
            .append_event(echo_session::SessionEvent::ToolResult(
                echo_session::event::ToolResultEvent {
                    tool_call_id: call.id.clone(),
                    result: result.to_string(),
                    images: vec![],
                    session: Some(session_id.to_string()),
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

    /// Current API configuration summary (for the TUI settings form).
    pub async fn api_config(&self) -> AgentConfig {
        self.config.read().await.clone()
    }

    /// Emit the current API configuration to subscribers.
    pub async fn emit_api_config(&self) {
        let cfg = self.api_config().await;
        let mut resolved = cfg.clone();
        resolved.apply_active_profile();
        let key_set = !resolved.effective_api_key().is_empty();
        let profiles: Vec<crate::event::ApiProfileInfo> = cfg
            .api_profiles
            .iter()
            .map(|p| crate::event::ApiProfileInfo {
                name: p.name.clone(),
                provider: p.provider.clone(),
                model: p.model.clone(),
                base_url: p.base_url.clone(),
                api_key_set: !p.api_key.is_empty(),
                thinking: p.thinking,
                reasoning_effort: p.reasoning_effort,
            })
            .collect();
        self.emit(BackendEvent::ApiConfigUpdated {
            provider: resolved.provider.clone(),
            model: self.active_model().await,
            base_url: resolved.effective_base_url(),
            api_key_set: key_set,
            thinking: resolved.thinking,
            reasoning_effort: resolved.reasoning_effort,
            system_prompt: cfg.system_prompt.clone(),
            active_api: cfg.active_api.clone(),
            profiles: profiles.clone(),
        });
        self.emit(BackendEvent::ApiProfilesUpdated {
            active_api: cfg.active_api.clone(),
            profiles,
        });
    }

    /// Rebuild the LLM provider from the current config snapshot.
    async fn rebuild_provider(&self, cfg: &AgentConfig) -> bool {
        let mut resolved = cfg.clone();
        resolved.apply_active_profile();
        resolved.api_key = resolved.effective_api_key();
        resolved.base_url = resolved.effective_base_url();
        match crate::llm::create_provider(&resolved) {
            Ok(p) => {
                *self.provider.write().await = Arc::from(p);
                true
            }
            Err(e) => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("failed to build provider: {e}"),
                });
                false
            }
        }
    }

    /// Save an API profile (or the top-level default when `name` is empty),
    /// activate it, rebuild the provider, and persist.
    #[allow(clippy::too_many_arguments)]
    async fn update_api_config(
        &self,
        name: String,
        provider: String,
        model: String,
        base_url: String,
        api_key: String,
        thinking: Option<crate::config::ThinkingMode>,
        reasoning_effort: Option<crate::config::ReasoningEffort>,
    ) {
        let mut config = self.config.write().await;
        let keep_key = api_key.is_empty();
        // 新建命名 profile 时，空的 provider/model/base_url 从当前生效配置
        // 继承，避免写出残缺 profile：残缺 profile 单独做 TestApi 时必然
        // "provider build failed"，且极易误导用户以为 LLM 整体不可用。
        // （更新已有 profile 时保持"空 = 保留该 profile 原值"的语义。）
        let (provider, model, base_url) =
            if !name.is_empty() && !config.api_profiles.iter().any(|p| p.name == name) {
                let mut resolved = config.clone();
                resolved.apply_active_profile();
                (
                    if provider.is_empty() {
                        resolved.provider
                    } else {
                        provider
                    },
                    if model.is_empty() {
                        resolved.model
                    } else {
                        model
                    },
                    if base_url.is_empty() {
                        resolved.base_url
                    } else {
                        base_url
                    },
                )
            } else {
                (provider, model, base_url)
            };
        if name.is_empty() {
            if !provider.is_empty() {
                config.provider = provider.clone();
            }
            if !model.is_empty() {
                config.model = model.clone();
            }
            if !base_url.is_empty() {
                config.base_url = base_url.clone();
            }
            if !keep_key {
                config.api_key = api_key.clone();
            }
            if let Some(thinking) = thinking {
                config.thinking = thinking;
            }
            if let Some(reasoning_effort) = reasoning_effort {
                config.reasoning_effort = reasoning_effort;
            }
            // Don't clear active_api — let explicit SwitchApi handle that.
        } else {
            let profile = match config.api_profiles.iter_mut().find(|p| p.name == name) {
                Some(p) => p,
                None => {
                    config.api_profiles.push(ApiProfile::new(
                        name.clone(),
                        provider.clone(),
                        model.clone(),
                    ));
                    let idx = config.api_profiles.len() - 1;
                    &mut config.api_profiles[idx]
                }
            };
            if !provider.is_empty() {
                profile.provider = provider.clone();
            }
            if !model.is_empty() {
                profile.model = model.clone();
            }
            if !base_url.is_empty() {
                profile.base_url = base_url.clone();
            }
            if !keep_key {
                profile.api_key = api_key.clone();
            }
            if let Some(thinking) = thinking {
                profile.thinking = thinking;
            }
            if let Some(reasoning_effort) = reasoning_effort {
                profile.reasoning_effort = reasoning_effort;
            }
            config.active_api = name.clone();
        }
        let cfg_snapshot = config.clone();
        drop(config);

        self.set_model(model.clone()).await;
        if self.rebuild_provider(&cfg_snapshot).await {
            self.emit(BackendEvent::Error {
                session_id: None,
                message: if name.is_empty() {
                    format!("default API updated: {provider} / {model}")
                } else {
                    format!("API saved and activated: {name} ({provider} / {model})")
                },
            });
        }
        self.emit_api_config().await;
        self.persist_config(&cfg_snapshot).await;
    }

    /// Switch to an API profile (or the top-level default when name is empty).
    async fn switch_api(&self, name: &str) {
        let mut config = self.config.write().await;
        if name.is_empty() {
            config.active_api.clear();
        } else {
            if !config.api_profiles.iter().any(|p| p.name == name) {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("API config not found: {name}"),
                });
                return;
            }
            config.active_api = name.to_string();
        }
        let cfg_snapshot = config.clone();
        let mut resolved = cfg_snapshot.clone();
        resolved.apply_active_profile();
        let model = resolved.model.clone();
        drop(config);

        if self.rebuild_provider(&cfg_snapshot).await {
            self.set_model(model).await;
            let label = if name.is_empty() {
                "default config".to_string()
            } else {
                name.to_string()
            };
            self.emit(BackendEvent::Error {
                session_id: None,
                message: format!("switched to API: {label}"),
            });
        }
        self.emit_api_config().await;
        self.persist_config(&cfg_snapshot).await;
    }

    /// Delete an API profile; if it was active, fall back to the top-level default.
    async fn delete_api(&self, name: &str) {
        let mut config = self.config.write().await;
        let before = config.api_profiles.len();
        config.api_profiles.retain(|p| p.name != name);
        let removed = config.api_profiles.len() < before;
        if removed && config.active_api == name {
            config.active_api.clear();
        }
        let cfg_snapshot = config.clone();
        drop(config);

        if removed {
            if self.rebuild_provider(&cfg_snapshot).await {
                self.set_model(cfg_snapshot.model.clone()).await;
            }
            self.emit(BackendEvent::Error {
                session_id: None,
                message: format!("deleted API config: {name}"),
            });
            self.emit_api_config().await;
            self.persist_config(&cfg_snapshot).await;
        } else {
            self.emit(BackendEvent::Error {
                session_id: None,
                message: format!("API config not found: {name}"),
            });
        }
    }

    /// Test connectivity of an API config by sending a minimal probe request.
    ///
    /// `name` empty tests the active (top-level) config; otherwise the named
    /// profile's values are used. Emits `BackendEvent::ApiTestResult`; the
    /// active provider is never modified.
    async fn test_api_config(&self, name: &str) {
        let timestamp_start = std::time::Instant::now();
        let config = self.config.read().await.clone();

        // Resolve the config to test: named profile or the active default.
        let probe = match resolve_probe_config(&config, name) {
            Ok(probe) => probe,
            Err(message) => {
                self.emit(BackendEvent::ApiTestResult {
                    name: name.into(),
                    ok: false,
                    message,
                    latency_ms: timestamp_start.elapsed().as_millis() as u64,
                });
                return;
            }
        };

        // Build a throwaway provider from the probe config.
        let provider = match create_provider(&probe) {
            Ok(provider) => provider,
            Err(error) => {
                self.emit(BackendEvent::ApiTestResult {
                    name: name.into(),
                    ok: false,
                    message: format!("provider build failed: {error}"),
                    latency_ms: timestamp_start.elapsed().as_millis() as u64,
                });
                return;
            }
        };

        let model = probe.model.clone();
        let request = ChatRequest {
            model: model.clone(),
            messages: vec![ChatMessage::user("ping")],
            tools: None,
            temperature: Some(0.0),
            max_tokens: Some(4),
        };
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(20), provider.chat(&request)).await;

        let latency_ms = timestamp_start.elapsed().as_millis() as u64;
        let (ok, message) = match result {
            Ok(Ok(response)) => (
                true,
                match response.content {
                    Some(content) if !content.trim().is_empty() => format!(
                        "OK ({}) — 响应: {}",
                        model,
                        echo_defs::token::truncate(content.trim(), 60)
                    ),
                    _ => format!("OK ({model}) — 收到空响应"),
                },
            ),
            Ok(Err(error)) => (false, format!("请求失败: {error}")),
            Err(_) => (false, "请求超时（>20s）".into()),
        };
        self.emit(BackendEvent::ApiTestResult {
            name: name.into(),
            ok,
            message,
            latency_ms,
        });
    }

    /// 查询 API 账户余额（目前仅 DeepSeek 官方端点支持 `/user/balance`）。
    ///
    /// `name` 空 = 全局默认配置，非空 = 该 profile；用 profile 自身的
    /// api_key / base_url 请求。响应示例：
    /// `{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"110.00",...}]}`
    async fn query_api_balance(&self, name: &str) {
        let config = self.config.read().await.clone();
        let probe = match resolve_probe_config(&config, name) {
            Ok(probe) => probe,
            Err(message) => {
                self.emit_balance_fail(name, message).await;
                return;
            }
        };
        let Some(endpoint) = deepseek_balance_endpoint(&probe.base_url) else {
            self.emit_balance_fail(
                name,
                format!(
                    "余额查询仅支持 DeepSeek 官方端点（当前 base_url: {}）",
                    probe.base_url
                ),
            )
            .await;
            return;
        };
        let api_key = probe.effective_api_key();
        if api_key.is_empty() {
            self.emit_balance_fail(name, "缺少 API Key，无法查询余额".into())
                .await;
            return;
        }

        #[derive(serde::Deserialize)]
        struct BalanceResponse {
            #[serde(default)]
            is_available: bool,
            #[serde(default)]
            balance_infos: Vec<BalanceInfo>,
        }
        #[derive(serde::Deserialize)]
        struct BalanceInfo {
            #[serde(default)]
            currency: String,
            #[serde(default)]
            total_balance: String,
        }

        let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|e| format!("HTTP client build failed: {e}"))?;
            let response = client
                .get(&endpoint)
                .bearer_auth(&api_key)
                .send()
                .await
                .map_err(|e| format!("请求失败: {e}"))?;
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| format!("读取响应失败: {e}"))?;
            if !status.is_success() {
                return Err(format!(
                    "HTTP {status}: {}",
                    echo_defs::token::truncate(&text, 200)
                ));
            }
            let parsed: BalanceResponse = serde_json::from_str(&text).map_err(|e| {
                format!(
                    "响应解析失败: {e} — body: {}",
                    echo_defs::token::truncate(&text, 200)
                )
            })?;
            Ok::<BalanceResponse, String>(parsed)
        })
        .await;

        match result {
            Ok(Ok(parsed)) => {
                let info = parsed.balance_infos.first();
                self.emit(BackendEvent::ApiBalanceResult {
                    name: name.into(),
                    ok: true,
                    available: parsed.is_available,
                    total: info.map(|i| i.total_balance.clone()).unwrap_or_default(),
                    currency: info.map(|i| i.currency.clone()).unwrap_or_default(),
                    message: if parsed.is_available {
                        "余额已更新".into()
                    } else {
                        "账户当前不可用（余额不足或已停用）".into()
                    },
                });
            }
            Ok(Err(message)) => self.emit_balance_fail(name, message).await,
            Err(_) => {
                self.emit_balance_fail(name, "请求超时（>15s）".into())
                    .await
            }
        }
    }

    /// 余额查询失败时发 `ApiBalanceResult{ok:false}`。
    async fn emit_balance_fail(&self, name: &str, message: String) {
        self.emit(BackendEvent::ApiBalanceResult {
            name: name.into(),
            ok: false,
            available: false,
            total: String::new(),
            currency: String::new(),
            message,
        });
    }

    /// Persist the system prompt plugin text to `[plugins.system_prompt]`
    /// instead of `[agent]`. The API config no longer owns the system prompt.
    async fn persist_system_prompt_plugin(&self, text: &str) {
        let store = match self.config_store.lock().await.clone() {
            Some(s) => s,
            None => return,
        };
        let text = text.to_string();
        if let Err(error) = store.patch(|root| {
            let plugins = echo_adapter::ensure_table(root, "plugins");
            let system_prompt = echo_adapter::ensure_table(plugins, "system_prompt");
            system_prompt.insert("text".into(), toml::Value::String(text));
            Ok(())
        }) {
            tracing::warn!(error = %error, "failed to persist system prompt plugin");
        }
    }

    /// Persist a config snapshot's `[agent]` section back to the TOML file
    /// through the shared [`echo_adapter::ConfigStore`].
    async fn persist_config(&self, cfg: &AgentConfig) {
        let store = match self.config_store.lock().await.clone() {
            Some(s) => s,
            None => return,
        };
        let mut value = match toml::Value::try_from(cfg) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize agent config");
                return;
            }
        };
        tracing::debug!(api_key_len = cfg.api_key.len(), "persisting agent config");
        if let Err(e) = store.patch(|root| {
            // `api_key` is excluded from AgentConfig's generic serialization.
            // Write the explicit in-memory key when set; otherwise keep the
            // on-disk value so unrelated persists never strip it from the file.
            let key_value = if cfg.api_key.is_empty() {
                root.get("agent")
                    .and_then(|agent| agent.get("api_key"))
                    .cloned()
            } else {
                Some(toml::Value::String(cfg.api_key.clone()))
            };
            if let Some(key_value) = key_value {
                value
                    .as_table_mut()
                    .expect("serialized agent config is a table")
                    .insert("api_key".into(), key_value);
            }
            // `teams` / `disabled_teams` 由 AgentManager 独立持久化
            // （config_writer 只写这两张表）。人格快照里的副本可能已过期，
            // 这里一律以磁盘为准，避免"某次无关保存把新人格名单写回旧值"。
            let table = value
                .as_table_mut()
                .expect("serialized agent config is a table");
            for key in ["teams", "disabled_teams"] {
                let disk = root.get("agent").and_then(|agent| agent.get(key)).cloned();
                match disk {
                    Some(v) => {
                        table.insert(key.into(), v);
                    }
                    None => {
                        table.remove(key);
                    }
                }
            }
            root.insert("agent".into(), value.clone());
            Ok(())
        }) {
            tracing::warn!(error = %e, "failed to persist agent config");
            return;
        }
        tracing::info!(path = %store.path().display(), "agent config persisted");
    }
}

/// Build the config snapshot used for a connectivity probe.
///
/// `name` empty → active (top-level) config with the active profile merged
/// in; otherwise the named profile's non-empty values override the current
/// effective config. Returns an error string when the profile does not exist
/// or the resolved config has no provider.
/// 从 base_url 推导 DeepSeek 余额查询端点。
///
/// `https://api.deepseek.com/anthropic` / `.../v1` / `.../beta` / 裸域
/// 统一映射为 `{root}/user/balance`；非 DeepSeek 域返回 None。
fn deepseek_balance_endpoint(base_url: &str) -> Option<String> {
    let mut root = base_url.trim().trim_end_matches('/');
    for suffix in ["/anthropic", "/v1", "/beta"] {
        if let Some(stripped) = root.strip_suffix(suffix) {
            root = stripped;
            break;
        }
    }
    if root.contains("deepseek.com") {
        Some(format!("{root}/user/balance"))
    } else {
        None
    }
}

fn resolve_probe_config(
    config: &crate::config::AgentConfig,
    name: &str,
) -> Result<crate::config::AgentConfig, String> {
    let mut probe = config.clone();
    if name.is_empty() {
        probe.apply_active_profile();
    } else {
        let profile = config
            .api_profiles
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("profile not found: {name}"))?;
        // 与 apply_active_profile 一致的宽松语义：profile 的空字段
        // 回退到当前生效值，避免字段残缺的 profile 测不出本来的配置。
        if !profile.provider.is_empty() {
            probe.provider = profile.provider.clone();
        }
        if !profile.model.is_empty() {
            probe.model = profile.model.clone();
        }
        if !profile.base_url.is_empty() {
            probe.base_url = profile.base_url.clone();
        }
        if !profile.api_key.is_empty() {
            probe.api_key = profile.api_key.clone();
        }
        probe.thinking = profile.thinking;
        probe.reasoning_effort = profile.reasoning_effort;
    }
    probe.api_key = probe.effective_api_key();
    probe.base_url = probe.effective_base_url();
    if probe.provider.is_empty() {
        return Err(format!(
            "provider not set (name={name}); 请先在设置中配置 Provider/API 或激活某个 profile"
        ));
    }
    Ok(probe)
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
/// Whether a plugin with `id` is already registered in the host.
/// persona 名单对某工具是否允许（白名单非空 = 仅列出；黑名单命中即拒绝）。
fn persona_tool_allowed(profile: &crate::config::AgentProfile, name: &str) -> bool {
    (profile.enabled_tools.is_empty() || profile.enabled_tools.iter().any(|t| t == name))
        && !profile.disabled_tools.iter().any(|t| t == name)
}

/// persona 名单对某技能是否允许（语义同 [`persona_tool_allowed`]）。
fn persona_skill_allowed(profile: &crate::config::AgentProfile, name: &str) -> bool {
    (profile.enabled_skills.is_empty() || profile.enabled_skills.iter().any(|s| s == name))
        && !profile.disabled_skills.iter().any(|s| s == name)
}

fn desc_exists(host: &crate::plugins::PluginHost, id: &str) -> bool {
    host.descriptors().iter().any(|d| d.id == id)
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
pub mod tests {
    use super::tool_exec::invalid_tool_arguments;
    use super::*;
    use crate::llm::{ChatChunk, ChatResponse, LlmError, Usage};
    use crate::session::SessionKey;
    use crate::tool::{Tool, ToolError};
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub struct MockProvider {
        pub calls: Arc<AtomicUsize>,
        pub reply: String,
    }

    struct ConcurrentProvider {
        entered: Arc<AtomicUsize>,
        release: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }
        fn default_model(&self) -> &str {
            "mock-model"
        }
        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.messages[0].role, crate::llm::ChatRole::System);
            Ok(ChatResponse {
                stop_reason: None,
                content: Some(self.reply.clone()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            })
        }
        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<(), LlmError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ConcurrentProvider {
        fn name(&self) -> &str {
            "concurrent"
        }

        fn default_model(&self) -> &str {
            "concurrent"
        }

        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.release
                .acquire()
                .await
                .expect("release semaphore should stay open")
                .forget();
            let sequence = request
                .messages
                .iter()
                .filter_map(|message| structured_message_sequence(&message.content))
                .next_back()
                .unwrap_or_default();
            Ok(ChatResponse {
                stop_reason: None,
                content: Some(format!("reply-{sequence}")),
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: Usage::default(),
            })
        }

        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<(), LlmError> {
            Ok(())
        }
    }

    /// 记录每次请求看到的历史，并在测试放行前一直挂起（用于单会话排队断言）。
    #[derive(Clone)]
    struct SnapshotProvider {
        calls: Arc<AtomicUsize>,
        seen: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        gate: Arc<tokio::sync::Semaphore>,
    }

    impl SnapshotProvider {
        /// 等第一次模型调用进入（此后队列闸门被第一个 turn 持有）。
        async fn wait_entered(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while self.calls.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("first turn should reach the provider");
        }

        /// 放行所有挂起的模型调用。
        fn release(&self) {
            self.gate.add_permits(8);
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for SnapshotProvider {
        fn name(&self) -> &str {
            "snapshot"
        }

        fn default_model(&self) -> &str {
            "snapshot"
        }

        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            let seen: Vec<String> = request
                .messages
                .iter()
                .map(|message| message.content.clone())
                .collect();
            let sequence = seen
                .iter()
                .filter_map(|content| structured_message_sequence(content))
                .next_back()
                .unwrap_or_default();
            self.seen.lock().expect("seen poisoned").push(seen);
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.gate
                .acquire()
                .await
                .expect("gate semaphore should stay open")
                .forget();
            Ok(ChatResponse {
                stop_reason: None,
                content: Some(format!("reply-{sequence}")),
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: Usage::default(),
            })
        }

        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<(), LlmError> {
            Ok(())
        }
    }

    fn test_agent(provider: Arc<dyn LlmProvider>) -> Agent {
        Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            ToolRegistry::new(),
            Arc::new(AdapterRegistry::new()),
        )
    }

    // ── 进程级事件汇聚点（去主智能体 P0）──

    /// 多人格共享同一汇聚点：任一人格 emit 的事件都到达同一处，
    /// 「谁挂到 Panel」不再取决于默认人格。
    #[tokio::test]
    async fn shared_event_sink_receives_events_from_every_persona() {
        let received = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink: EventSink = {
            let received = received.clone();
            Arc::new(move |event: BackendEvent| {
                let id = match &event {
                    BackendEvent::AgentOutput { session_id, .. } => session_id.clone(),
                    _ => "other".into(),
                };
                received.lock().unwrap().push(id);
            })
        };
        let a = test_agent(Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "a".into(),
        }));
        let b = test_agent(Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "b".into(),
        }));
        a.attach_event_sink(sink.clone());
        b.attach_event_sink(sink);

        a.emit(BackendEvent::AgentOutput {
            session_id: "from-a".into(),
            team_id: None,
            content: "x".into(),
            branch_id: None,
        });
        b.emit(BackendEvent::AgentOutput {
            session_id: "from-b".into(),
            team_id: None,
            content: "y".into(),
            branch_id: None,
        });

        let got = received.lock().unwrap().clone();
        assert!(got.contains(&"from-a".to_string()), "got {got:?}");
        assert!(got.contains(&"from-b".to_string()), "got {got:?}");
    }

    /// 汇聚点优先于 handle：同时配置两者时只投递一次（不双投）。
    #[tokio::test]
    async fn event_sink_takes_precedence_over_handle_without_double_delivery() {
        let (bridge, handle) = crate::create_bridge();
        let agent = test_agent(Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        }));
        agent.attach(Arc::new(handle));
        let sink_calls = Arc::new(AtomicUsize::new(0));
        let sink: EventSink = {
            let sink_calls = sink_calls.clone();
            Arc::new(move |_event: BackendEvent| {
                sink_calls.fetch_add(1, Ordering::SeqCst);
            })
        };
        agent.attach_event_sink(sink);

        agent.emit(BackendEvent::AgentOutput {
            session_id: "s".into(),
            team_id: None,
            content: "x".into(),
            branch_id: None,
        });

        assert_eq!(sink_calls.load(Ordering::SeqCst), 1);
        let mut rx = bridge.event_rx.lock().await;
        assert!(
            rx.try_recv().is_err(),
            "sink must suppress the legacy handle path (no double delivery)"
        );
    }

    #[tokio::test]
    async fn emitted_events_are_recorded_into_the_display_timeline() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "Hello!".into(),
        });
        let agent = test_agent(provider);
        let key = SessionKey::local_tui();
        let session = agent.trunk.get_or_create(&key, "user".into(), None);
        // Adapter flow: inbound message event, then branch execution, then output.
        agent.emit(BackendEvent::MessageReceived {
            session_id: session.id.clone(),
            adapter_name: "local".into(),
            platform: "local".into(),
            user_id: "local_user".into(),
            user_name: "local user".into(),
            channel: "direct".into(),
            group_name: None,
            content: "hi".into(),
            images: vec![],
            timestamp: 1700000000,
            received_at_ms: 1700000000123,
            message_sequence: 1,
            team_id: None,
        });
        let reply = agent.process_message(&session, "hi").await.unwrap();
        agent.emit(BackendEvent::AgentOutput {
            session_id: session.id.clone(),
            content: reply.clone(),
            branch_id: None,
            team_id: None,
        });
        assert_eq!(reply, "Hello!");

        let timeline = agent.trunk.timeline_snapshot();
        // user entry + backend reply entry.
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[0].kind, "user");
        assert_eq!(timeline[0].content, "hi");
        assert!(timeline[0].source.is_some(), "source provenance recorded");
        assert_eq!(timeline[1].kind, "backend");
        assert_eq!(timeline[1].content, "Hello!");
    }

    #[tokio::test]
    async fn timeline_records_tools_and_attaches_outcomes() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MockTool {
            name: "mock_tool",
            result: "found".into(),
        }));
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let call = ToolCall {
            id: "c1".into(),
            name: "mock_tool".into(),
            arguments: r#"{"query":"x","api_key":"secret-1"}"#.into(),
        };
        let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
        assert_eq!(result.text, "found");

        let timeline = agent.trunk.timeline_snapshot();
        assert_eq!(timeline.len(), 1);
        let tool = timeline[0].tool.as_ref().expect("tool entry");
        assert_eq!(tool.name, "mock_tool");
        assert!(tool.output.is_some(), "outcome attached");
        assert!(!tool.input.contains("secret-1"), "secrets redacted");
        assert!(tool.input.contains("[已隐藏]"), "redaction marker shown");
        assert!(!tool.failed);
    }

    #[tokio::test]
    async fn timeline_associates_reasoning_with_the_final_output() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "done".into(),
        });
        let agent = test_agent(provider);
        let session_id = "local:tui::one";
        let branch_id = "branch-abc";
        agent.emit(BackendEvent::AgentReasoning {
            session_id: session_id.into(),
            team_id: None,
            branch_id: branch_id.into(),
            content: "先查资料".into(),
        });
        agent.emit(BackendEvent::ReplyBranchCompleted {
            session_id: session_id.into(),
            branch_id: branch_id.into(),
            message_sequence: 1,
            success: true,
            cancelled: false,
            completed_at_ms: 1000,
        });
        agent.emit(BackendEvent::AgentOutput {
            session_id: session_id.into(),
            content: "done".into(),
            branch_id: Some(branch_id.into()),
            team_id: None,
        });
        let timeline = agent.trunk.timeline_snapshot();
        // 新语义：推理按到达顺序落为独立 reasoning 条目，backend 输出不再附加。
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[0].kind, "reasoning");
        assert_eq!(timeline[0].content, "先查资料");
        assert_eq!(timeline[1].kind, "backend");
        assert_eq!(timeline[1].content, "done");
        assert!(
            timeline[1].reasoning.is_none(),
            "reasoning is a standalone entry"
        );
    }

    #[tokio::test]
    async fn agent_replies_and_remembers() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "Hello!".into(),
        });
        let agent = test_agent(provider.clone());
        let key = SessionKey::local_tui();
        let session = agent.trunk.get_or_create(&key, "user".into(), None);
        let reply = agent.process_message(&session, "hi").await.unwrap();
        assert_eq!(reply, "Hello!");
        assert_eq!(session.history.lock().await.len(), 2);
        let _ = agent.process_message(&session, "again").await.unwrap();
        assert_eq!(session.history.lock().await.len(), 4);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }

    /// 并行多会话模式：同一会话的多个 turn 并发进入模型，按请求序号合并。
    #[tokio::test]
    async fn parallel_mode_runs_turns_concurrently_and_merges_by_request_sequence() {
        let entered = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let provider = Arc::new(ConcurrentProvider {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let agent = Arc::new(test_agent(provider));
        agent
            .set_loop_mode_for_test(echo_defs::LoopMode::Parallel)
            .await;
        let session = agent
            .trunk
            .get_or_create(&SessionKey::local_tui(), "user".into(), None);
        let first = r#"<backend_message_hook>{"message_sequence":1,"content":"first"}</backend_message_hook>"#;
        let second = r#"<backend_message_hook>{"message_sequence":2,"content":"second"}</backend_message_hook>"#;

        let first_task = {
            let agent = Arc::clone(&agent);
            let session = session.clone();
            tokio::spawn(async move { agent.process_message(&session, first).await })
        };
        let second_task = {
            let agent = Arc::clone(&agent);
            let session = session.clone();
            tokio::spawn(async move { agent.process_message(&session, second).await })
        };

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while entered.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both branches should enter the provider concurrently");
        release.add_permits(2);
        first_task.await.unwrap().unwrap();
        second_task.await.unwrap().unwrap();

        let history = session.history.lock().await;
        assert_eq!(history.len(), 4);
        assert_eq!(structured_message_sequence(&history[0].content), Some(1));
        assert_eq!(history[1].content, "reply-1");
        assert_eq!(structured_message_sequence(&history[2].content), Some(2));
        assert_eq!(history[3].content, "reply-2");
    }

    /// 单会话模式隐藏并行分支工具；并行模式恢复（工具 schema 与提示词同源）。
    #[tokio::test]
    async fn spawn_parallel_task_is_hidden_in_single_mode() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Arc::new(test_agent(provider));
        assert!(!agent.allows_dynamic_tool("spawn_parallel_task"));
        agent
            .set_loop_mode_for_test(echo_defs::LoopMode::Parallel)
            .await;
        assert!(agent.allows_dynamic_tool("spawn_parallel_task"));
    }

    /// 单会话模式（默认）：同一会话的 turn 串行排队——第二个 turn 拿到的是
    /// 第一轮结束后的上下文（能看到 reply-1），且不会并发进入模型。
    #[tokio::test]
    async fn single_mode_serialises_turns_and_second_turn_sees_the_first_reply() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(SnapshotProvider {
            calls: Arc::clone(&calls),
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            gate: Arc::new(tokio::sync::Semaphore::new(0)),
        });
        let agent = Arc::new(test_agent(provider.clone()));
        // 默认即单会话：不做任何配置。
        assert_eq!(agent.loop_mode(), echo_defs::LoopMode::Single);
        let session = agent
            .trunk
            .get_or_create(&SessionKey::local_tui(), "user".into(), None);
        let first = r#"<backend_message_hook>{"message_sequence":1,"content":"first"}</backend_message_hook>"#;
        let second = r#"<backend_message_hook>{"message_sequence":2,"content":"second"}</backend_message_hook>"#;

        let first_task = {
            let agent = Arc::clone(&agent);
            let session = session.clone();
            tokio::spawn(async move { agent.process_message(&session, first).await })
        };
        // 等第一轮真的进入模型（持有队列闸门），再投第二个输入。
        provider.wait_entered().await;
        let second_task = {
            let agent = Arc::clone(&agent);
            let session = session.clone();
            tokio::spawn(async move { agent.process_message(&session, second).await })
        };
        // 第二个 turn 必须排队：闸门仍被第一轮持有，模型调用数不增加。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "second turn must wait");
        assert_eq!(
            agent.active_inbound_turn_count(),
            2,
            "queued turn is cancellable"
        );

        provider.release();
        first_task.await.unwrap().unwrap();
        second_task.await.unwrap().unwrap();

        let seen = provider.seen.lock().expect("seen poisoned").clone();
        assert_eq!(seen.len(), 2, "both turns ran");
        assert!(
            seen[1].iter().any(|content| content == "reply-1"),
            "the queued turn must see the first reply: {:?}",
            seen[1]
        );
        let history = session.history.lock().await;
        assert_eq!(history.len(), 4);
        assert_eq!(structured_message_sequence(&history[0].content), Some(1));
        assert_eq!(history[1].content, "reply-1");
        assert_eq!(structured_message_sequence(&history[2].content), Some(2));
        assert_eq!(history[3].content, "reply-2");
    }

    #[tokio::test]
    async fn request_trunk_timeline_returns_the_persisted_history() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Arc::new(test_agent(provider));
        // 去主智能体后会话命令必须带 team_id：本 agent 视为 "t"。
        agent.set_team_id(Some("t".into()));
        let (bridge, handle) = crate::create_bridge();
        agent.attach(Arc::new(handle));
        agent.emit(BackendEvent::MessageReceived {
            session_id: "local:tui::local_user".into(),
            adapter_name: "local".into(),
            platform: "local".into(),
            user_id: "local_user".into(),
            user_name: "local user".into(),
            channel: "direct".into(),
            group_name: None,
            content: "你好".into(),
            images: vec![],
            timestamp: 1700000000,
            received_at_ms: 1700000000123,
            message_sequence: 1,
            team_id: None,
        });

        agent
            .apply_command(BackendCommand::RequestTrunkTimeline {
                team_id: Some("t".into()),
                since_seq: 0,
            })
            .await;
        let mut events = Vec::new();
        while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
            events.push(event);
        }
        let timeline = events
            .into_iter()
            .find_map(|event| match event {
                BackendEvent::TrunkTimeline { messages, .. } => Some(messages),
                _ => None,
            })
            .expect("TrunkTimeline event emitted");
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].kind, "user");
        assert_eq!(timeline[0].content, "你好");
    }

    /// 去主智能体：会话类命令缺少 team_id 时必须明确报错（无默认人格兜底）。
    #[tokio::test]
    async fn session_commands_require_explicit_team_id() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Arc::new(test_agent(provider));
        let (bridge, handle) = crate::create_bridge();
        agent.attach(Arc::new(handle));

        for cmd in [
            BackendCommand::RequestTrunkTimeline {
                team_id: None,
                since_seq: 0,
            },
            BackendCommand::RequestContext {
                team_id: None,
                session_id: None,
            },
            BackendCommand::ClearHistory { team_id: None },
        ] {
            agent.apply_command(cmd).await;
        }

        let mut errors = Vec::new();
        while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
            if let BackendEvent::Error { message, .. } = event {
                errors.push(message);
            }
        }
        assert_eq!(errors.len(), 3, "each command must error: {errors:?}");
        assert!(
            errors.iter().all(|m| m.contains("team_id")),
            "errors must explain the missing team_id: {errors:?}"
        );
    }

    #[tokio::test]
    async fn update_api_config_persists() {
        let tmp =
            std::env::temp_dir().join(format!("echo-agent-cfg-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(
            &tmp,
            "[server]\nx = 1\n\n[agent]\nprovider = \"openai\"\nmodel = \"old-model\"\n",
        )
        .unwrap();
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = test_agent(provider);
        agent.set_config_path(tmp.clone());

        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: String::new(),
                provider: "anthropic".into(),
                model: "claude-x".into(),
                base_url: "http://example.test".into(),
                api_key: "sk-test-123".into(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;

        let cfg = agent.api_config().await;
        assert_eq!(cfg.provider, "anthropic");
        assert_eq!(cfg.model, "claude-x");
        assert_eq!(cfg.base_url, "http://example.test");
        assert_eq!(cfg.effective_api_key(), "sk-test-123");
        assert!(cfg.active_api.is_empty());

        let content = std::fs::read_to_string(&tmp).unwrap();
        assert!(content.contains("[server]\nx = 1"));
        assert!(content.contains("provider = \"anthropic\""));
        assert!(content.contains("model = \"claude-x\""));
        assert!(content.contains("base_url = \"http://example.test\""));
        assert!(content.contains("api_key = \"sk-test-123\""));
        assert!(!content.contains("old-model"));
        std::fs::remove_file(&tmp).ok();
    }

    #[tokio::test]
    async fn persist_config_keeps_on_disk_api_key() {
        let tmp =
            std::env::temp_dir().join(format!("echo-agent-cfg-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(
            &tmp,
            "[agent]\nprovider = \"openai\"\napi_key = \"sk-on-disk\"\n",
        )
        .unwrap();
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = test_agent(provider);
        agent.set_config_path(tmp.clone());

        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: String::new(),
                provider: "anthropic".into(),
                model: "claude-x".into(),
                base_url: "http://example.test".into(),
                api_key: String::new(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;

        let content = std::fs::read_to_string(&tmp).unwrap();
        assert!(content.contains("provider = \"anthropic\""));
        assert!(
            content.contains("api_key = \"sk-on-disk\""),
            "unexpected: {content}"
        );
        std::fs::remove_file(&tmp).ok();
    }

    /// 人格名单由 AgentManager 独立落盘；人格快照里的副本可能过期，
    /// `persist_config` 必须以磁盘为准（否则无关保存会写回旧名单）。
    #[tokio::test]
    async fn persist_config_preserves_on_disk_teams() {
        let tmp =
            std::env::temp_dir().join(format!("echo-agent-cfg-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(
            &tmp,
            "[agent]\nprovider = \"openai\"\n\n[agent.teams.fresh]\nname = \"Fresh\"\n",
        )
        .unwrap();
        let agent = test_agent(Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        }));
        // 模拟"过期快照"：agent 内存里的 teams 是空的，磁盘上已有 fresh。
        agent.set_config_path(tmp.clone());

        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: String::new(),
                provider: "anthropic".into(),
                model: "claude-x".into(),
                base_url: "http://example.test".into(),
                api_key: String::new(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;

        let content = std::fs::read_to_string(&tmp).unwrap();
        assert!(content.contains("provider = \"anthropic\""));
        assert!(
            content.contains("[agent.teams.fresh]"),
            "on-disk teams must survive an unrelated persist: {content}"
        );
        std::fs::remove_file(&tmp).ok();
    }

    #[tokio::test]
    async fn update_api_config_new_profile_inherits_empty_fields() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = test_agent(provider);

        // 先建立顶层默认配置。
        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: String::new(),
                provider: "deepseek".into(),
                model: "deepseek-x".into(),
                base_url: "http://ds.test".into(),
                api_key: "sk-top".into(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;

        // 用全空字段创建命名 profile：应从当前生效配置继承，而不是写出
        // 残缺 profile（真实事故：TUI 只改 model 保存后 profile 缺字段）。
        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: "backup".into(),
                provider: String::new(),
                model: String::new(),
                base_url: String::new(),
                api_key: String::new(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;

        let cfg = agent.api_config().await;
        assert_eq!(cfg.active_api, "backup");
        let profile = cfg
            .api_profiles
            .iter()
            .find(|p| p.name == "backup")
            .expect("profile created");
        assert_eq!(profile.provider, "deepseek", "provider inherited");
        assert_eq!(profile.model, "deepseek-x", "model inherited");
        assert_eq!(profile.base_url, "http://ds.test", "base_url inherited");

        // 更新已有 profile 时保持"空 = 保留原值"语义。
        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: "backup".into(),
                provider: String::new(),
                model: "deepseek-y".into(),
                base_url: String::new(),
                api_key: String::new(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;
        let cfg = agent.api_config().await;
        let profile = cfg
            .api_profiles
            .iter()
            .find(|p| p.name == "backup")
            .expect("profile exists");
        assert_eq!(profile.provider, "deepseek", "provider kept, not inherited");
        assert_eq!(profile.model, "deepseek-y");
    }

    #[tokio::test]
    async fn test_api_config_profile_empty_fields_fall_back() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Arc::new(test_agent(provider));
        let (bridge, handle) = crate::create_bridge();
        agent.attach(Arc::new(handle));

        // 顶层配置指向一个必然连不上的地址（连接即刻被拒绝，不打真实网络）。
        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: String::new(),
                provider: "deepseek".into(),
                model: "deepseek-x".into(),
                base_url: "http://127.0.0.1:1".into(),
                api_key: "sk-top".into(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;

        // 手工注入一个字段残缺的 profile（provider/base_url 为空）。
        agent.config.write().await.api_profiles.push(ApiProfile {
            name: "broken".into(),
            provider: String::new(),
            model: "deepseek-x".into(),
            base_url: String::new(),
            api_key: String::new(),
            thinking: crate::config::ThinkingMode::Enabled,
            reasoning_effort: crate::config::ReasoningEffort::Max,
        });

        agent
            .apply_command(BackendCommand::TestApi {
                name: "broken".into(),
            })
            .await;

        let mut result = None;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
                if let BackendEvent::ApiTestResult { name, message, .. } = event {
                    result = Some((name, message));
                    break;
                }
            }
            if result.is_some() {
                break;
            }
        }
        let (name, message) = result.expect("ApiTestResult event emitted");
        assert_eq!(name, "broken");
        assert!(
            !message.contains("provider build failed"),
            "空字段应回退到顶层配置而不是构建失败: {message}"
        );
        // 回退后 provider 构建成功，失败只可能发生在请求阶段（连接拒绝）。
        assert!(
            message.contains("请求失败") || message.contains("请求超时"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn invalid_tool_arguments_flags_missing_required() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
        });
        // 空对象：缺 command
        let message = invalid_tool_arguments("bash", "{}", &serde_json::json!({}), &schema)
            .expect("missing required flagged");
        assert!(message.contains("缺少必需参数 command"), "{message}");
        assert!(message.contains("你发送的参数: {}"), "{message}");
        assert!(message.contains("schema"), "{message}");
        // null 值同样算缺失
        assert!(invalid_tool_arguments(
            "bash",
            "{}",
            &serde_json::json!({"command": null}),
            &schema
        )
        .is_some());
        // 参数不是对象：所有必需字段都缺失
        assert!(invalid_tool_arguments("bash", "[]", &serde_json::Value::Null, &schema).is_some());
        // 字段齐全（含空字符串——合法值，不算缺失）：通过
        assert!(
            invalid_tool_arguments("bash", "{}", &serde_json::json!({"command": ""}), &schema,)
                .is_none()
        );
        // schema 无 required：不预检
        let free = serde_json::json!({"type": "object", "properties": {}});
        assert!(invalid_tool_arguments("stub", "{}", &serde_json::json!({}), &free).is_none());
    }

    #[tokio::test]
    async fn run_tool_empty_arguments_gets_corrective_error() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let mut tools = ToolRegistry::new();
        crate::tool::builtin::coding::register_coding_tools(&mut tools, std::env::temp_dir());
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let call = ToolCall {
            id: "bad-1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
        assert!(
            result.text.contains("缺少必需参数 command"),
            "corrective message: {}",
            result.text
        );
        assert!(
            result.text.contains("你发送的参数: {}"),
            "echoes received args: {}",
            result.text
        );
        assert!(
            result.text.contains("schema"),
            "includes tool schema: {}",
            result.text
        );
    }

    #[tokio::test]
    async fn run_tool_malformed_json_gets_clear_error() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let mut tools = ToolRegistry::new();
        crate::tool::builtin::coding::register_coding_tools(&mut tools, std::env::temp_dir());
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let call = ToolCall {
            id: "bad-2".into(),
            name: "bash".into(),
            arguments: "{bad json".into(),
        };
        let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
        assert!(
            result.text.contains("工具参数不是合法 JSON"),
            "clear JSON error: {}",
            result.text
        );
        assert!(
            result.text.contains("{bad json"),
            "echoes raw arguments: {}",
            result.text
        );
    }

    #[tokio::test]
    async fn run_tool_valid_arguments_pass_preflight() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let mut tools = ToolRegistry::new();
        crate::tool::builtin::coding::register_coding_tools(&mut tools, std::env::temp_dir());
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let call = ToolCall {
            id: "ok-1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"echo preflight-ok"}"#.into(),
        };
        let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
        assert!(
            result.text.contains("preflight-ok"),
            "valid call executes: {}",
            result.text
        );
        assert!(
            !result.text.contains("工具参数无效"),
            "no preflight error: {}",
            result.text
        );
    }

    #[tokio::test]
    async fn skill_keyword_loads_instructions() {
        let mut reg = SkillRegistry::new();
        reg.register(crate::skill::Skill {
            metadata: crate::skill::SkillMetadata {
                name: "calc".into(),
                description: "d".into(),
                keywords: vec!["calc".into()],
                always: false,
                enabled: true,
                category: String::new(),
                package: None,
                system: false,
            },
            instructions: "use calculator tool".into(),
        });
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            reg,
            ToolRegistry::new(),
            Arc::new(AdapterRegistry::new()),
        );
        let prompt = agent.build_system_prompt("help me calc 1+1").await;
        assert!(prompt.contains("Available skills"));
        assert!(prompt.contains("Triggered skill: calc"));
        assert!(prompt.contains("calculator"));
    }

    #[tokio::test]
    async fn skill_directory_hot_reloads_updates_additions_and_deletions() {
        let dir = std::env::temp_dir().join(format!("echo-skill-reload-{}", uuid::Uuid::new_v4()));
        let style_dir = dir.join("style");
        std::fs::create_dir_all(&style_dir).unwrap();
        let style_path = style_dir.join("SKILL.md");
        std::fs::write(
            &style_path,
            "---\nname: style\ndescription: style\nmetadata:\n  always: true\n---\nold instructions",
        )
        .unwrap();

        let config = AgentConfig {
            skills_dir: dir.display().to_string(),
            ..Default::default()
        };
        let initial = SkillRegistry::discover(&config.skills_dir).unwrap();
        let skills_dir = config.skills_dir.clone();
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Arc::new(Agent::new(
            provider,
            config,
            initial,
            ToolRegistry::new(),
            Arc::new(AdapterRegistry::new()),
        ));

        let first_prompt = agent.build_system_prompt("plain input").await;
        assert!(first_prompt.contains("old instructions"));

        // 手动触发重载（替代旧的定时扫描）：修改 + 新增技能后显式 reload。
        std::fs::write(
            &style_path,
            "---\nname: style\ndescription: updated\nmetadata:\n  always: true\n---\nnew instructions",
        )
        .unwrap();
        let extra_dir = dir.join("extra");
        std::fs::create_dir_all(&extra_dir).unwrap();
        std::fs::write(
            extra_dir.join("SKILL.md"),
            "---\nname: extra\ndescription: extra\nkeywords: [extra]\n---\nextra instructions",
        )
        .unwrap();
        assert!(
            agent.reload_skills(&skills_dir).await.unwrap(),
            "modified + added skills should report a reload"
        );

        let skills = agent.skills.lock().await;
        let updated = skills
            .get("style")
            .is_some_and(|skill| skill.instructions == "new instructions");
        let added = skills.get("extra").is_some();
        drop(skills);
        assert!(updated, "modified skill content should reload");
        assert!(added, "added skill should be discovered");

        let second_prompt = agent.build_system_prompt("plain input").await;
        assert!(second_prompt.contains("new instructions"));
        assert!(!second_prompt.contains("old instructions"));

        // 删除技能后再次手动重载。
        std::fs::remove_file(&style_path).unwrap();
        assert!(
            agent.reload_skills(&skills_dir).await.unwrap(),
            "deleted skill should report a reload"
        );
        assert!(agent.skills.lock().await.get("style").is_none());

        agent.shutdown().await;
        std::fs::remove_dir_all(&dir).ok();
    }
    // ── Tool calling loop ──────────────────────────────────────────────────

    /// LLM provider that replays a fixed script of responses.
    struct ScriptedProvider {
        script: tokio::sync::Mutex<std::collections::VecDeque<ChatResponse>>,
        calls: Arc<AtomicUsize>,
        requests: tokio::sync::Mutex<Vec<ChatRequest>>,
    }

    impl ScriptedProvider {
        fn new(script: Vec<ChatResponse>) -> Self {
            Self {
                script: tokio::sync::Mutex::new(script.into()),
                calls: Arc::new(AtomicUsize::new(0)),
                requests: tokio::sync::Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }
        fn default_model(&self) -> &str {
            "mock-model"
        }
        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.messages[0].role, crate::llm::ChatRole::System);
            self.requests.lock().await.push(request.clone());
            self.script
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| LlmError::Config("script exhausted".into()))
        }
        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<(), LlmError> {
            Ok(())
        }
    }

    /// A tool that returns a canned result.
    struct MockTool {
        name: &'static str,
        result: String,
    }

    #[async_trait::async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "mock tool"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _arguments: serde_json::Value) -> Result<String, ToolError> {
            Ok(self.result.clone())
        }
    }

    fn tool_call(id: &str, name: &str) -> ChatResponse {
        ChatResponse {
            stop_reason: None,
            content: Some(format!("calling {name}")),
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: "{}".into(),
            }],
            usage: Usage::default(),
        }
    }

    #[tokio::test]
    async fn tool_calling_loop_executes_and_finishes() {
        let script = vec![
            tool_call("call_1", "mock_tool"),
            ChatResponse {
                stop_reason: None,
                content: Some("final answer".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            },
        ];
        let provider = Arc::new(ScriptedProvider::new(script));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MockTool {
            name: "mock_tool",
            result: "tool output".into(),
        }));

        let agent = Agent::new(
            provider.clone(),
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let key = SessionKey::local_tui();
        let session = agent.trunk.get_or_create(&key, "user".into(), None);
        let reply = agent
            .process_message(&session, "use the tool")
            .await
            .unwrap();
        assert_eq!(reply, "final answer");
        assert_eq!(provider.call_count(), 2);
    }

    #[tokio::test]
    async fn tool_loop_replays_reasoning_content_on_the_next_request() {
        let mut first = tool_call("call_1", "mock_tool");
        first.reasoning_content = Some("先调用工具，再根据结果回答".into());
        let provider = Arc::new(ScriptedProvider::new(vec![
            first,
            ChatResponse {
                stop_reason: None,
                content: Some("done".into()),
                reasoning_content: Some("工具返回成功".into()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
            },
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MockTool {
            name: "mock_tool",
            result: "ok".into(),
        }));
        let agent = Agent::new(
            provider.clone(),
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let session = agent
            .trunk
            .get_or_create(&SessionKey::local_tui(), "user".into(), None);

        assert_eq!(
            agent.process_message(&session, "run").await.unwrap(),
            "done"
        );
        let requests = provider.requests.lock().await;
        let replayed = requests[1]
            .messages
            .iter()
            .find(|message| message.tool_calls.is_some())
            .expect("assistant tool-call message");
        assert_eq!(
            replayed.reasoning_content.as_deref(),
            Some("先调用工具，再根据结果回答")
        );
    }

    /// 回归：生产装配（组合根）把 spawn_subagent 注册进注册表并接线运行态后，
    /// echo-loop 路径发送的工具名必须唯一——重复会让 API 拒绝整个请求
    ///（"Tool names must be unique"）。
    #[tokio::test]
    async fn echo_loop_tool_names_stay_unique_when_spawn_subagent_is_registered() {
        let provider = Arc::new(ScriptedProvider::new(vec![ChatResponse {
            stop_reason: None,
            content: Some("done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        }]));
        let store = crate::subagent::SubagentStore::new();
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(crate::subagent::SpawnSubagentTool::new(
            store.clone(),
            Arc::new(|_| {}),
        )));
        tools.set_package(
            crate::subagent::SPAWN_SUBAGENT_TOOL,
            crate::plugins::SUBAGENT_PLUGIN_ID,
        );
        let agent = Arc::new(Agent::new(
            provider.clone(),
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        ));
        agent.attach_subagent_runtime(store);
        assert!(
            agent.allows_dynamic_tool(crate::subagent::SPAWN_SUBAGENT_TOOL),
            "插件允许 + 运行态已接线"
        );
        let runner = Arc::new(echo_loop::runner::TurnRunner::new(
            Arc::new(echo_context::EventBus::default()),
            provider.clone(),
            Arc::new(echo_loop::ToolPipeline::new()),
            echo_loop::LoopOptions::default(),
        ));
        agent.set_loop_runner(runner);
        agent.set_use_echo_loop(true);

        let session = agent
            .trunk
            .get_or_create(&SessionKey::local_tui(), "user".into(), None);
        assert_eq!(agent.process_message(&session, "hi").await.unwrap(), "done");

        let requests = provider.requests.lock().await;
        let names: Vec<String> = requests[0]
            .tools
            .as_ref()
            .expect("tools sent")
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert!(
            names.contains(&crate::subagent::SPAWN_SUBAGENT_TOOL.to_string()),
            "schema 应来自注册表: {names:?}"
        );
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "duplicate tool names: {names:?}");
    }

    /// echo-loop 路径的工具超时守卫（2026-09-26 补：此前该路径没有守卫，
    /// 挂死的工具会永久拖住 turn——内置循环的外圈守卫在 echo-loop 上不生效）。
    /// 超时后 turn 继续（notice 喂回模型），事件日志补记中断结果、不留悬空调用。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn echo_loop_path_guards_hung_tools_with_timeout() {
        struct HungTool;
        #[async_trait::async_trait]
        impl crate::tool::Tool for HungTool {
            fn name(&self) -> &str {
                "hung_tool"
            }
            fn description(&self) -> &str {
                "sleeps far beyond the guard (test)"
            }
            async fn execute(
                &self,
                _arguments: serde_json::Value,
            ) -> Result<String, crate::tool::ToolError> {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                Ok("never".into())
            }
        }

        let provider = Arc::new(ScriptedProvider::new(vec![
            ChatResponse {
                stop_reason: Some("tool_calls".into()),
                content: None,
                reasoning_content: None,
                tool_calls: vec![crate::llm::ToolCall {
                    id: "call_hung_1".into(),
                    name: "hung_tool".into(),
                    arguments: "{}".into(),
                }],
                usage: Usage::default(),
            },
            ChatResponse {
                stop_reason: None,
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            },
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(HungTool));
        let agent = Arc::new(Agent::new(
            provider.clone(),
            AgentConfig {
                // 1s 守卫：挂死工具（60s）必须被中止。
                tool_timeout_secs: Some(1),
                ..Default::default()
            },
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        ));
        let runner = Arc::new(echo_loop::runner::TurnRunner::new(
            Arc::new(echo_context::EventBus::default()),
            provider.clone(),
            Arc::new(echo_loop::ToolPipeline::new()),
            echo_loop::LoopOptions::default(),
        ));
        agent.set_loop_runner(runner);
        agent.set_use_echo_loop(true);

        let session = agent
            .trunk
            .get_or_create(&SessionKey::local_tui(), "user".into(), None);
        let started = std::time::Instant::now();
        let reply = agent
            .process_message(&session, "run the slow tool")
            .await
            .unwrap();
        assert_eq!(reply, "done", "turn must continue after a tool timeout");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "guard must abort the 60s tool long before it finishes: {:?}",
            started.elapsed()
        );

        // 模型可见的下一次请求里带超时 notice（turn 未中断）。
        let requests = provider.requests.lock().await;
        assert!(requests.len() >= 2, "expected a second model request");
        let saw_notice = requests[1].messages.iter().any(|message| {
            message.role == crate::llm::ChatRole::Tool
                && message.content.contains("timed out after")
        });
        assert!(saw_notice, "tool message must carry the timeout notice");

        // 事件日志成对：ToolCall 有对应的（中断）ToolResult，不留悬空调用。
        let paired = agent.trunk.event_log().iter().any(|event| {
            matches!(
                event,
                echo_session::SessionEvent::ToolResult(result)
                    if result.tool_call_id == "call_hung_1"
                        && result.result.contains("timed out after")
            )
        });
        assert!(
            paired,
            "interrupted result must be recorded for log pairing"
        );
    }

    /// echo-loop 路径的提示词分层与内置循环一致（2026-09-30 修复回归）：
    /// persona 系统技能与工作区会话注入在 echo-loop 路径同样生效——此前
    /// `build_prompt_blocks` 传 `&[]`/None，两层在普通输入上静默丢失。
    #[tokio::test]
    async fn echo_loop_path_injects_persona_skills_and_workspace() {
        let mut skills = SkillRegistry::new();
        skills.register(crate::skill::Skill::direct(
            "persona-style",
            "人格语气规则",
            vec![],
            false,
            "",
            "PERSONA_SKILL_MARKER",
        ));
        let provider = Arc::new(ScriptedProvider::new(vec![ChatResponse {
            stop_reason: None,
            content: Some("done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        }]));
        let agent = Arc::new(Agent::new(
            provider.clone(),
            AgentConfig::default(),
            skills,
            ToolRegistry::new(),
            Arc::new(AdapterRegistry::new()),
        ));
        // persona 系统技能 + 工作区插件允许（空白名单 = 全部启用）。
        agent
            .apply_capabilities(&crate::config::TeamMember {
                system_skills: vec!["persona-style".into()],
                ..Default::default()
            })
            .await;
        let store = Arc::new(crate::workspace::WorkspaceStore::load(None));
        store
            .upsert(echo_protocol::WorkspaceSessionInfo {
                id: "proj".into(),
                name: "Proj".into(),
                description: String::new(),
                directories: vec!["/srv/proj".into()],
            })
            .unwrap();
        store.set_active(Some("proj".into())).unwrap();
        agent.set_workspace_store(store);

        let runner = Arc::new(echo_loop::runner::TurnRunner::new(
            Arc::new(echo_context::EventBus::default()),
            provider.clone(),
            Arc::new(echo_loop::ToolPipeline::new()),
            echo_loop::LoopOptions::default(),
        ));
        agent.set_loop_runner(runner);
        agent.set_use_echo_loop(true);

        let session = agent
            .trunk
            .get_or_create(&SessionKey::local_tui(), "user".into(), None);
        assert_eq!(agent.process_message(&session, "hi").await.unwrap(), "done");

        let requests = provider.requests.lock().await;
        let system = requests[0].messages[0].content.clone();
        assert!(
            system.contains("PERSONA_SKILL_MARKER"),
            "persona 系统技能必须注入（echo-loop 路径）: {system}"
        );
        assert!(
            system.contains("# 当前工作区会话"),
            "工作区会话必须注入（echo-loop 路径）: {system}"
        );
    }

    #[test]
    fn sequence_parsing_only_accepts_structured_markers() {
        let hook = r#"<qq_message_hook>
{"channel":{"type":"private"},"message_sequence":7,"content":"hi"}
</qq_message_hook>"#;
        assert_eq!(structured_message_sequence(hook), Some(7));
        let backend = r#"<backend_message_hook>{"message_sequence":3}</backend_message_hook>"#;
        assert_eq!(structured_message_sequence(backend), Some(3));
        let timer = r#"<timer_event>{"message_sequence":9}</timer_event>"#;
        assert_eq!(structured_message_sequence(timer), Some(9));

        // Ordinary conversation containing braces must NOT be parsed.
        assert_eq!(
            structured_message_sequence("请对比 {\"a\":1} 和 {\"b\":2}"),
            None
        );
        assert_eq!(
            structured_message_sequence("message_sequence: 42 in plain text"),
            None
        );
        assert_eq!(structured_message_sequence("const x = {n: 1};"), None);
        // The marker must be at the very start.
        assert_eq!(
            structured_message_sequence(
                "note: <qq_message_hook>{\"message_sequence\":1}</qq_message_hook>"
            ),
            None
        );
    }

    #[test]
    fn extract_input_images_reads_hook_payload() {
        let hook = r#"<qq_message_hook>
        {"channel":{"type":"private"},"message_sequence":7,"content":"hi","images":["https://example.com/a.png","data:image/png;base64,AAA"]}
        </qq_message_hook>"#;
        assert_eq!(
            extract_input_images(hook),
            vec![
                "https://example.com/a.png".to_string(),
                "data:image/png;base64,AAA".to_string()
            ]
        );
        // 无图 hook 返回空
        let plain = r#"<qq_message_hook>{"message_sequence":1,"content":"hi","images":[]}</qq_message_hook>"#;
        assert!(extract_input_images(plain).is_empty());
        // 普通文本不解析
        assert!(extract_input_images("看一下这张图片 https://example.com/a.png").is_empty());
    }

    #[test]
    fn inbound_image_hook_yields_a_compact_model_request() {
        // 端到端护栏：大图入站后，模型请求里不允许再出现 base64 —— 图片只走
        // image 块，文本里是占位符（1.3MB 截图 ≈ 125 万文本 token 的根因）。
        let payload = "A".repeat(6000);
        let uri = format!("data:image/png;base64,{payload}");
        let hook = format!(
            "<backend_message_hook>{{\"message_sequence\":15,\"content\":\"看截图\",\"images\":[\"{uri}\"]}}</backend_message_hook>"
        );
        let images = extract_input_images(&hook);
        assert_eq!(images, vec![uri.clone()]);

        let log = vec![echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: None,
                content: hook.clone(),
                timestamp: 0,
                message_sequence: Some(15),
                source: None,
                images,
            },
        )];
        let messages = echo_session::derive::derive_messages(&log, 800_000);
        assert_eq!(messages.len(), 1);
        assert!(
            !messages[0].content.contains("AAAA"),
            "base64 must not reach the model text"
        );
        assert!(messages[0].content.contains("[图片#1]"));
        assert_eq!(
            messages[0].images.len(),
            1,
            "image block still carries the payload"
        );
        // 单条消息的估算成本从 6000+ 降到占位符级别。
        assert!(echo_defs::token::estimate_message_tokens(&messages[0]) < 500);
    }

    #[test]
    fn user_message_event_roundtrips_images() {
        let hook = r#"<qq_message_hook>{"message_sequence":3,"content":"看图","images":["https://example.com/a.png"]}</qq_message_hook>"#;
        let event = echo_session::event::UserMessage {
            session: None,
            content: hook.into(),
            timestamp: 0,
            message_sequence: Some(3),
            source: None,
            images: extract_input_images(hook),
        };
        let json =
            serde_json::to_string(&echo_session::SessionEvent::UserMessage(event.clone())).unwrap();
        let back: echo_session::SessionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, echo_session::SessionEvent::UserMessage(event));
    }

    #[test]
    fn probe_config_resolves_active_profile_and_named_profiles() {
        use crate::config::ApiProfile;
        let mut cfg = crate::config::AgentConfig {
            provider: "deepseek".into(),
            model: "deepseek-v4-flash".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: "sk-xxx".into(),
            active_api: "deepseek".into(),
            ..Default::default()
        };
        cfg.api_profiles.push(ApiProfile {
            name: "deepseek".into(),
            provider: "deepseek".into(),
            model: "deepseek-v4-flash".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: "sk-xxx".into(),
            thinking: crate::config::ThinkingMode::Enabled,
            reasoning_effort: crate::config::ReasoningEffort::Max,
        });
        cfg.api_profiles.push(ApiProfile {
            name: "openai".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            thinking: crate::config::ThinkingMode::Disabled,
            reasoning_effort: crate::config::ReasoningEffort::Low,
        });

        // 默认配置（name=''）：合并激活 profile → provider 非空。
        let probe = resolve_probe_config(&cfg, "").expect("default probe resolves");
        assert_eq!(probe.provider, "deepseek");
        assert!(!probe.api_key.is_empty(), "api key resolved");

        // 指定 profile：使用该 profile 的值。
        let probe = resolve_probe_config(&cfg, "openai").expect("openai probe resolves");
        assert_eq!(probe.provider, "openai");
        assert_eq!(probe.model, "gpt-4o");

        // 不存在的 profile → 明确报错。
        let err = resolve_probe_config(&cfg, "nope").unwrap_err();
        assert!(err.contains("profile not found"));
    }

    #[test]
    fn deepseek_balance_endpoint_maps_official_variants() {
        // Anthropic / OpenAI / beta / 裸域 → 统一 /user/balance
        assert_eq!(
            deepseek_balance_endpoint("https://api.deepseek.com/anthropic").as_deref(),
            Some("https://api.deepseek.com/user/balance")
        );
        assert_eq!(
            deepseek_balance_endpoint("https://api.deepseek.com/v1").as_deref(),
            Some("https://api.deepseek.com/user/balance")
        );
        assert_eq!(
            deepseek_balance_endpoint("https://api.deepseek.com").as_deref(),
            Some("https://api.deepseek.com/user/balance")
        );
        assert_eq!(
            deepseek_balance_endpoint("https://api.deepseek.com/").as_deref(),
            Some("https://api.deepseek.com/user/balance")
        );
        // 非 DeepSeek 域 → None（前端不展示余额入口）
        assert!(deepseek_balance_endpoint("https://uuapi.io/v1").is_none());
        assert!(deepseek_balance_endpoint("https://api.openai.com/v1").is_none());
    }

    #[test]
    fn probe_config_reports_missing_provider() {
        let cfg = crate::config::AgentConfig::default();
        let err = resolve_probe_config(&cfg, "").unwrap_err();
        assert!(
            err.contains("provider"),
            "error must mention provider: {err}"
        );
    }

    #[tokio::test]
    async fn max_tool_iterations_returns_error() {
        let cfg = AgentConfig {
            max_tool_iterations: 2,
            ..Default::default()
        };
        let script = vec![
            tool_call("c1", "mock_tool"),
            tool_call("c2", "mock_tool"),
            tool_call("c3", "mock_tool"),
        ];
        let provider = Arc::new(ScriptedProvider::new(script));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MockTool {
            name: "mock_tool",
            result: "x".into(),
        }));

        let agent = Agent::new(
            provider,
            cfg,
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let key = SessionKey::local_tui();
        let session = agent.trunk.get_or_create(&key, "user".into(), None);
        let err = agent.process_message(&session, "loop").await.unwrap_err();
        assert!(err.to_string().contains("max tool iterations"));
    }

    #[tokio::test]
    async fn tool_execution_failure_is_surfaced_to_llm() {
        let script = vec![
            tool_call("call_1", "missing_tool"),
            ChatResponse {
                stop_reason: None,
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            },
        ];
        let provider = Arc::new(ScriptedProvider::new(script));
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            ToolRegistry::new(),
            Arc::new(AdapterRegistry::new()),
        );
        let key = SessionKey::local_tui();
        let session = agent.trunk.get_or_create(&key, "user".into(), None);
        let reply = agent.process_message(&session, "hi").await.unwrap();
        assert_eq!(reply, "done");
    }

    // ── 工作区会话（workspace 插件）──

    /// 工作区命令走 team 路由（去主智能体后不落"默认人格"兜底），
    /// 且 save → list → active → git 的完整链路可用。
    #[tokio::test]
    async fn workspace_commands_lifecycle() {
        let agent = Arc::new(test_agent(Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        })));
        agent.set_team_id(Some("t".into()));
        let path = std::env::temp_dir().join(format!(
            "echo-workspace-agent-test-{}.json",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
        agent.set_workspace_store(Arc::new(crate::workspace::WorkspaceStore::load(Some(
            path.clone(),
        ))));

        let (bridge, handle) = crate::create_bridge();
        agent.attach(Arc::new(handle));

        // 保存（空 id → 服务端生成）。
        agent
            .apply_command(BackendCommand::SaveWorkspaceSession {
                team_id: Some("t".into()),
                session: echo_protocol::WorkspaceSessionInfo {
                    id: String::new(),
                    name: "core".into(),
                    description: String::new(),
                    directories: vec![env!("CARGO_MANIFEST_DIR").into()],
                },
            })
            .await;
        // 激活。
        agent
            .apply_command(BackendCommand::ActivateWorkspaceSession {
                team_id: Some("t".into()),
                id: Some("core".into()),
            })
            .await;
        // 列表。
        agent
            .apply_command(BackendCommand::RequestWorkspaceSessions {
                team_id: Some("t".into()),
            })
            .await;
        // git 状态（真实仓库 checkout）。
        agent
            .apply_command(BackendCommand::RequestWorkspaceGitStatus {
                team_id: Some("t".into()),
                session_id: "core".into(),
            })
            .await;

        let mut events = Vec::new();
        while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
            events.push(event);
        }
        // 取最后一次列表快照（save → activate 都会推送列表）。
        let list = events
            .iter()
            .rev()
            .find_map(|e| match e {
                BackendEvent::WorkspaceSessions {
                    sessions, active, ..
                } => Some((sessions.clone(), active.clone())),
                _ => None,
            })
            .expect("WorkspaceSessions emitted");
        assert_eq!(list.0.len(), 1);
        assert_eq!(list.0[0].id, "core");
        assert_eq!(list.1.as_deref(), Some("core"), "activation survives");

        let git = events
            .iter()
            .find_map(|e| match e {
                BackendEvent::WorkspaceGitStatus { directories, .. } => Some(directories.clone()),
                _ => None,
            })
            .expect("WorkspaceGitStatus emitted");
        assert_eq!(git.len(), 1);
        assert!(git[0].is_repo, "repo checkout detected: {git:?}");

        // 提示注入：激活后 build_prompt_blocks 含 workspace 区块。
        let store = agent.workspace_store().expect("store attached");
        let text = store.prompt_text().expect("active prompt text");
        assert!(text.contains("core"));
        assert!(text.contains(env!("CARGO_MANIFEST_DIR")));

        // 激活 = 进入项目对话：通道会话注册并广播 SessionUpdated。
        let channel = events
            .iter()
            .find_map(|e| match e {
                BackendEvent::SessionUpdated { session }
                    if session.id == "local:workspace:core:local_user" =>
                {
                    Some(session)
                }
                _ => None,
            })
            .expect("channel SessionUpdated emitted");
        assert_eq!(channel.nickname, "core");
        assert_eq!(channel.team_id.as_deref(), Some("t"));
        // 通道会话常驻 trunk registry（RequestState 会带上它，供切换器展示）。
        assert!(
            agent
                .trunk
                .all()
                .iter()
                .any(|s| s.id == "local:workspace:core:local_user"),
            "channel session registered"
        );

        std::fs::remove_file(&path).ok();
    }

    /// 文件浏览器：列出会话目录（拒绝越界路径，返回条目含目录与文件）。
    #[tokio::test]
    async fn workspace_files_command_lists_within_scope() {
        let agent = Arc::new(test_agent(Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        })));
        agent.set_team_id(Some("t".into()));
        let path = std::env::temp_dir().join(format!(
            "echo-workspace-files-test-{}.json",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
        agent.set_workspace_store(Arc::new(crate::workspace::WorkspaceStore::load(Some(
            path.clone(),
        ))));

        let (bridge, handle) = crate::create_bridge();
        agent.attach(Arc::new(handle));

        let root = env!("CARGO_MANIFEST_DIR").to_string();
        agent
            .apply_command(BackendCommand::SaveWorkspaceSession {
                team_id: Some("t".into()),
                session: echo_protocol::WorkspaceSessionInfo {
                    id: "core".into(),
                    name: "core".into(),
                    description: String::new(),
                    directories: vec![root.clone().into()],
                },
            })
            .await;
        // 列目录（会话目录本身）。
        agent
            .apply_command(BackendCommand::RequestWorkspaceFiles {
                team_id: Some("t".into()),
                session_id: "core".into(),
                path: root.clone(),
            })
            .await;
        // 越界路径（/etc 不在会话目录内）→ 错误事件。
        agent
            .apply_command(BackendCommand::RequestWorkspaceFiles {
                team_id: Some("t".into()),
                session_id: "core".into(),
                path: "/etc".into(),
            })
            .await;

        let mut events = Vec::new();
        while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
            events.push(event);
        }
        let listings: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::WorkspaceFiles {
                    path: p,
                    entries,
                    error,
                    ..
                } => Some((p.clone(), entries.clone(), error.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(listings.len(), 2, "two listings expected");
        // 第一次：合法目录，条目非空且含 src（本仓库 checkout）。
        let (_, entries, error) = &listings[0];
        assert!(error.is_none(), "in-scope listing error: {error:?}");
        assert!(
            entries.iter().any(|e| e.name == "src" && e.is_dir),
            "src/ expected: {entries:?}"
        );
        // 第二次：越界拒绝。
        let (_, entries, error) = &listings[1];
        assert!(entries.is_empty());
        assert!(
            error
                .as_deref()
                .unwrap_or("")
                .contains("不在该会话的工作区目录内"),
            "unexpected: {error:?}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// 通道广播唯一入口（store 变更钩子）：
    /// - 重启恢复：装配前 store 已带 active → `set_workspace_store` 补注册通道；
    /// - 模型侧 `workspace` 工具 `use`：与面板命令同一条广播路径
    ///   （`WorkspaceSessions` + 通道 `SessionUpdated`）。
    #[tokio::test]
    async fn workspace_channel_tool_use_broadcasts() {
        let store = Arc::new(crate::workspace::WorkspaceStore::load(None));
        for (id, name) in [("core", "Core"), ("docs", "Docs")] {
            store
                .upsert(echo_protocol::WorkspaceSessionInfo {
                    id: id.into(),
                    name: name.into(),
                    description: String::new(),
                    directories: vec!["/srv/x".into()],
                })
                .unwrap();
        }
        // 模拟持久化恢复：激活发生在进程装配之前。
        store.set_active(Some("core".into())).unwrap();

        let agent = Arc::new(test_agent(Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        })));
        agent.set_team_id(Some("t".into()));
        agent.set_workspace_store(store.clone());
        assert!(
            agent
                .trunk
                .all()
                .iter()
                .any(|s| s.id == "local:workspace:core:local_user"),
            "startup ensure registers the persisted active channel"
        );

        let (bridge, handle) = crate::create_bridge();
        agent.attach(Arc::new(handle));

        // 模型侧工具直接切换激活（不经命令路径）。
        let tool = crate::workspace::WorkspaceTool::new(store.clone());
        crate::tool::Tool::execute(&tool, serde_json::json!({"operation": "use", "id": "docs"}))
            .await
            .expect("tool use");

        let mut events = Vec::new();
        while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
            events.push(event);
        }
        let active = events
            .iter()
            .rev()
            .find_map(|e| match e {
                BackendEvent::WorkspaceSessions { active, .. } => Some(active.clone()),
                _ => None,
            })
            .expect("WorkspaceSessions broadcast on tool use");
        assert_eq!(active.as_deref(), Some("docs"));
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::SessionUpdated { session }
                    if session.id == "local:workspace:docs:local_user"
            )),
            "tool use broadcasts the channel SessionUpdated"
        );
        assert!(
            agent
                .trunk
                .all()
                .iter()
                .any(|s| s.id == "local:workspace:docs:local_user"),
            "tool use registers the channel"
        );
    }

    /// workspace 插件门控：白名单外人格的工具包被禁用、提示词区块不注入；
    /// 白名单内人格工具可见且激活会话时注入「工作区会话」区块。
    #[tokio::test]
    async fn workspace_plugin_gates_tool_and_prompt_block() {
        let mut tools = ToolRegistry::new();
        let store = Arc::new(crate::workspace::WorkspaceStore::load(None));
        store
            .upsert(echo_protocol::WorkspaceSessionInfo {
                id: "proj".into(),
                name: "Proj".into(),
                description: String::new(),
                directories: vec!["/srv/proj".into()],
            })
            .unwrap();
        store.set_active(Some("proj".into())).unwrap();
        tools.register(Arc::new(crate::workspace::WorkspaceTool::new(
            store.clone(),
        )));
        tools.set_package("workspace", crate::plugins::WORKSPACE_PLUGIN_ID);
        let agent = Arc::new(Agent::new(
            Arc::new(MockProvider {
                calls: Arc::new(AtomicUsize::new(0)),
                reply: "ok".into(),
            }),
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        ));
        agent.set_workspace_store(store);

        // 白名单含 workspace → 工具可见 + 提示区块注入。
        agent
            .apply_capabilities(&crate::config::TeamMember {
                enabled_plugins: vec![
                    crate::plugins::TOOLS_BUILTIN_PLUGIN_ID.into(),
                    crate::plugins::WORKSPACE_PLUGIN_ID.into(),
                ],
                ..Default::default()
            })
            .await;
        let names: Vec<String> = agent
            .tools
            .definitions()
            .await
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert!(names.contains(&"workspace".to_string()), "tools: {names:?}");
        let blocks = agent.build_prompt_blocks().await;
        assert!(
            blocks.iter().any(|b| b.key == "workspace"),
            "workspace block injected"
        );
        assert!(blocks
            .iter()
            .find(|b| b.key == "workspace")
            .unwrap()
            .content
            .contains("/srv/proj"));

        // 白名单不含 workspace → 工具包禁用 + 区块消失。
        agent
            .apply_capabilities(&crate::config::TeamMember {
                enabled_plugins: vec![crate::plugins::TOOLS_BUILTIN_PLUGIN_ID.into()],
                ..Default::default()
            })
            .await;
        let names: Vec<String> = agent
            .tools
            .definitions()
            .await
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert!(
            !names.contains(&"workspace".to_string()),
            "tools: {names:?}"
        );
        let blocks = agent.build_prompt_blocks().await;
        assert!(
            !blocks.iter().any(|b| b.key == "workspace"),
            "workspace block hidden without the plugin"
        );
    }

    /// 缺 team_id 的工作区命令被拒绝（与其它会话类命令同一约定）。
    #[tokio::test]
    async fn workspace_commands_require_team_id() {
        let agent = Arc::new(test_agent(Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        })));
        agent.set_team_id(Some("t".into()));
        let (bridge, handle) = crate::create_bridge();
        agent.attach(Arc::new(handle));
        agent
            .apply_command(BackendCommand::RequestWorkspaceSessions { team_id: None })
            .await;
        let mut events = Vec::new();
        while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
            events.push(event);
        }
        let message = events
            .iter()
            .find_map(|e| match e {
                BackendEvent::Error { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("error emitted");
        assert!(message.contains("team_id"), "unexpected: {message}");
    }
}
