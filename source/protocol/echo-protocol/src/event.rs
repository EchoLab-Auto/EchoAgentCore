//! Events emitted by the agent backend to subscribers (TUI, API, ...).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::mode::LoopMode;

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
    /// QQ 实例（2026-09 多实例）：归属人格 id。
    #[serde(default)]
    pub persona: Option<String>,
    /// QQ 实例：NapCat 容器名（Panel 展示 / 诊断用）。
    #[serde(default)]
    pub container: Option<String>,
    /// QQ 实例：NapCat WebUI 地址（扫码登录页面链接）。
    #[serde(default)]
    pub webui_url: Option<String>,
    /// QQ 实例：OneBot HTTP API 地址。
    #[serde(default)]
    pub onebot_url: Option<String>,
}

/// 后台 shell 会话摘要（前端可视化终端）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellSessionInfo {
    pub session_id: String,
    /// 归属 team（persona）。面板按当前 agent 过滤，不串显其他 agent 的 shell。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
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
    /// "triggered:web-search", "boundary:qq_hook",
    /// "history:user".
    pub key: String,
    /// Human-readable label for the block list.
    pub label: String,
    /// Category: "base" | "skills" | "skill" | "triggered"
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
    /// A completed task was committed to the shared context and the parent
    /// agent was asked to deliver to its declared targets.
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
    /// 活跃 turn 快照（刷新恢复用，2026-10）：Core 在响应
    /// `RequestTrunkTimeline` 之后补发——运行状态（thinking/tool/subagent）
    /// 本是瞬时事件流，前端刷新重连后只能拿到历史快照，若无本事件，
    /// 正在运行的会话的活动浮条/取消按钮会丢失直到下一个瞬时事件。
    /// 前端处理：清空本地全部 activity，按本列表重建 running 集合。
    ActiveTurnsSnapshot {
        /// 当前正在执行 turn 的会话 id 列表。
        session_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
    },

    // ── 联邦管理（federation Phase 4）──
    /// 联邦状态快照（`RequestFederationStatus` 响应；peer 增删/链路
    /// Up/Down 时主动刷新广播）。
    FederationStatus {
        enabled: bool,
        #[serde(default)]
        node_id: String,
        #[serde(default)]
        node_name: Option<String>,
        #[serde(default)]
        listen: String,
        #[serde(default)]
        peers: Vec<FederationPeerInfo>,
    },
    /// 本机邀请串（`RequestFederationInvite` 响应）。
    FederationInvite {
        invite: String,
    },
    /// The current conversation context snapshot (`RequestContext` response).
    ContextSnapshot {
        /// 快照归属的会话 id（多会话，2026-09；None = 旧 Core 的全局口径）。
        #[serde(default)]
        session_id: Option<String>,
        /// Every message currently in the context (oldest first).
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
        /// 列表归属 team（与请求一致；旧 core 无此字段时前端不过滤）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
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
    /// API account balance (response to `QueryApiBalance`；目前仅 DeepSeek
    /// 官方端点支持 `/user/balance`）。
    ApiBalanceResult {
        /// Queried config name (empty = top-level default).
        name: String,
        ok: bool,
        /// 账户是否可用（is_available）。
        #[serde(default)]
        available: bool,
        /// 总余额（含赠送），如 "110.00"；失败时为空串。
        #[serde(default)]
        total: String,
        /// 币种，如 "CNY"；失败时为空串。
        #[serde(default)]
        currency: String,
        /// Human-readable message (error detail or success note).
        #[serde(default)]
        message: String,
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
    /// QQ group list. `adapter` = 实例名（多实例寻址；None = 旧单实例）。
    GroupList {
        groups: Vec<GroupInfo>,
        #[serde(default)]
        adapter: Option<String>,
    },
    /// Current QQ filter configuration (allowlist/denylist).
    QqFilterConfig {
        allowlist_users: Vec<i64>,
        allowlist_groups: Vec<i64>,
        denylist_users: Vec<i64>,
        denylist_groups: Vec<i64>,
        #[serde(default)]
        adapter: Option<String>,
    },
    /// QQ gate mode changed.
    QqGateMode {
        mode: crate::mode::GateMode,
        #[serde(default)]
        adapter: Option<String>,
    },
    /// QQ 登录状态（Core 代理查询，Panel 不再直连 OneBot HTTP）。
    QqLoginStatus {
        adapter: String,
        online: bool,
        #[serde(default)]
        user_id: Option<String>,
        #[serde(default)]
        nickname: Option<String>,
    },
    /// QQ 登录二维码（PNG base64；Core 从 NapCat 容器取回）。
    QqQrcode {
        adapter: String,
        /// base64 编码的 PNG；错误时为空字符串。
        png_base64: String,
        #[serde(default)]
        error: Option<String>,
    },
    /// Current QQ owner (admin) QQ number. Frontend-only; never emitted to
    /// the agent/LLM context.
    QqOwner {
        owner_qq: i64,
        #[serde(default)]
        adapter: Option<String>,
    },
    /// QQ friend list (for interactive allowlist/denylist picker).
    FriendList {
        friends: Vec<FriendInfo>,
        #[serde(default)]
        adapter: Option<String>,
    },

    // ---- 工作区会话（workspace 插件）----
    /// 工作区会话列表快照（含激活会话标记）。
    WorkspaceSessions {
        #[serde(default)]
        team_id: Option<String>,
        sessions: Vec<WorkspaceSessionInfo>,
        /// 当前激活会话 id（None = 未激活）。
        #[serde(default)]
        active: Option<String>,
    },
    /// 某会话各工作区目录的 git 状态（`RequestWorkspaceGitStatus` 响应）。
    WorkspaceGitStatus {
        #[serde(default)]
        team_id: Option<String>,
        session_id: String,
        directories: Vec<WorkspaceGitInfo>,
    },
    /// 某工作区目录下的文件列表（`RequestWorkspaceFiles` 响应；只读浏览）。
    WorkspaceFiles {
        #[serde(default)]
        team_id: Option<String>,
        session_id: String,
        /// 被列出的目录绝对路径（回显请求值）。
        path: String,
        entries: Vec<WorkspaceFileEntry>,
        /// 列目录失败的原因（路径越界 / 不存在等；成功时为 None）。
        #[serde(default)]
        error: Option<String>,
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

/// 已退役的编排模式（兼容旧前端）：新字段是 [`LoopMode`]。
///
/// 该枚举只用于 `TeamInfo.orchestration_mode` 的过渡期兼容——`Parallel`
/// 序列化为 `"chatbot"`（旧名），`Single` 为 `"single"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrchestrationMode {
    Single,
    #[default]
    Chatbot,
}

impl From<LoopMode> for OrchestrationMode {
    /// 循环模式 → 旧的编排模式名（`parallel` 即旧 `chatbot`）。
    fn from(value: LoopMode) -> Self {
        match value {
            LoopMode::Single => OrchestrationMode::Single,
            LoopMode::Parallel => OrchestrationMode::Chatbot,
        }
    }
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
    /// 该 agent 的循环模式（互斥循环插件推导：`echo-agent.loop.{single,parallel}`）。
    /// single（默认）：侧边栏不显示「会话」「临时分支」卡片与「全局」分组，
    /// 后端不发射 ReplyBranch* 可见性事件，同一会话的 turn 串行排队；
    /// parallel：全部可见、同一会话可并发分支。取代旧
    /// reply_branches_enabled / global_session_enabled / chat_sessions_enabled
    /// 三布尔（2026-09 协议变更）。
    #[serde(default)]
    pub loop_mode: LoopMode,
    /// Persona system prompt (empty = inherits global prompt/skills).
    #[serde(default)]
    pub system_prompt: String,
    /// Per-persona disabled tool/skill ids（插件黑名单已移除：白名单单轨）。
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
    /// Persona 级 API 供应商引用（None = 跟随全局默认配置；
    /// Some(name) = 使用全局供应商池中该名字的 profile）。
    /// 与 Profile 池、全局默认的展示组合见 API 设置页。
    #[serde(default)]
    pub api_profile: Option<String>,
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
    /// 所属包 id（横跨 plugin+tool+skill 的组合标签；未声明 = 插件 id）。
    #[serde(default)]
    pub package: String,
}

/// QQ group info for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupInfo {
    pub group_id: i64,
    pub group_name: String,
}

/// 联邦 peer 配置与链路状态（`FederationStatus` 列表元素；
/// `SaveFederationPeer` 的载荷——保存时 link 字段忽略）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FederationPeerInfo {
    /// peer 配置名（`[federation.peers.<name>]`）。
    pub name: String,
    /// `ws://host:port`；空 = 仅接受连入。
    #[serde(default)]
    pub url: String,
    /// per-peer 共享密钥。**状态快照中不回传明文**（`token_set` 标记）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    /// 状态快照用：是否已配置 token（代替明文）。
    #[serde(default)]
    pub token_set: bool,
    #[serde(default)]
    pub allow_tools: Vec<String>,
    #[serde(default)]
    pub allow_subagent: bool,
    #[serde(default)]
    pub require_confirm: Vec<String>,
    /// 允许的只读查询种类（Phase 5；node_status 恒允许不在此列）。
    #[serde(default)]
    pub allow_queries: Vec<String>,
    /// 链路状态（状态快照填充；保存请求中忽略）。
    #[serde(default)]
    pub link: FederationLinkState,
}

