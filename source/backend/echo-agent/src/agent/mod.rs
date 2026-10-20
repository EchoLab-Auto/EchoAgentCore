//! The agent loop: LLM + tools + skills + memory composed into one reply.

mod api_admin;
mod boundary;
pub mod builder;
mod commands;
mod compact;
mod gating;
mod inbound;
mod plugin_reload;
mod prompt;
pub mod registry;
mod streaming;
mod subagent_runtime;
mod tool_dispatch;
mod tool_exec;
mod turn;
mod workspace_commands;
pub mod workspace_host;
// 命令域按包拆分：QQ 侧在 `crate::packages::instance_ops::commands`，
// 工作区侧在本目录 `workspace_commands.rs`（插件实现已外迁到
// `echo-plugin-workspace`，此处只留 Agent 接线）。

use boundary::PromptBlock;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// 进程级事件汇聚点：任意 persona 的 `emit` 都投递到此，组合根保证
/// 所有人格（含运行期新建）共享同一汇聚点，Panel 单连接即可看到全部活动。
pub type EventSink = std::sync::Arc<dyn Fn(BackendEvent) + Send + Sync>;
use std::sync::Arc;

use anyhow::Result;
use dashmap::DashMap;
use tokio::sync::{Mutex, RwLock};

use echo_defs::plugin_instance::InstanceRegistry;

use crate::bridge::BackendHandle;
use crate::command::BackendCommand;
use crate::config::{AgentConfig, ApiProfile};
use crate::event::{BackendEvent, InstanceMetadataEntry, InstanceStatus};
use crate::llm::{create_provider, ChatChunk, ChatMessage, ChatRequest, LlmProvider, ToolCall};
use crate::session::{Session, TrunkStore};
use crate::skill::SkillRegistry;
use crate::tool::ToolRegistry;

/// 「本轮被调用方取消」的错误文案（口径单源 = `echo_defs::llm`）。
///
/// 取消不是故障：主 turn / 子 agent / 会话排空主动中止时一律用这一句，
/// 上层按字符串识别（`inbound.rs` 的 `is_cancelled_error`）。
pub(crate) use echo_defs::llm::TURN_CANCELLED;
/// Graceful-shutdown drain window: how long to wait for in-flight turns to
/// finish before force-cancelling (self-update interruption guard).
const SHUTDOWN_DRAIN_SECS: u64 = 120;
const MAX_CONCURRENT_REPLY_BRANCHES: usize = 8;

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

