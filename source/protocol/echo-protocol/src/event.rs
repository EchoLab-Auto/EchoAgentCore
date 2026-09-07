//! Events emitted by the agent backend to subscribers (TUI, API, ...).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 技能来源信息（外部 Git 仓库安装）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSourceInfo {
    pub url: String,
    pub rev: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub installed_at: Option<String>,
}

/// Adapter status reported to the TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterStatus {
    pub name: String,
    pub display_name: String,
    pub connected: bool,
    pub running: bool,
    pub self_id: Option<String>,
    pub bind_address: String,
    pub started_at: Option<i64>,
}

/// 后台 shell 会话摘要（前端可视化终端）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellSessionInfo {
    pub session_id: String,
    /// 启动时的工作目录。
    pub workdir: String,
    /// 创建时间（Unix 毫秒）。
    pub created_at_ms: i64,
    /// 最后活动时间（Unix 毫秒）。
    pub last_active_ms: i64,
    /// 已执行命令数。
    pub exec_count: u64,
    /// 会话是否处于运行态。
    pub running: bool,
    /// 最近一次执行结果（截断到 4096 字符，供列表预览）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output: Option<String>,
}

/// Snapshot of a conversation session, for the TUI session list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    /// Owning team id (None = default/legacy single agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// Platform identifier (e.g., "qq", "local").
    pub platform: String,
    /// Scope within the platform ("dm", "group", "tui").
    pub scope: String,
    /// Platform user ID as a string.
    pub user_id: String,
    pub nickname: String,
    /// Group name when the session is group-scoped, None for direct chat.
    pub group_name: Option<String>,
    pub last_active: i64,
    pub last_message: String,
}

/// One message inside the trunk context snapshot (`/context`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextMessageInfo {
    /// "user" | "assistant" | "tool" | "system".
    pub role: String,
    /// Full message content.
    pub content: String,
    /// Estimated token count of this message.
    pub tokens: usize,
    /// Structured message_sequence embedded in the content, if any.
    pub sequence: Option<u64>,
}

/// One visual block of the trunk context (`/context`).
///
/// The system prompt is decomposed into named blocks (base prompt, skill
/// metadata, per-skill injections, orchestration, per-input boundary rules)
/// so the panel can visualize where tokens are spent; conversation history is
/// aggregated per role.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBlockInfo {
    /// Stable identifier, e.g. "base", "skills", "skill:alix-persona",
    /// "triggered:web-search", "orchestration", "boundary:qq_hook",
    /// "history:user".
    pub key: String,
    /// Human-readable label for the block list.
    pub label: String,
    /// Category: "base" | "skills" | "skill" | "triggered" | "orchestration"
    /// | "boundary" | "history".
    pub kind: String,
    /// Estimated token count of this block.
    pub tokens: usize,
    /// Character count of this block.
    pub chars: usize,
    /// Full block text (prompt sections) or a per-message summary (history).
    pub content: String,
}

/// Source provenance of a timeline user message (mirrors `MessageReceived`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineSource {
    pub adapter_name: String,
    pub platform: String,
    pub user_id: String,
    pub user_name: String,
    /// Channel descriptor: "direct" or "group:{group_id}".
    pub channel: String,
    pub group_name: Option<String>,
    pub received_at_ms: i64,
    pub message_sequence: u64,
}

/// Tool-call details of a timeline tool message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineTool {
    pub name: String,
    pub input: String,
    pub output: Option<String>,
    pub failed: bool,
    /// Provider-issued tool-call id; empty = legacy entry.
    /// 前端据此精确配对/就地修补（同名并行调用不再配错对）。
    #[serde(default)]
    pub tool_call_id: String,
    /// 执行被外圈超时守卫中止。
    #[serde(default)]
    pub timed_out: bool,
}

/// One persisted trunk timeline entry. The timeline is a *display* history
/// (up to `TRUNK_TIMELINE_MAX` entries) that survives TUI restarts; it is
/// independent of the LLM-facing trunk context, which is token-bounded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineMessage {
    /// "user" | "backend" | "tool" | "system".
    pub kind: String,
    pub content: String,
    /// Owning session key (e.g. "local:tui::local_user", "qq:dm::123",
    /// "qq:group:456:123"). Empty = global/unknown (legacy entries).
    #[serde(default)]
    pub session_id: String,
    /// Unix seconds.
    pub time: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<TimelineSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<TimelineTool>,
    /// Multimodal media attached to the message (URLs / data URIs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    /// 条目级单调序号（push 与就地更新都会推进）；0 = 旧版/重启前条目。
    /// 增量同步（since_seq）按此过滤，就地更新的工具条目会被重新投递。
    #[serde(default)]
    pub seq: u64,
}