/// peer 链路状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FederationLinkState {
    #[default]
    Offline,
    Online,
}

/// 一个工作区会话（workspace 插件）：名称 + 多个工作区目录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSessionInfo {
    /// 稳定 id（空 = 由 Core 依名称生成）。
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// 工作区目录（绝对路径列表，一个会话可含多个）。
    ///
    /// federation Phase 0：元素从纯字符串升级为 [`WorkspaceDirectory`]
    /// （untagged：线格式与持久化仍接受纯字符串数组，读旧写新自动迁移）。
    #[serde(default)]
    pub directories: Vec<WorkspaceDirectory>,
}

/// 工作区目录条目（federation Phase 0）。
///
/// serde untagged：旧格式 `"\/abs\/path"`（本机）与新格式
/// `{ "path": "...", "node": "node-..." }`（声明远程归属）均可反序列化；
/// 序列化时本机条目仍写纯字符串（线格式对旧前端零变化），仅带 `node`
/// 的条目写表形式。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkspaceDirectory {
    /// 本机目录（线格式：纯字符串）。
    Local(String),
    /// 带节点归属的目录；`node: None` 等价于本机。
    Qualified {
        path: String,
        /// 归属节点（federation Phase 0 仅解析与保留；远程目录在
        /// Phase 2 之前不可用于工具执行）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<String>,
    },
}