/// The agent: owns provider, tools, skills, sessions, instances, and emits events.
pub struct Agent {
    /// 模型出口（S4a）：provider / 配置 / 模型名 / profile 与会话级覆盖。
    ///
    /// 这四件事必须**一起变**（切 provider 要同时改实例与配置；切 profile 要
    /// 同时改 persona 标记与会话覆盖），所以它们现在有一个共同的持有者而不是
    /// 散在 `Agent` 的 8 个字段里。不变式表见 [`crate::model_service`]。
    ///
    /// `Arc` 而不是内联：后续阶段要把这个句柄单独递给别的服务（读配置不需要
    /// 整只 `Agent`）。
    pub(crate) model: Arc<crate::model_service::ModelService>,
    pub tools: Arc<ToolRegistry>,
    pub skills: Arc<Mutex<SkillRegistry>>,
    pub trunk: TrunkStore,
    /// Platform instances managed externally (registered by main / config).
    pub instances: Arc<InstanceRegistry>,
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
    /// `llm::wrap_redacting` 装饰 provider，见组合根装配）、插件实例出站
    /// （`core::instance_tools` 闸门）。设计见 `document/core/security-redaction-design.md`。
    redactor: std::sync::RwLock<Option<Arc<dyn echo_defs::sanitize::Redactor>>>,
    /// API 指标存储（余额快照 / token 用量；组合根装配期注入）。
    /// None = 不记录（测试 / 未接线的旧路径）。
    metrics: std::sync::RwLock<Option<Arc<crate::metrics::MetricsStore>>>,
    system_prompt_cache: RwLock<Option<String>>,
    /// Decomposed system-prompt blocks of the most recent turn **per
    /// session**, kept so the panel can visualize the exact prompt sections
    /// sent to the LLM. 2026-10 上下文审计 🟡 修复：此前为 agent 级单槽，
    /// 多会话时 /context 展示的是另一会话最近 turn 的提示词块（串会话）。
    last_prompt_blocks: DashMap<String, Vec<PromptBlock>>,
    plugin_reload_started: AtomicBool,
    /// Turn 驱动（echo-loop TurnRunner，唯一循环实现）。组合根注入共享
    /// runner；未注入时首个 turn 按本 agent 的 provider/配置惰性构建
    /// （见 `turn::turn_runner`）。
    loop_runner: RwLock<Option<std::sync::Arc<echo_loop::runner::TurnRunner>>>,
    reply_branch_slots: tokio::sync::Semaphore,
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
    /// 全局禁用策略（`[agent].disabled_tools/disabled_skills`）：进程级开关
    /// 不属于任何人格，组合根注入同一份。None = 未接线（调用方回退本 agent
    /// 的配置快照，单元测试走这条）。
    ///
    /// 2026-10 技术债清理：此前存在进程级 `echo_bootstrap` 锚点，读取方是
    /// `&self` 方法——上下文本来就在手上，却去读全局。改为字段注入后，
    /// 依赖变显式，同一进程的两套宿主也互不干扰。
    global_policy: std::sync::RwLock<Option<std::sync::Arc<GlobalPolicy>>>,
    /// 所属 [`AgentManager`]（**弱引用**：manager 强持有 agents，agents 弱
    /// 指回它，避免引用环）。
    ///
    /// 2026-10 技术债清理：此前 21 处读取走 `echo_bootstrap` 进程级槽。
    /// 但"我属于哪个 manager"天然是**本 agent 的属性**——现在显式持有，
    /// 同一进程两套 manager 不会互相看见对方的人格表。
    /// 兄弟 agent 的查询面（**弱引用**：manager 强持有 agents，agents 弱持有
    /// 它，否则成环泄漏）。
    ///
    /// 类型是 `dyn AgentRegistry` 而不是 `AgentManager`（S4b）：`Agent` 只需要
    /// "按 id 找兄弟 / 列全部 / 报清单"，不需要管理能力。团队管理动作已经在
    /// [`crate::agent_manager`] 的自由函数里，不再经过 `Agent`。
    registry: std::sync::RwLock<Option<std::sync::Weak<dyn crate::agent::registry::AgentRegistry>>>,
    /// 团队管理面（**弱引用**，与 `registry` 同一份 manager 的两个面向）。
    ///
    /// 分成两个字段而不是一个："查兄弟"与"增删改人格"是不同关注点，测试可只造
    /// 前者。**这是技术债**：团队 CRUD 本该由进程级处理器接掉，而不是让被命令
    /// 泵选中的 agent 代办（详见 [`crate::agent::registry::TeamAdmin`]）。
    admin: std::sync::RwLock<Option<std::sync::Weak<dyn crate::agent::registry::TeamAdmin>>>,
    /// 「未绑定 manager」告警只报一次（避免每命令刷屏）。
    manager_warned: std::sync::atomic::AtomicBool,
    /// Shell 子系统句柄（组合根创建并注入；工具与命令侧共享同一份）。
    /// 未注入 = 空句柄（shell 命令回「manager unavailable」）。
    ///
    /// 2026-10 技术债清理：此前 shell manager / emit 读的是进程级锚点。
    shell: echo_shell::ShellHandle,
    /// Team id (None/empty = default). Sets session tagging after boot.
    team_id: std::sync::Mutex<Option<String>>,
    /// 联邦出口集合（invoker / querier / notifier / deliver / aggregator）：
    /// 组合根装配联邦时构造一份，经 [`crate::agent::builder::AgentBuilder`]
    /// 注入。缺省 = 联邦未接线（全部出口为空，远程功能 fail-closed 并给出
    /// 可读错误）。**P3 终态：不再是 5 个进程级 kernel cell。**
    federation: std::sync::RwLock<std::sync::Arc<crate::federation::FederationPorts>>,
    /// 节点身份（federation NodeId + 运行区域名）：组合根构造一份，装配期
    /// 注入（[`Self::set_node_identity`]）。缺省为空 = 纯嵌入式用法（线格式
    /// 里不带这两个字段）。**不再是进程级 kernel cell**——身份是构造参数。
    node_identity: std::sync::RwLock<crate::NodeIdentity>,
    /// Graceful-drain flag: set during shutdown so new turns are rejected
    /// while in-flight replies finish (self-update continuity).
    draining: AtomicBool,
    capabilities: std::sync::Mutex<Option<crate::config::AgentProfile>>,
    /// Subagent 插件运行态：子任务注册表 + spawn 执行闭包（组合根装配；
    /// None = 插件未启用，spawn_subagent 对模型不可见）。
    subagent: std::sync::RwLock<Option<SubagentRuntime>>,
    /// 门控日志去重（`plugin_id` → 上次记录的最终启用态）。
    ///
    /// `apply_plugin_gating` 在每轮能力重算里被逐 persona 调用，绝大多数
    /// 调用是幂等的；只按「状态真的翻转」打日志，把默认级别下的
    /// `plugin tool/skill package toggled` 从 321 条/天降到仅剩真实开关。
    gating_log_state: DashMap<String, bool>,
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
            .field("active_model", &self.model.active_model)
            .field("adapters", &self.instances.names())
            .finish_non_exhaustive()
    }
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