impl TimelineMessage {
    /// A user message with provenance and optional images.
    pub fn user(
        content: impl Into<String>,
        session_id: impl Into<String>,
        time: i64,
        source: Option<TimelineSource>,
        images: Vec<String>,
    ) -> Self {
        Self {
            kind: "user".into(),
            content: content.into(),
            session_id: session_id.into(),
            time,
            source,
            reasoning: None,
            tool: None,
            images: (!images.is_empty()).then_some(images),
            seq: 0,
        }
    }

    /// An assistant/backend output.
    pub fn backend(
        content: impl Into<String>,
        session_id: impl Into<String>,
        time: i64,
        reasoning: Option<Vec<String>>,
    ) -> Self {
        Self {
            kind: "backend".into(),
            content: content.into(),
            session_id: session_id.into(),
            time,
            source: None,
            reasoning,
            tool: None,
            images: None,
            seq: 0,
        }
    }

    /// A tool-call/result entry.
    pub fn tool(
        tool_name: impl Into<String>,
        session_id: impl Into<String>,
        time: i64,
        tool: TimelineTool,
    ) -> Self {
        Self {
            kind: "tool".into(),
            content: tool_name.into(),
            session_id: session_id.into(),
            time,
            source: None,
            reasoning: None,
            tool: Some(tool),
            images: None,
            seq: 0,
        }
    }

    /// A system notice (e.g. timer summary).
    pub fn system(content: impl Into<String>, session_id: impl Into<String>, time: i64) -> Self {
        Self {
            kind: "system".into(),
            content: content.into(),
            session_id: session_id.into(),
            time,
            source: None,
            reasoning: None,
            tool: None,
            images: None,
            seq: 0,
        }
    }
}