impl WorkspaceDirectory {
    /// 本机目录条目。
    pub fn local(path: impl Into<String>) -> Self {
        Self::Local(path.into())
    }

    /// 目录路径（不含节点维度）。
    pub fn path(&self) -> &str {
        match self {
            Self::Local(p) => p,
            Self::Qualified { path, .. } => path,
        }
    }

    /// 归属节点；`None` = 本机。
    pub fn node(&self) -> Option<&str> {
        match self {
            Self::Local(_) => None,
            Self::Qualified { node, .. } => node.as_deref(),
        }
    }

    /// 是否声明了远程归属。
    pub fn is_remote(&self) -> bool {
        self.node().is_some()
    }
}

impl From<String> for WorkspaceDirectory {
    fn from(path: String) -> Self {
        Self::Local(path)
    }
}

impl From<&str> for WorkspaceDirectory {
    fn from(path: &str) -> Self {
        Self::Local(path.to_string())
    }
}

impl std::fmt::Display for WorkspaceDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.node() {
            Some(node) => write!(f, "node://{node}/{}", self.path()),
            None => f.write_str(self.path()),
        }
    }
}
/// 一个工作区目录的 git 状态快照（只读采集）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceGitInfo {
    pub directory: String,
    /// 是否是 git 仓库（工作树内）。
    pub is_repo: bool,
    /// 当前分支（detached 时为 `HEAD`）。
    #[serde(default)]
    pub branch: Option<String>,
    /// 相对上游的领先/落后提交数（无上游时为 0）。
    #[serde(default)]
    pub ahead: u32,
    #[serde(default)]
    pub behind: u32,
    /// 已暂存（索引区）文件数。
    #[serde(default)]
    pub staged: u32,
    /// 工作区已修改（未暂存）文件数。
    #[serde(default)]
    pub modified: u32,
    /// 未跟踪文件数。
    #[serde(default)]
    pub untracked: u32,
    /// 变更文件路径（`git status --porcelain` 前若干条，供 UI 展示）。
    #[serde(default)]
    pub changed_files: Vec<String>,
    /// 最近一次提交的摘要（`<short-hash> <subject>`）。
    #[serde(default)]
    pub last_commit: Option<String>,
    /// 采集失败原因（目录不存在 / git 不可用等；非仓库时为 None）。
    #[serde(default)]
    pub error: Option<String>,
}