// ── 联邦命令处理器（federation Phase 4）──
//
// `SaveFederationPeer` 等命令的处理需要组合根持有的 Federation 句柄与
// ConfigStore——不在 Agent 能力面内。组合根装配期把它放进
// [`crate::federation::FederationPorts::command_handler`]（P3 终态：参数
// 下发，不再是 kernel cell）；未注入时命令明确报错（联邦关闭）。签名用
// boxed future：handler 由组合根闭包提供，捕获其句柄。
/// 联邦命令处理器签名：接受 Agent 引用做同步分发，返回 `'static`
/// future（实现侧如需跨 await 使用 Agent 能力，应提取所需句柄——
/// emit 直接经引用同步调用即可，handler 本身可以就是同步实现
/// 包一层 ready future）。
pub type FederationCommandHandler = std::sync::Arc<
    dyn Fn(&Agent, echo_protocol::BackendCommand) -> FederationHandlerFuture + Send + Sync,
>;

pub type FederationHandlerFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>;

/// 单步推理落盘上限（字符）。超出截断并标注——推理正文只在面板里读，
/// 而事件日志是**整份 JSON、30 秒一次整体重写**：不设上限时少数几步超长
/// 推理就能把日志撑到几十 MB（推理体积通常是正文的数倍）。
///
/// 线上传输另有更激进的瘦身（`timeline.rs::elide_reasoning_for_wire`：
/// 只有最近 40 条保留全文，更早的截成 240 字），所以这里的 2 万字只影响
/// "保留全文"的那批条目。
const MAX_REASONING_EVENT_CHARS: usize = 20_000;

/// 按 [`MAX_REASONING_EVENT_CHARS`] 截断推理正文（保留头部 + 明确标注）。
fn cap_reasoning_for_log(content: &str) -> String {
    let total = content.chars().count();
    if total <= MAX_REASONING_EVENT_CHARS {
        return content.to_string();
    }
    let head: String = content.chars().take(MAX_REASONING_EVENT_CHARS).collect();
    format!("{head}…（推理过长已截断：原文 {total} 字）")
}