/// Events flowing from the backend to the TUI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum BackendEvent {
    // ---- Adapter lifecycle ----
    /// An adapter's connection state changed.
    AdapterStateChanged {
        adapter_name: String,
        connected: bool,
        self_id: Option<String>,
    },

    // ---- Incoming messages ----
    /// A message was received from any platform.
    MessageReceived {
        session_id: String,
        adapter_name: String,
        platform: String,
        user_id: String,
        user_name: String,
        /// Channel descriptor: "direct" or "group:{group_id}"
        channel: String,
        group_name: Option<String>,
        content: String,
        /// Multimodal media attached to the message (URLs / data URIs).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
        /// Source/platform timestamp in Unix seconds.
        timestamp: i64,
        /// Core receive timestamp in Unix milliseconds.
        received_at_ms: i64,
        /// Monotonic sequence assigned by this Core process.
        message_sequence: u64,
        /// Owning team id (empty/None = default). Lets the panel filter
        /// realtime events to the active team.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
    },

    // ---- Agent processing (platform-agnostic) ----
    /// Agent began processing (LLM call pending).
    AgentThinking {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
    },
    /// LLM request was sent.
    LlmRequest {
        session_id: String,
        model: String,
    },
    /// LLM response completed.
    LlmResponse {
        session_id: String,
        model: String,
        prompt_tokens: u32,
        completion_tokens: u32,
    },
    /// Model reasoning content returned before the visible answer.
    AgentReasoning {
        session_id: String,
        /// 归属 team（多 persona 场景由 annotate_team 注入，前端据此过滤
        /// 实时事件，避免跨 agent 串显）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
        /// Reply branch that owns this reasoning fragment.
        branch_id: String,
        content: String,
    },
    /// An isolated subagent started processing a task for the parent agent.
    SubagentStarted {
        session_id: String,
        task: String,
    },
    /// An isolated subagent finished and returned control to the parent agent.
    SubagentCompleted {
        session_id: String,
        success: bool,
    },
    /// A QQ message was routed through an isolated temporary reply branch.
    ReplyBranchStarted {
        session_id: String,
        branch_id: String,
        message_sequence: u64,
        task: String,
        target: String,
        started_at_ms: i64,
    },
    /// Content generated for a temporary QQ wait reply. It stays in the
    /// branch inspector and is never appended to the main chat transcript.
    ReplyBranchContent {
        session_id: String,
        branch_id: String,
        content: String,
    },
    /// A temporary reply branch finished or was cancelled.
    ReplyBranchCompleted {
        session_id: String,
        branch_id: String,
        message_sequence: u64,
        success: bool,
        cancelled: bool,
        completed_at_ms: i64,
    },
    /// A detached background task was accepted. It does not keep the parent
    /// conversation in a busy state.
    BackgroundTaskStarted {
        session_id: String,
        task_id: String,
        sequence: u64,
        branch_count: usize,
        objective: String,
        created_at_ms: i64,
    },
    /// All branches finished. Integration may still wait for an earlier task.
    BackgroundTaskCompleted {
        session_id: String,
        task_id: String,
        sequence: u64,
        success: bool,
        completed_at_ms: i64,
    },
    /// A completed task was committed to the shared context and its delivery
    /// plan was processed by the parent agent.
    BackgroundTaskIntegrated {
        session_id: String,
        task_id: String,
        sequence: u64,
        success: bool,
        integrated_at_ms: i64,
    },
    /// Agent invoked a tool.
    ToolCall {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
        tool_name: String,
        arguments: String,
        /// Provider-issued tool-call id; empty = legacy peer (pre-id protocol).
        /// 前端据此精确配对 ToolResult（同名并行调用不再配错对）。
        #[serde(default)]
        tool_call_id: String,
        /// Owning reply branch (empty for pre-branch legacy paths).
        #[serde(default)]
        branch_id: String,
    },
    /// Tool execution completed.
    ToolResult {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
        tool_name: String,
        result: String,
        /// Provider-issued tool-call id; empty = legacy peer.
        #[serde(default)]
        tool_call_id: String,
        /// 执行被外圈超时守卫中止（notice 语义：不算 error，但也不算成功）。
        /// 前端据此把该工具渲染为失败而非成功。
        #[serde(default)]
        timed_out: bool,
        /// Owning reply branch (empty for pre-branch legacy paths).
        #[serde(default)]
        branch_id: String,
    },
    /// A tool published a structured state snapshot for TUI visualization
    /// (e.g. the checklist tool → checklist panel). `state` is tool-specific;
    /// the checklist tool emits `{"lists": [{name, done, total, items}]}`.
    ChecklistUpdated {
        session_id: String,
        state: serde_json::Value,
    },
    /// Agent generated backend output. This is never sent to a platform
    /// automatically; platform delivery requires an explicit tool call.
    /// `branch_id` links the output to the temporary snapshot branch that
    /// produced it (QQ reply branches, backend/timer turns). It is `None`
    /// for detached emissions such as `send_backend_message` deliveries.
    AgentOutput {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
        content: String,
        #[serde(default)]
        branch_id: Option<String>,
    },
    /// The agent turn finished, including turns whose backend output is empty.
    AgentCompleted {
        session_id: String,
    },
    /// A session was created or updated.
    SessionUpdated {
        session: SessionInfo,
    },
    /// The current trunk context snapshot (`/context` command response).
    ContextSnapshot {
        /// Every message currently in the trunk (oldest first).
        messages: Vec<ContextMessageInfo>,
        /// Decomposed context blocks for visualization (system prompt
        /// sections + per-role history aggregates).
        #[serde(default)]
        blocks: Vec<ContextBlockInfo>,
        /// Estimated total tokens of the trunk.
        total_tokens: usize,
        /// Effective token budget for the trunk.
        limit_tokens: usize,
    },
    /// 后台 shell 会话列表（响应 RequestShellSessions）。
    ShellSessionsList {
        sessions: Vec<crate::event::ShellSessionInfo>,
    },
    /// 新建 shell 会话成功。
    ShellSessionStarted {
        session: crate::event::ShellSessionInfo,
    },
    /// 一次命令执行开始（面板据此显示运行态/回显命令）。
    ShellExecStarted {
        session_id: String,
        seq: u64,
        command: String,
    },
    /// 命令输出的增量块（stdout/stderr 合并，按到达顺序分块）。
    ShellExecOutput {
        session_id: String,
        seq: u64,
        chunk: String,
    },
    /// 一次命令执行结束。
    ShellExecDone {
        session_id: String,
        seq: u64,
        success: bool,
        elapsed_ms: u64,
    },
    /// shell 会话被销毁（停止/空闲回收/进程退出）。
    ShellSessionClosed {
        session_id: String,
        reason: String,
    },
    /// The persisted display timeline (response to `RequestTrunkTimeline`).
    /// Sent on TUI startup so historical messages survive restarts.
    TrunkTimeline {
        messages: Vec<TimelineMessage>,
        /// 该 agent timeline 的最新单调序号（每次新增条目递增）。
        /// 前端据此缓存与增量请求（since_seq），避免每次切换全量传输。
        #[serde(default)]
        seq: u64,
        /// 响应归属：= 请求的 team_id（None = 主/默认 agent）。
        /// 前端必须按此路由缓存——绝不能拿当前 activeTeamId 猜测，
        /// 否则其他 agent 的兜底请求会污染当前视图（串显 alix 会话）。
        #[serde(default)]
        team_id: Option<String>,
        /// true = 完整快照（替换缓存）；false = 相对 since_seq 的增量
        /// （追加/就地修补）。前端不再凭 seq 大小猜测，避免把全量快照
        /// 误当增量追加导致整段历史重复。
        #[serde(default)]
        full: bool,
    },
    /// API configuration changed (for TUI settings form).
    ApiConfigUpdated {
        provider: String,
        model: String,
        base_url: String,
        api_key_set: bool,
        #[serde(default)]
        thinking: crate::mode::ThinkingMode,
        #[serde(default)]
        reasoning_effort: crate::mode::ReasoningEffort,
        system_prompt: String,
        /// Currently active profile name (empty = top-level default).
        active_api: String,
        /// All saved API profiles.
        profiles: Vec<ApiProfileInfo>,
    },
    /// API profile list changed (emitted together with ApiConfigUpdated).
    ApiProfilesUpdated {
        active_api: String,
        profiles: Vec<ApiProfileInfo>,
    },
    /// Full list of discovered skills (response to `RequestSkillsList`).
    SkillsList {
        skills: Vec<SkillInfo>,
    },
    /// Full list of registered tools (response to `RequestToolsList`).
    ToolsList {
        tools: Vec<ToolInfo>,
    },
    /// Full list of mounted plugins (response to `RequestPluginsList`).
    PluginsList {
        plugins: Vec<PluginInfo>,
    },
    /// Full list of team members (response to `RequestTeamsList`).
    TeamsList {
        teams: Vec<TeamInfo>,
    },
    /// API connectivity test result (response to `TestApi`).
    ApiTestResult {
        /// Tested config name (empty = top-level default).
        name: String,
        ok: bool,
        /// Human-readable message (error detail or success note).
        message: String,
        /// Round-trip latency of the probe request, ms.
        latency_ms: u64,
    },
    /// Current system prompt plugin text.
    SystemPrompt {
        text: String,
    },
    /// Backend error (also used for info toasts).
    Error {
        session_id: Option<String>,
        message: String,
    },

    // ---- Adapter management ----
    AdapterList {
        adapters: Vec<AdapterStatus>,
    },
    /// QQ group list.
    GroupList {
        groups: Vec<GroupInfo>,
    },
    /// Current QQ filter configuration (allowlist/denylist).
    QqFilterConfig {
        allowlist_users: Vec<i64>,
        allowlist_groups: Vec<i64>,
        denylist_users: Vec<i64>,
        denylist_groups: Vec<i64>,
    },
    /// QQ gate mode changed.
    QqGateMode {
        mode: crate::mode::GateMode,
    },
    /// Current QQ owner (admin) QQ number. Frontend-only; never emitted to
    /// the agent/LLM context.
    QqOwner {
        owner_qq: i64,
    },
    /// QQ friend list (for interactive allowlist/denylist picker).
    FriendList {
        friends: Vec<FriendInfo>,
    },

    // ---- Sudo authorization (human-in-the-loop) ----
    /// The agent requests root privileges for `command`. The Panel must show
    /// the user a masked password prompt and answer with
    /// [`SudoPasswordSubmit`](crate::bridge::SudoPasswordSubmit) on the
    /// dedicated sudo channel (never through the agent command queue). The
    /// password itself never appears in any event.
    SudoRequest {
        request_id: u64,
        command: String,
        session_id: String,
    },
    /// A sudo authorization request was resolved (password submitted,
    /// denied, or the tool timed out). `message` is a short human-readable
    /// outcome for the Panel toast; it never contains the password.
    SudoResolved {
        request_id: u64,
        accepted: bool,
        message: String,
    },
}