/// 文件浏览器中的一个条目（文件或子目录）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceFileEntry {
    pub name: String,
    /// 绝对路径（目录条目可继续下钻）。
    pub path: String,
    pub is_dir: bool,
    /// 文件字节数（目录为 0）。
    #[serde(default)]
    pub size: u64,
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

#[cfg(test)]
mod workspace_directory_tests {
    use super::*;

    /// 旧格式（纯字符串数组）与新格式（表数组）均可反序列化（untagged）。
    #[test]
    fn directories_accept_legacy_strings_and_qualified_tables() {
        let legacy = r#"{
            "name": "proj",
            "directories": ["/srv/a", "/srv/b/"]
        }"#;
        let info: WorkspaceSessionInfo = serde_json::from_str(legacy).unwrap();
        assert_eq!(info.directories.len(), 2);
        assert!(info.directories.iter().all(|d| !d.is_remote()));
        assert_eq!(info.directories[0].path(), "/srv/a");

        let qualified = r#"{
            "name": "proj",
            "directories": [
                "/srv/local",
                {"path": "/srv/remote", "node": "node-abc"}
            ]
        }"#;
        let info: WorkspaceSessionInfo = serde_json::from_str(qualified).unwrap();
        assert_eq!(info.directories[0].node(), None);
        assert_eq!(info.directories[1].node(), Some("node-abc"));
        assert!(info.directories[1].is_remote());
    }

    /// 序列化：本机条目仍写纯字符串（线格式对旧前端零变化）；带 node 的
    /// 条目写表形式。
    #[test]
    fn directories_serialize_local_as_string_remote_as_table() {
        let info = WorkspaceSessionInfo {
            id: "p".into(),
            name: "proj".into(),
            description: String::new(),
            directories: vec![
                WorkspaceDirectory::local("/srv/local"),
                WorkspaceDirectory::Qualified {
                    path: "/srv/remote".into(),
                    node: Some("node-abc".into()),
                },
            ],
        };
        let v = serde_json::to_value(&info).unwrap();
        assert_eq!(v["directories"][0], serde_json::json!("/srv/local"));
        assert_eq!(
            v["directories"][1],
            serde_json::json!({"path": "/srv/remote", "node": "node-abc"})
        );
    }

    #[test]
    fn display_qualifies_remote_paths() {
        assert_eq!(WorkspaceDirectory::local("/srv/a").to_string(), "/srv/a");
        assert_eq!(
            WorkspaceDirectory::Qualified {
                path: "/srv/r".into(),
                node: Some("node-x".into()),
            }
            .to_string(),
            "node://node-x//srv/r"
        );
    }
}