impl Agent {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        config: AgentConfig,
        skills: SkillRegistry,
        tools: ToolRegistry,
        instances: Arc<InstanceRegistry>,
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
            model: Arc::new(crate::model_service::ModelService::new(provider, config)),
            tools: Arc::new(tools),
            skills: Arc::new(Mutex::new(skills)),
            trunk,
            instances,
            handle: tokio::sync::RwLock::new(None),
            event_sink: std::sync::RwLock::new(None),
            redactor: std::sync::RwLock::new(None),
            metrics: std::sync::RwLock::new(None),
            system_prompt_cache: RwLock::new(None),
            last_prompt_blocks: DashMap::new(),
            plugin_reload_started: AtomicBool::new(false),
            loop_runner: RwLock::new(None),
            reply_branch_slots: tokio::sync::Semaphore::new(MAX_CONCURRENT_REPLY_BRANCHES),
            active_inbound_turns: DashMap::new(),
            next_message_sequence: AtomicU64::new(1),
            event_bus,
            _timeline_projection,
            cancel,
            plugin_host: std::sync::RwLock::new(std::sync::Arc::new(
                crate::plugins::PluginHost::new(),
            )),
            global_policy: std::sync::RwLock::new(None),
            registry: std::sync::RwLock::new(None),
            admin: std::sync::RwLock::new(None),
            manager_warned: std::sync::atomic::AtomicBool::new(false),
            shell: echo_shell::ShellHandle::new(),
            workspace_store: std::sync::RwLock::new(None),
            team_id: std::sync::Mutex::new(None),
            federation: std::sync::RwLock::new(std::sync::Arc::new(
                crate::federation::FederationPorts::unlinked(),
            )),
            node_identity: std::sync::RwLock::new(crate::NodeIdentity::default()),
            draining: AtomicBool::new(false),
            capabilities: std::sync::Mutex::new(None),
            subagent: std::sync::RwLock::new(None),
            gating_log_state: DashMap::new(),
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
        self.set_config_store(echo_config::ConfigStore::new(p));
    }

    /// Attach a shared [`echo_config::ConfigStore`] for `[agent]` persistence.
    ///
    /// 只接管 TOML 配置持久化，不动会话路径（会话路径由
    /// [`Self::set_session_persist_path`] 独立设置）。Called once at startup —
    /// uses try_lock since no concurrent access exists yet.
    pub fn set_config_store(&self, store: echo_config::ConfigStore) {
        if let Ok(mut slot) = self.model.config_store.try_lock() {
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
            let mut ticks: u64 = 0;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        agent.trunk.maybe_save().await;
                        // 时间线探针（2026-10）：每 10 分钟采样一次时间线窗口
                        // 状态，供"面板历史无声消失"类事故定位——见
                        // `TrunkStore::probe_timeline`。
                        if ticks.is_multiple_of(20) {
                            agent.trunk.probe_timeline();
                        }
                        ticks += 1;
                    }
                    _ = cancel.cancelled() => break,
                }
            }
        });
    }

    /// 热重载技能：合并发现「出厂层（skills_dirs）+ 用户层（skills_dir，
    /// 同名覆盖）」。传入的 `write_dir` 仅用于判断用户层是否存在（不存在
    /// 只意味着没有用户技能，出厂层仍照常加载）。
    pub(crate) async fn reload_skills(&self, write_dir: &str) -> Result<bool, String> {
        let mut dirs = self.model.config.read().await.skills_dirs.clone();
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
    /// 注入共享 turn 驱动（echo-loop TurnRunner；组合根装配时调用）。
    pub fn set_loop_runner(&self, runner: std::sync::Arc<echo_loop::runner::TurnRunner>) {
        if let Ok(mut slot) = self.loop_runner.try_write() {
            *slot = Some(runner);
        } else {
            tracing::warn!("loop runner slot busy, ignoring set_loop_runner");
        }
    }

    pub fn set_team_id(&self, team_id: Option<String>) {
        *self
            .team_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = team_id.clone();
        self.trunk.set_team_id(team_id);
    }

    pub fn team_id(&self) -> Option<String> {
        self.team_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 注入节点身份（组合根装配期；`AgentBuilder::node_identity` 调用）。
    ///
    /// 幂等覆盖（可在运行期重新注入，例如测试或多节点聚合场景切身份）——
    /// 与旧 kernel cell 的"首次生效"不同：那是全局槽位的约束，这里是实例
    /// 字段，后者更直观。
    pub fn set_node_identity(&self, identity: crate::NodeIdentity) {
        *self
            .node_identity
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = identity;
    }

    /// 当前节点身份（未注入 = 全空）。
    pub fn node_identity(&self) -> crate::NodeIdentity {
        self.node_identity
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 注入联邦出口集合（组合根装配期；`AgentBuilder::federation` 调用）。
    pub fn set_federation(&self, ports: std::sync::Arc<crate::federation::FederationPorts>) {
        *self
            .federation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ports;
    }

    /// 当前联邦出口集合（未注入 = 全部为空，即联邦未接线）。
    pub fn federation(&self) -> std::sync::Arc<crate::federation::FederationPorts> {
        self.federation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 该 agent 的循环模式（互斥循环插件推导，见
    /// [`crate::plugins::SINGLE_LOOP_PLUGIN_ID`] /
    /// [`crate::plugins::PARALLEL_LOOP_PLUGIN_ID`]）。
    /// 未配置 capabilities 时默认 `Single`（单会话，见
    /// [`echo_defs::LoopMode`]）。
    pub fn loop_mode(&self) -> echo_defs::LoopMode {
        let guard = self
            .capabilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

    /// 注入全局禁用策略（组合根对每个 persona 调用一次）。
    pub fn set_global_policy(&self, policy: std::sync::Arc<GlobalPolicy>) {
        if let Ok(mut slot) = self.global_policy.write() {
            *slot = Some(policy);
        } else {
            tracing::warn!("global policy slot busy, ignoring set_global_policy");
        }
    }

    /// 绑定兄弟 agent 的查询面（组合根 / `AgentManager::bound` 调用一次）。
    ///
    /// 收窄成 `&dyn AgentRegistry`：调用方传 `&AgentManager`（它实现了该 trait）
    /// 即可，而 `Agent` 侧不需要知道具体类型。
    pub fn set_host(
        &self,
        registry: &std::sync::Arc<dyn crate::agent::registry::AgentRegistry>,
        admin: &std::sync::Arc<dyn crate::agent::registry::TeamAdmin>,
    ) {
        if let Ok(mut slot) = self.registry.write() {
            *slot = Some(std::sync::Arc::downgrade(registry));
        } else {
            tracing::warn!("agent registry slot busy, ignoring set_host");
        }
        if let Ok(mut slot) = self.admin.write() {
            *slot = Some(std::sync::Arc::downgrade(admin));
        } else {
            tracing::warn!("agent admin slot busy, ignoring set_host");
        }
    }

    /// 兄弟 agent 的查询面（未绑定 / 已被回收 = `None`）。
    ///
    /// **不要在新代码里用它**：直接用 [`Self::require_registry`]，让"未绑定"
    /// 变成显式错误而不是静默降级——这正是 2026-10-20 事故的教训
    /// （`None => {}` 让 Panel 收到空清单而日志里什么也没有）。
    pub fn registry(&self) -> Option<std::sync::Arc<dyn crate::agent::registry::AgentRegistry>> {
        let found = self
            .registry
            .read()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(|weak| weak.upgrade()));
        if found.is_none()
            && !self
                .manager_warned
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                "agent has no registry bound; sibling lookup (TeamsList / command \
                 forwarding / cross-persona broadcast) will fail explicitly"
            );
        }
        found
    }

    /// 取查询面，未绑定即**明确错误**（fail-closed）。
    ///
    /// 与 [`Self::registry`] 的区别就是"失败时怎么办"：这里返回 `Err` 让调用方
    /// 必须处理（通常是回一条可见的 `Error` 事件），而不是 `if let Some(..)`
    /// 静默跳过。原 `manager()` 是后者，那让漏绑藏了很久。
    pub fn require_registry(
        &self,
    ) -> Result<std::sync::Arc<dyn crate::agent::registry::AgentRegistry>, String> {
        self.registry().ok_or_else(|| {
            "agent registry unavailable（该 agent 未被装入 AgentManager）".to_string()
        })
    }

    /// 取团队管理面，未绑定即**明确错误**（fail-closed）。
    ///
    /// 原来这些命令走 `if let Some(mgr) = self.manager() { … } else { emit Error }`
    /// ——失败时至少还有回执。改成 `Result` 是为了让**所有**调用点必须处理，
    /// 包括那些原本写 `None => {}` 静默跳过的（`ToggleTeam` 就是）。
    pub(crate) fn require_admin(
        &self,
    ) -> Result<std::sync::Arc<dyn crate::agent::registry::TeamAdmin>, String> {
        self.admin
            .read()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(|weak| weak.upgrade()))
            .ok_or_else(|| {
                "agent manager unavailable（该 agent 未被装入 AgentManager）".to_string()
            })
    }

    /// Shell 子系统句柄（见字段文档）。
    pub fn shell(&self) -> &echo_shell::ShellHandle {
        &self.shell
    }

    /// 本 agent 的全局禁用策略（未注入 = None，调用方回退自身配置快照）。
    pub fn global_policy(&self) -> Option<std::sync::Arc<GlobalPolicy>> {
        self.global_policy.read().ok().and_then(|slot| slot.clone())
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
                let handle = tokio::runtime::Handle::try_current();
                if let Ok(handle) = handle {
                    handle.spawn(async move {
                        agent.workspace_after_change().await;
                    });
                }
            }
        }));
        // 多对话（2026-10）：删除对话 → 清空其通道历史（会话级清理语义，
        // 与面板 ClearHistory 同路径；面板命令与模型工具都经 store 钩子）。
        let weak2 = std::sync::Arc::downgrade(self);
        store.set_on_conversation_deleted(std::sync::Arc::new(move |ws_id, conv| {
            let Some(agent) = weak2.upgrade() else {
                return;
            };
            let scope = crate::workspace::channel_scope_id(ws_id, conv);
            let session_id = crate::session::SessionKey::local_workspace(&scope).to_session_id();
            let trunk = agent.trunk.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    trunk.clear_session(&session_id).await;
                    tracing::info!(%session_id, "workspace conversation channel cleared on delete");
                });
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
        // 插件事件出口的快照（与 bus/sink 同口径：创建时快照，避免跨 await
        // 边界持锁）。`None` = 当时未注入宿主。
        let host_handle = self.plugin_host.read().ok().map(|g| g.clone());
        let team = self.team_id();
        std::sync::Arc::new(move |event| {
            // 与 emit 同口径（2026-10 修复）：此前本路径绕过 annotate_team
            // 且留了 `let _ = &team` 死代码——经它发射的事件若携带会话归属
            // 会静默漏标 team（Panel 按 team 过滤时串显）。现共享
            // annotate_team_for（team 快照于创建时，与 sink/handle 一致）。
            let event = Self::annotate_team_for(event, team.clone());
            if let Some(host) = host_handle.as_ref() {
                if host.has_event_subscribers() {
                    host.deliver_backend_event(&event);
                }
            }
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
        self.deliver_event_to_plugins(&event);
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

    /// 把一条领域事件投给订阅了它的已挂载插件（plugin-framework v0.7 事件通道）。
    ///
    /// **热路径**：`emit` 会被流式正文逐 token 调用，所以门槛是
    /// `has_event_subscribers()`——**一次原子 load**，无人订阅就立刻返回。
    /// 当前所有部署都属这一类（没有任何内置插件声明 `subscribed_events`），
    /// 因此这次接线不增加可测开销；有人订阅才继续走名字判定与序列化
    /// （三层保护见 `PluginHost::deliver_backend_event`）。
    ///
    /// 为什么 `emit` 与 `emit_handle` **都**调它：两条都是"事件出口"
    /// （后者给联邦命令处理器这类跨 await 的调用方）。只在一条上投递，会让
    /// "插件能看到哪些事件"取决于 Core 内部走了哪条路——那种不一致没法从
    /// 插件侧解释。
    fn deliver_event_to_plugins(&self, event: &BackendEvent) {
        let host = self.plugin_host();
        if host.has_event_subscribers() {
            host.deliver_backend_event(event);
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
                instance_name,
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
                instance_name,
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
            // 落 durable 事件（2026-10 根治）：推理进事件日志后，显示历史
            // 派生才与实时投影同形状（推理与工具调用真实交错），前端不再
            // 需要为推理走"只存在于实时窗口"的特殊路径。
            //
            // 模型不可见：`project_messages` 跳过该变体（见 derive.rs）。
            // 时间在此定格——派生条目与实时 timeline 条目据此对齐。
            //
            // 落盘上限见 [`MAX_REASONING_EVENT_CHARS`]：单步推理异常长时截断，
            // 否则事件日志（整份 JSON、30s 一次整体重写）会被少数几步撑爆。
            self.trunk
                .append_event(echo_session::SessionEvent::Reasoning(
                    echo_session::event::ReasoningEvent {
                        content: cap_reasoning_for_log(content),
                        timestamp: chrono::Utc::now().timestamp(),
                        session: Some(session_id.to_string()),
                    },
                ));
            self.emit(BackendEvent::AgentReasoning {
                session_id: session_id.to_string(),
                team_id: None, // 由 annotate_team 统一注入
                branch_id: branch_id.to_string(),
                content: content.to_string(),
            });
        }
    }

    pub async fn active_model(&self) -> String {
        let model = self.model.active_model.read().await.clone();
        if model.is_empty() {
            self.model.provider.read().await.default_model().to_string()
        } else {
            model
        }
    }

    /// 会话级 provider 解析（2026-10 会话粒度模型）：有覆盖 → 专用
    /// provider；无覆盖 → persona 级（`self.model.provider`）。
    ///
    /// 覆盖的 provider 在首次使用时按 profile 惰性构建（含计量 + 脱敏
    /// 装饰器）；构建失败时回退 persona 级（记日志，不中断 turn）。
    ///
    /// 请求模型必须经 [`Self::session_model`] **成对**解析——覆盖 provider
    /// 是其 profile 构建的（模型可能不同），两者不一致会打到覆盖端点报
    /// model_not_found（2026-10 用户报障）。
    pub async fn resolve_session_provider(&self, session_id: &str) -> Arc<dyn LlmProvider> {
        // 快路径：已缓存的覆盖直接命中。
        if let Some(entry) = self.model.session_providers.get(session_id) {
            return Arc::clone(&entry.1);
        }
        // 慢路径：无覆盖 → 跟随 persona 级。
        Arc::clone(&*self.model.provider.read().await)
    }

    /// 会话级模型解析（与 [`Self::resolve_session_provider`] **成对**使用）：
    /// 有覆盖 → 覆盖 provider 的 profile 模型；无覆盖 → persona 级
    /// `active_model`。
    ///
    /// 请求模型与 provider 必须同源——覆盖 provider 是其 profile 构建的，
    /// 模型可能不同；若请求仍用 persona active_model，覆盖端点会按该模型
    /// 报错（2026-10 用户报障：uuapi 返回 404 model_not_found
    /// "deepseek-v4-flash is not supported"，而该会话的 profile 模型是
    /// gemini-4-argon）。
    pub async fn session_model(&self, session_id: &str) -> String {
        if let Some(entry) = self.model.session_providers.get(session_id) {
            return entry.1.default_model().to_string();
        }
        self.active_model().await
    }

    /// 设置/清除会话级 provider 覆盖（管理命令入口）。
    ///
    /// `profile_name` 空串 = 清除覆盖（回到跟随 persona 级）；非空 =
    /// 按全局供应商池构建该 profile 的 provider 并缓存。构建失败时
    /// 返回 false（旧覆盖/无覆盖保持）。
    pub async fn set_session_provider(&self, session_id: &str, profile_name: &str) -> bool {
        if profile_name.is_empty() {
            self.model.session_providers.remove(session_id);
            self.model.session_provider_locks.remove(session_id);
            return true;
        }
        // 构建锁：同一会话的并发 set 串行（防重复构建）。
        let lock = self
            .model
            .session_provider_locks
            .entry(session_id.to_string())
            .or_insert_with(|| tokio::sync::Mutex::new(()));
        let _guard = lock.lock().await;
        // 双重检查：可能已被并发调用写入。
        if let Some(entry) = self.model.session_providers.get(session_id) {
            if entry.0 == profile_name {
                return true; // 已是目标 profile
            }
        }
        // 供应商池以共享 ConfigStore（core.toml）为首选事实源：本 agent
        // （persona）持有的配置克隆可能落后于管理面——管理面新增的
        // profile 在 persona 快照里不存在，直接读本快照会误报
        // "profile not found"（2026-10 用户报障）。读不到时回退本快照。
        let mut config = self.model.config.read().await.clone();
        config.api_profiles = self.api_pool_snapshot().await;
        let probe = match crate::agent::api_admin::resolve_probe_config(&config, profile_name) {
            Ok(p) => p,
            Err(message) => {
                self.emit(BackendEvent::Error {
                    session_id: Some(session_id.to_string()),
                    message: format!("session profile resolve failed: {message}"),
                });
                return false;
            }
        };
        // 计量归属：profile 名（与全局/persona 级同口径）。
        let provider = match crate::llm::create_provider(&probe) {
            Ok(p) => Arc::from(p),
            Err(e) => {
                self.emit(BackendEvent::Error {
                    session_id: Some(session_id.to_string()),
                    message: format!("session profile provider build failed: {e}"),
                });
                return false;
            }
        };
        let provider = match self.metrics() {
            Some(store) => crate::llm::wrap_metering(provider, store, profile_name.to_string()),
            None => provider,
        };
        let provider = match self.redactor() {
            Some(redactor) => crate::llm::wrap_redacting(provider, redactor),
            None => provider,
        };
        // 安全：登记该 profile 的密钥（幂等）。
        self.register_config_secrets(&probe);
        self.model
            .session_providers
            .insert(session_id.to_string(), (profile_name.to_string(), provider));
        true
    }

    /// API profile 改名传播（2026-10）：把会话级覆盖表里引用 `from` 的条目
    /// 重绑到 `to`——经 [`Self::set_session_provider`] 重建 provider（计量
    /// 归属名在构建时烘焙，必须重建而非改字符串）。返回成功重绑的会话数；
    /// 构建失败的条目直接移除（回落跟随 persona 级，避免悬空旧名）。
    pub(crate) async fn rebind_session_providers(&self, from: &str, to: &str) -> usize {
        let sids: Vec<String> = self
            .model
            .session_providers
            .iter()
            .filter(|entry| entry.value().0 == from)
            .map(|entry| entry.key().clone())
            .collect();
        let mut rebound = 0usize;
        for sid in sids {
            if self.set_session_provider(&sid, to).await {
                rebound += 1;
                tracing::info!(session = %sid, %from, %to, "session API override rebound after rename");
            } else {
                self.model.session_providers.remove(&sid);
                self.model.session_provider_locks.remove(&sid);
                tracing::warn!(session = %sid, %from, %to, "session API override dropped after rename (rebuild failed)");
            }
        }
        rebound
    }

    pub async fn active_provider(&self) -> String {
        self.model.provider.read().await.name().to_string()
    }

    pub async fn set_model(&self, model: String) {
        *self.model.active_model.write().await = model;
    }

    /// 同步版 [`Self::set_model`]：供组合根启动期（make_agent 为同步闭包）调用。
    pub fn set_model_now(&self, model: String) {
        if let Ok(mut slot) = self.model.active_model.try_write() {
            *slot = model;
        }
    }

    /// 工作区会话（workspace 插件）注入提示词的文本：插件对该 persona 启用
    /// 且存在激活会话时返回 Some（名称 + 工作目录清单），否则 None。
    /// 构建系统提示词块（无输入相关区块的代表性构建，供 `/context` 在无
    /// 最近 turn 时回退使用）。
    async fn build_prompt_blocks(&self) -> Vec<PromptBlock> {
        let base = self.model.config.read().await.system_prompt.clone();
        // 没有指定会话 → 上下文为空。插件负责决定"空上下文下有没有内容"
        // （如工作区插件在无 active 会话时不贡献块）。
        let plugin_blocks = self.collect_plugin_text_blocks("");
        let skills = self.skills.lock().await;
        crate::agent::prompt::build_prompt_blocks(&skills, &base, "", None, &[], &plugin_blocks)
            .await
    }

    /// 收集插件贡献的文本块，上下文带**本次 turn 的会话 id 与人格 id**。
    ///
    /// 为什么这两个键：`Contribution::Text` 的 `text_block_content` 会拿到它们
    /// （框架只透传，不解释）。`session_id` 让插件按会话给不同文本——这正是
    /// 2026-10 多窗口问题的修法（全局 active 是 per-persona 单槽，另一个窗口
    /// 激活工作区后会被注入到这里）；`team_id` 让进程级插件服务多个人格时能
    /// 找到"当前这个 agent 的那一份状态"。
    pub(crate) fn collect_plugin_text_blocks(
        &self,
        session_id: &str,
    ) -> Vec<(echo_plugin::PluginTextBlock, String)> {
        // **persona 白名单仍然生效**：插件挂载是进程级的，但"这个人格能不能
        // 看到它的内容"是 per-persona 的（`enabled_plugins`）。过滤钩子在这里，
        // 判断逻辑在 `profile_allows_plugin`——框架只提供"按 id 过滤"。
        let capabilities = self.capabilities.lock().ok().and_then(|cap| cap.clone());
        self.plugin_host().collect_text_blocks(
            &serde_json::json!({
                "session_id": session_id,
                "team_id": self.team_id().unwrap_or_default(),
            }),
            |plugin_id| match &capabilities {
                Some(cap) => crate::plugins::profile_allows_plugin(cap, plugin_id),
                // 没注入能力表 = 不启用任何人格级限制（单测/裸装配）。
                None => true,
            },
        )
    }
    /// Decomposed context blocks for `/context`: the prompt sections of the
    /// most recent turn (falling back to a boundary-free build) plus the
    /// conversation history aggregated per role. The trunk history holds user
    /// / assistant / tool messages only — the system prompt is rebuilt every
    /// turn and is represented by the prompt blocks above.
    pub async fn context_blocks(
        &self,
        session_id: Option<&str>,
        history: &[ChatMessage],
    ) -> Vec<crate::event::ContextBlockInfo> {
        let prompt = match session_id.and_then(|id| self.last_prompt_blocks.get(id)) {
            Some(blocks) => blocks.clone(),
            // No turn has run yet for this session in this process — build a
            // representative set without input-specific sections.
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
        let base = self.model.config.read().await.system_prompt.clone();
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

    /// Emit the current instance list to subscribers.
    fn emit_instance_list(&self) {
        let instances: Vec<InstanceStatus> = self
            .instances
            .list_info()
            .into_iter()
            .map(|info| {
                // 实例句柄：management / ui_descriptor / session_labels 三个
                // trait 访问器同源（贡献在注册时静态）。
                let handle = self.instances.get(&info.name);
                // 实例附加信息（persona/容器/URL/绑定地址）经管理面
                // `cached_state` 取（W3.4 单源化——与 InstanceState 同一状态
                // 文档；缺管理面 / 无状态时留空，wire 字段本就 Option）。
                let instance = handle
                    .as_ref()
                    .and_then(|a| a.management().and_then(|m| m.cached_state()))
                    .map(|state| state["instance"].clone())
                    .unwrap_or_default();
                let bind_address = instance["bind_address"].as_str().unwrap_or("").to_string();
                let persona = instance["persona"].as_str().map(str::to_string);
                // UI 描述符 / 会话标签（01a S2 访问器；InstanceContribution
                // 原样透传。None = 未声明，线上字段本就 Option）。
                let ui = handle.as_ref().and_then(|a| a.ui_descriptor());
                let session_labels = handle.as_ref().and_then(|a| a.session_labels());
                // **平台自有展示字段由插件自己描述**（UI 描述符里的字段绑定，
                // Panel 的通用渲染器按 `bind` 取 `instance.*`）。Core 不再逐键
                // 命名 `container` / `webui_url` / `onebot_url` 并配中文标签
                // ——那是让框架持有某个平台的展示 schema（2026-10 去适配器化）。
                let metadata: Vec<InstanceMetadataEntry> = Vec::new();
                InstanceStatus {
                    name: info.name,
                    display_name: info.display_name,
                    connected: matches!(
                        info.status,
                        echo_defs::plugin_instance::InstanceConnectionState::Connected
                    ),
                    running: !matches!(
                        info.status,
                        echo_defs::plugin_instance::InstanceConnectionState::Stopped
                    ),
                    self_id: info.self_id,
                    bind_address,
                    started_at: info.started_at,
                    persona,
                    metadata,
                    platform: Some(info.platform),
                    ui,
                    session_labels,
                }
            })
            .collect();
        self.emit(BackendEvent::InstanceList { instances });
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
/// [`crate::instance_bridge::format_hook_input`]); this pulls them into the
/// durable `UserMessage` event so session replay keeps the multimodal
/// content. Returns an empty vec for plain text.
/// 单张图片（URL 或 data URI 字符串）进入持久化/模型请求的上限。
///
/// 上限对齐各入口的内嵌上限：QQ 插件实例按解码后 ≤10MB 内嵌
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod reasoning_cap_tests {
    use super::{cap_reasoning_for_log, MAX_REASONING_EVENT_CHARS};

    #[test]
    fn short_reasoning_is_stored_verbatim() {
        let text = "简短思考";
        assert_eq!(cap_reasoning_for_log(text), text);
    }

    #[test]
    fn long_reasoning_is_capped_with_marker() {
        let text = "字".repeat(MAX_REASONING_EVENT_CHARS + 50);
        let capped = cap_reasoning_for_log(&text);
        assert!(capped.starts_with(&"字".repeat(MAX_REASONING_EVENT_CHARS)));
        assert!(capped.contains("推理过长已截断"));
        assert!(capped.contains(&format!("原文 {}", MAX_REASONING_EVENT_CHARS + 50)));
        // 截断后长度有界（标记本身十几字）
        assert!(capped.chars().count() < MAX_REASONING_EVENT_CHARS + 40);
    }

    #[test]
    fn exactly_at_limit_is_stored_verbatim() {
        let text = "字".repeat(MAX_REASONING_EVENT_CHARS);
        assert_eq!(cap_reasoning_for_log(&text), text);
    }
}

#[cfg(test)]
pub mod tests;