/// One discovered skill with its full instructions (for panel display).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub always: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// UI grouping category (from SKILL.md metadata; empty = "未分类").
    #[serde(default)]
    pub category: String,
    /// Owning package id (from SKILL.md frontmatter `package:`); None = standalone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Full instructions body of `SKILL.md`.
    #[serde(default)]
    pub content: String,
    /// 系统提示词 skill（system: true）：内容注入 base 区。
    #[serde(default)]
    pub system: bool,
    /// 外部 Git 来源（安装的 skill；内置技能为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SkillSourceInfo>,
}

fn default_true() -> bool {
    true
}

/// One registered tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub parameters: serde_json::Value,
    /// UI grouping category.
    #[serde(default)]
    pub category: String,
    /// Runtime enable/disable state.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Owning package id (e.g. "echo-agent.adapter.qq"); None = standalone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
}

/// 编排模式（per-persona，互斥）：single = 单任务编排（无会话管理 UI、
/// 回执分支不可见）；chatbot = 多任务并行编排（会话列表/全局会话/可见回执分支）。
/// 由 persona 的 enabled_plugins/disabled_plugins 白名单语义对
/// echo-agent.orchestration.{single,chatbot} 两个互斥子插件推导（single 为兜底）。
/// serde default = Chatbot：对齐旧三布尔（reply_branches_enabled 等）的
/// default_true 语义——旧 core 消息缺字段时按全功能模式处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrchestrationMode {
    Single,
    #[default]
    Chatbot,
}

/// One team member (frontend display).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamInfo {
    pub id: String,
    pub name: String,
    /// Whether this agent is the primary/default agent (protected from deletion).
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 该 agent 系统提示词由哪些 skill 组成（空 = 回退 system_prompt 字段）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_skills: Vec<String>,
    /// Number of active sessions owned by this agent.
    #[serde(default)]
    pub sessions: usize,
    /// 该 agent 的编排模式（互斥子插件推导，single 为兜底）。
    /// single：侧边栏不显示「会话」「临时分支」卡片与「全局」分组，
    /// 后端不发射 ReplyBranch* 可见性事件（分支照常执行合并，仅不可见）；
    /// chatbot：全部可见。取代旧 reply_branches_enabled /
    /// global_session_enabled / chat_sessions_enabled 三布尔（2026-09 协议变更）。
    #[serde(default)]
    pub orchestration_mode: OrchestrationMode,
    /// Persona system prompt (empty = inherits global prompt/skills).
    #[serde(default)]
    pub system_prompt: String,
    /// Per-persona disabled capability ids (plugins/tools/skills).
    #[serde(default)]
    pub disabled_plugins: Vec<String>,
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    #[serde(default)]
    pub disabled_skills: Vec<String>,
    /// Per-persona allowlists (empty = everything enabled).
    #[serde(default)]
    pub enabled_plugins: Vec<String>,
    #[serde(default)]
    pub enabled_tools: Vec<String>,
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    /// Per-agent trunk token budget (None = 继承全局 [agent].memory_limit_tokens)。
    #[serde(default)]
    pub memory_limit_tokens: Option<usize>,
    /// Per-agent context window cap (None = 继承全局 [agent].context_window_tokens)。
    #[serde(default)]
    pub context_window_tokens: Option<usize>,
}

/// One mounted plugin (frontend display).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub entry: String,
    #[serde(default)]
    pub author: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Whether the plugin ships inside the binary.
    #[serde(default = "default_true")]
    pub builtin: bool,
}

/// QQ group info for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupInfo {
    pub group_id: i64,
    pub group_name: String,
}

/// QQ friend info for allowlist/denylist picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FriendInfo {
    pub user_id: i64,
    pub nickname: String,
}

/// API profile summary (for TUI list display).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiProfileInfo {
    pub name: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub base_url: String,
    pub api_key_set: bool,
    #[serde(default)]
    pub thinking: crate::mode::ThinkingMode,
    #[serde(default)]
    pub reasoning_effort: crate::mode::ReasoningEffort,
}

/// Full state snapshot sent to the TUI on connect.
#[derive(Debug, Clone, Default)]
pub struct BackendState {
    pub adapters: HashMap<String, AdapterStatus>,
    pub active_model: String,
    pub active_provider: String,
    pub system_prompt: String,
    pub enabled_skills: Vec<String>,
    pub sessions: Vec<SessionInfo>,
}

/// `BackendEvent` is dispatchable on the harness event bus: the enum derives
/// Clone + Debug + Send + Sync, this impl declares the membership so
/// listeners can subscribe by event type (dsh typed events).
impl echo_context::Event for BackendEvent {}
