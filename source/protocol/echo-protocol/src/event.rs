//! Events emitted by the agent backend to subscribers (TUI, API, ...).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::mode::LoopMode;

/// 状态快照 → 线上事件的构造器。
///
/// 工具用 `Tool::state_key` 声明"我执行完有可见状态"，而**这状态长什么线上
/// 事件**是协议形状——由绑定该工具的插件给出，不由执行链按工具名猜。
///
/// 参数是 `(归属会话, 状态快照)`：状态是 per-session 的（清单按会话隔离），
/// 线上事件必须带上归属会话，否则面板会把 A 会话的状态挂到 B 会话上。
pub type ToolStateEventBuilder =
    std::sync::Arc<dyn Fn(&str, &serde_json::Value) -> BackendEvent + Send + Sync>;


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

/// PluginInstance status reported to the TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub name: String,
    pub display_name: String,
    pub connected: bool,
    pub running: bool,
    pub self_id: Option<String>,
    pub bind_address: String,
    pub started_at: Option<i64>,
    /// 归属人格 id（概念中立；2026-09 多实例）。
    #[serde(default)]
    pub persona: Option<String>,
    /// 平台自有展示型字段的通用 KV 承载。
    ///
    /// **当前恒为空**（2026-10 去适配器化）：平台展示字段改由插件自己的
    /// **UI 描述符**声明（字段绑定 `instance.*`，Panel 通用渲染器按 `bind`
    /// 取值）——单一来源，Core 不必逐键命名再配中文标签。字段保留不改线协议。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata: Vec<InstanceMetadataEntry>,
    /// 平台标识（如 `"qq"`；来自 `InstanceInfo.platform`）。Panel 会话分组 /
    /// 通用化寻址需要它上线（W2.3 泛化 Core-1，2026-10）；旧 Panel 忽略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// UI 描述符（`InstanceContribution.ui` 原样透传；Panel 通用渲染器按
    /// `ui.version` 选择渲染方案）。None = 插件未声明 UI。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<serde_json::Value>,
    /// 会话标签（`InstanceContribution.session_labels` 原样透传；如
    /// `{"dm":"私聊","group":"群聊"}`）。None = 未声明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_labels: Option<serde_json::Value>,
}

/// 插件实例展示型元数据条目（通用 KV 承载）。
///
/// **当前恒为空**：平台自有的展示字段改由插件自己的 **UI 描述符**声明
/// （字段绑定 `instance.*`，Panel 的通用渲染器按 `bind` 取值）——那是单一
/// 来源，Core 不必再逐键命名「容器 / WebUI / OneBot API」再配中文标签
/// （2026-10 去适配器化）。字段保留是为了不改线协议：将来若有「不适合进
/// 描述符」的只读展示值，仍可走这里。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceMetadataEntry {
    /// 稳定键（程序消费）。
    pub key: String,
    /// 展示标签（UI 渲染）。
    pub label: String,
    /// 展示值（字符串化）。
    pub value: String,
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

/// 运行中的回复分支（`ActiveTurnsSnapshot.branches` 的元素）。
///
/// 字段是 `ReplyBranchStarted` 的子集：前端拿到它就能按与实时事件**同一套**
/// reducer 重建分支卡/任务卡，不必再维护第二条代码路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveBranchInfo {
    pub session_id: String,
    pub branch_id: String,
    pub message_sequence: u64,
    /// 任务文本（Core 侧已截断到 200 字符：任务卡只显示一行摘要）。
    #[serde(default)]
    pub task: String,
    /// 目标标签（普通入站分支为「源会话」）。
    #[serde(default)]
    pub target: String,
    pub started_at_ms: i64,
}

/// Snapshot of a conversation session, for the TUI session list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    /// 来源节点身份（federation NodeId，`node-<ulid>`）。多节点聚合客户端据此
    /// 区分同名会话（`local:tui::local_user` 在每个 Core 上都相同）。中继
    /// 聚合视图另有来源 core 名（信封），两者可对照。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// 运行区域展示名（`[core].region_name`；空 = 回退 NodeId）。与 node_id
    /// 共同构成 agent 的「运行区域」属性。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_name: Option<String>,
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
    pub instance_name: String,
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
    /// 工具开始执行的 epoch 毫秒时间戳（面板「运行中实时计时」用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    /// 工具执行总耗时（毫秒；中断场景 = 开始到中断判定为止）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
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
    /// **位置序号**（push 时分配，此后永不改变；0 = 旧版/未赋值的条目）。
    ///
    /// 与 `seq` 的分工：`seq` 服务于**增量同步**（就地更新会推进它，因此
    /// 非单调）；`ord` 服务于**分页锚点**（永不变更，严格随 push 递增）。
    ///
    /// 为什么分页不能复用 `seq`：运行中的工具条目会被就地更新并推进 `seq`
    /// （位置不变、序号变大），于是缓冲区里存在 seq 乱序——实测 1024 条里有
    /// 46 处非单调。按 seq 定位锚点会过早截断并**漏条目**。按"已加载条数"
    /// 定位同样不行：分页期间的新增会让窗口错位。只有「永不变更的 push 序号」
    /// 才是稳定锚点。
    #[serde(default)]
    pub ord: u64,
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
            ord: 0,
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
            ord: 0,
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
            ord: 0,
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
            ord: 0,
        }
    }
}

/// Events flowing from the backend to the TUI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum BackendEvent {
    // ---- PluginInstance lifecycle ----
    /// An instance's connection state changed.
    InstanceStateChanged {
        instance_name: String,
        connected: bool,
        self_id: Option<String>,
    },

    // ---- Incoming messages ----
    /// A message was received from any platform.
    MessageReceived {
        session_id: String,
        instance_name: String,
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
        /// 工具开始执行的 epoch 毫秒时间戳（面板「运行中实时计时」用）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at_ms: Option<i64>,
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
        /// 工具执行总耗时（毫秒；中断场景 = 开始到中断判定为止）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
    },
    /// 流式回复正文增量（`AgentOutput` 的逐段版本）。
    ///
    /// provider 在生成过程中实时发出（合并节流后每 ~40ms 一批）；终值仍以
    /// `AgentOutput`（全量）收敛——消费端按 (core, session, branch) 对账，
    /// 全量到达时用其替换流式消息内容（丢帧/乱序可自愈）。
    AgentContentDelta {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
        /// 归属分支（与本 turn 的 `AgentReasoning` 同值）。
        #[serde(default)]
        branch_id: String,
        /// 本段增量文本（上游已脱敏）。
        delta: String,
    },
    /// 流式推理增量（`AgentReasoning` 的逐段版本）。
    ///
    /// 与正文增量同构；终值以 `AgentReasoning`（全量）收敛。
    AgentReasoningDelta {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
        #[serde(default)]
        branch_id: String,
        delta: String,
    },
    /// A tool published a structured state snapshot for TUI visualization
    /// (e.g. the checklist tool → checklist panel). `state` is tool-specific;
    /// the checklist tool emits `{"lists": [{name, done, total, items}]}`.
    ChecklistUpdated {
        session_id: String,
        state: serde_json::Value,
        /// 归属人格（2026-10）：同一 core 内多 persona 的同名会话（各自
        /// 的 `local:tui::local_user`）此前在消费端（core+session_id）键
        /// 冲突——补 team 维度供消费端消歧。
        #[serde(default)]
        team_id: Option<String>,
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
        /// 正在运行的回复分支（带任务文本与起始时刻）。
        ///
        /// 必要性：`ReplyBranchStarted` 是**只在分支开始那一刻发一次**的推送
        /// 事件，Panel 重连/刷新后拿不到它，而协议里没有"查运行分支"的命令
        /// ——分支卡与任务卡于是永远是空的，只能等下一个分支开始才恢复。
        /// 快照带上这几项后，前端可在重连时就地重建。
        ///
        /// 向后兼容：旧前端忽略该字段；旧 Core 不发时前端按空数组处理
        /// （只影响重连恢复，不影响实时事件）。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        branches: Vec<ActiveBranchInfo>,
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
        /// 本节点是否接受**联邦级 Panel 身份**（`[federation] panel_keys`
        /// 非空且 `panel_access != "off"`；P1 2026-11）。Panel 据此在界面上
        /// 区分「本节点已开放/未开放」——未开放时用 panel 密钥连入会 401。
        #[serde(default)]
        panel_ready: bool,
        /// 授予 Panel 身份的级别（`off` / `readonly` / `operate` / `admin`）。
        /// 空串 = 旧版 Core（无此概念），按 `off` 处理。
        #[serde(default)]
        panel_access: String,
    },
    /// 联邦节点目录（`RequestFederationDirectory` 响应；P2「自动发现」）。
    ///
    /// 只含**直接邻居**（不做 gossip）：Panel 拿到后逐个用 Panel 身份密钥
    /// 尝试接入，成功即纳入聚合视图（会话列表/区域切换/拓扑随之完整）。
    /// 因此本条事件的语义是「我这个节点知道的地址」而不是「全网成员」——
    /// 全网的连通性由 Panel 侧汇总（多接入点各报一份邻居表）。
    FederationDirectory {
        /// 应答节点自身身份。
        #[serde(default)]
        node_id: String,
        #[serde(default)]
        node_name: Option<String>,
        /// 本机联邦监听/宣告地址（`host:port`；空 = 不监听纯连出）。
        #[serde(default)]
        listen: String,
        /// 应答节点自身是否开放 Panel 身份（同 `FederationStatus.panel_ready`）。
        #[serde(default)]
        panel_ready: bool,
        /// 应答节点授予 Panel 身份的级别。
        #[serde(default)]
        panel_access: String,
        /// 直接邻居（含离线——离线条目仍给出地址与历史宣告，Panel 可试连）。
        #[serde(default)]
        nodes: Vec<FederatedNodeInfo>,
    },
    /// 会话迁移结果（`MigrateSession` 响应）。
    SessionMigrated {
        /// 源会话 id。
        session_id: String,
        /// 目标 peer 名。
        target_peer: String,
        /// 目标侧新会话 id（成功时 Some）。
        #[serde(default)]
        new_session_id: Option<String>,
        /// 成功/失败说明。
        message: String,
        #[serde(default)]
        success: bool,
    },
    /// 自更新状态（`RequestSelfUpdate` 受理回执 / `RequestSelfUpdateStatus`
    /// 查询响应 / 状态变更广播）。`state` 取自 update-status 文件
    /// （running/restarting/updated/failed/rolled_back/idle）。
    SelfUpdateStatus {
        /// 当前状态（`update-status` 文件的 state 字段；无文件 = `idle`）。
        state: String,
        /// 当前阶段（update-status 的 phase：initializing/building/
        /// installing/restarting/verifying；细粒度进度反馈）。
        #[serde(default)]
        phase: Option<String>,
        /// 目标版本/说明（update-status 的 revision/message，原样透传）。
        #[serde(default)]
        revision: Option<String>,
        #[serde(default)]
        message: Option<String>,
        /// 时间戳（update-status 的 updated_at，原样透传）。
        #[serde(default)]
        updated_at: Option<String>,
    },
    /// 本机邀请串（`RequestFederationInvite` 响应）。
    FederationInvite {
        invite: String,
    },
    /// 压缩 / 归档的**进度**（`CompactHistory` / `ArchiveHistory` 执行期间）。
    ///
    /// 为什么要有独立事件：压缩会逐会话调 LLM 生成摘要，可能持续数分钟。
    /// 此前 Core 只在开始与结束各发一句话（还借用 `Error` 事件），中间几分钟
    /// 面板没有任何反馈 —— 用户无法区分"正在跑"与"卡死"，唯一能做的就是
    /// 刷新重来，而重来会撞上 `compact_lock`（看起来像毫无反应）。
    ///
    /// 走线固定：`preview → archive → summarize(逐组递增) → apply → done`
    /// （失败时最后一步是 `failed`，`message` 带原因）。面板据 `phase`
    /// 决定进度条形态：`total == 0` 画不确定条，否则按 `done/total` 画。
    CompactionProgress {
        /// 归属 team（persona）。多上游面板据此把进度归到对应 agent。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
        /// 阶段：`preview` / `archive` / `summarize` / `apply` / `done` / `failed`。
        phase: String,
        /// 已完成步数。
        #[serde(default)]
        done: usize,
        /// 总步数（0 = 该阶段长度不确定）。
        #[serde(default)]
        total: usize,
        /// 本次操作涉及的会话数。
        #[serde(default)]
        sessions: usize,
        /// 面向用户的一句话（面板直接显示）。
        message: String,
    },
    /// The current conversation context snapshot (`RequestContext` response).
    ContextSnapshot {
        /// 快照归属的会话 id（多会话，2026-09；None = 未指定会话的回退口径）。
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
        /// 列表归属 team（与请求一致；无归属（缺省）时前端不过滤）。
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
        /// 本次快照是否**被窗口上限截断**（还有更早的条目可翻）。
        ///
        /// 只在 `RequestTrunkTimeline { limit: Some(n) }` 时可能为 true；
        /// 缺省 false = 已含全部（旧版 Core / 未截断），前端不需要翻页。
        #[serde(default)]
        has_more: bool,
    },
    /// 更早的一页时间线（`RequestTimelineEarlier` 的响应；2026-10 分页）。
    ///
    /// 前端把 `messages` **前插**到已加载列表头部（不动同步游标）；`has_more`
    /// 为 false 表示已到时间线开头（再往上翻没有更多了）。
    TimelineEarlier {
        /// 响应归属：= 请求的 team_id（None = 主/默认 agent）。
        #[serde(default)]
        team_id: Option<String>,
        /// 时间升序的更早条目（`seq` 均小于请求的 `before_seq`）。
        messages: Vec<TimelineMessage>,
        /// 是否还有更早的条目。
        #[serde(default)]
        has_more: bool,
    },
    /// 某会话的显示历史（`RequestSessionHistory` 的响应；2026-10）。
    /// 从事件日志按会话派生（镜像显示时间线的形状），时间升序。
    SessionHistory {
        /// 响应归属：= 请求的 team_id（None = 主/默认 agent）。
        #[serde(default)]
        team_id: Option<String>,
        /// = 请求的 session_id（回显）。
        session_id: String,
        messages: Vec<TimelineMessage>,
        /// 派生出的条目总数（截断前）。
        total: usize,
        /// true = 因 limit 只含最近 limit 条。
        truncated: bool,
        /// 派生时该会话在显示时间线中的最大 seq（**合并水位线**：面板据此
        /// 跳过窗口里已被快照覆盖的条目；0 = 该会话不在窗口内）。
        #[serde(default)]
        max_seq: u64,
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
    /// 联邦远端节点的脱敏供应商池（`RequestRemoteApiProfiles` 响应；
    /// 失败时 `profiles` 为空、`error` 带原因——peer 离线/未实现查询）。
    RemoteApiProfiles {
        peer: String,
        #[serde(default)]
        active: Option<serde_json::Value>,
        #[serde(default)]
        profiles: Vec<serde_json::Value>,
        #[serde(default)]
        error: Option<String>,
    },
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
    /// **插件操作回执**（`TogglePlugin` / `RestartPlugin` 的结构化结果）。
    ///
    /// 为什么需要：此前成功与失败都走 `Error { message }`——面板无法把
    /// 「操作成功」与「真错误」分开渲染，只能靠文案猜。现在按操作回一条
    /// 结构化回执，`PluginsList` 紧跟着刷新（状态以列表为准，回执只管
    /// 这一次操作的成败与原因）。
    PluginOpResult {
        /// 目标插件 id。
        id: String,
        /// 操作：`enable` / `disable` / `restart`。
        op: String,
        /// 是否成功。
        ok: bool,
        /// 人读说明（成功也带一句，便于 toast 直接显示）。
        message: String,
    },
    /// **插件事件**（plugin-framework v0.7 的事件通道出口）。
    ///
    /// 插件在运行期 `emit` 的东西（注册表自己也会发 `plugin.mounted` /
    /// `plugin.unmounted`）经这里到前端。`name` / `payload` 是**插件与宿主
    /// 自行约定**的词汇，Core 不解释——所以协议只搬三个字段，新产品加事件
    /// 不必改 Core 协议。
    ///
    /// 面板把它当"插件活动日志"用：谁挂上了、谁发了个通知。**不是**领域
    /// 事件（会话/工具那些有各自的强类型变体）。
    PluginEvent {
        /// 来源插件 id（宿主自身发出的为空串）。
        #[serde(default)]
        plugin: String,
        /// 事件名（点分小写约定）。
        name: String,
        /// 载荷（自由 JSON）。
        #[serde(default)]
        payload: serde_json::Value,
        /// 服务端时间戳（毫秒；面板排序用，不信任客户端时钟）。
        #[serde(default)]
        at_ms: u64,
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
        /// 赠送余额（granted_balance），如 "0.00"；失败时为空串。
        #[serde(default)]
        granted: String,
        /// 充值余额（topped_up_balance），如 "110.00"；失败时为空串。
        #[serde(default)]
        topped_up: String,
        /// 币种，如 "CNY"；失败时为空串。
        #[serde(default)]
        currency: String,
        /// Human-readable message (error detail or success note).
        #[serde(default)]
        message: String,
    },
    /// API profile 指标时间序列（响应 `QueryApiMetrics`）：Core 本地积累的
    /// 余额快照（`balance_snapshot_secs` 周期采样）与 token 用量（每次 LLM
    /// 调用记录、按小时 × 模型聚合）。窗口 = 近 7 天。
    ApiMetrics {
        /// 每个被查询目标一条（默认配置 + 全部 profile 或指定 profile）。
        #[serde(default)]
        entries: Vec<ApiMetricsEntry>,
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

    // ---- PluginInstance management ----
    InstanceList {
        instances: Vec<InstanceStatus>,
    },
    /// 插件实例管理方法调用结果（W2.1，2026-10 插件实例管理通道；
    /// [`crate::command::BackendCommand::InvokeInstanceOp`] 的响应）。
    InstanceOpResult {
        /// 插件实例名（回显请求值）。
        instance: String,
        /// 方法名（回显请求值）。
        method: String,
        /// 成功时的结构化结果（方法自定义；如 `{"mode":"allowlist"}`）。
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        result: serde_json::Value,
        /// 失败原因（成功为 None）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// 插件实例状态推送（W2.1，2026-10）：插件/插件实例主动上报（含
    /// `report_ui_state` 与下次拉取状态的刷新触发）；`state` 为
    /// `InstanceManagement::cached_state` 同构文档（epoch 版本化由宿主缓存管理）。
    InstanceState {
        /// 插件实例名。
        instance: String,
        /// 状态文档（平台自定义；约定键见 `InstanceManagement::cached_state`）。
        state: serde_json::Value,
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
        /// 当前激活对话 id（2026-10 多对话；`active` 仍为工作区 id）。
        #[serde(default)]
        active_conversation: Option<String>,
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
    /// 目录选择器浏览结果（`RequestBrowseDirectories` 响应，2026-10）。
    BrowseDirectories {
        /// 被浏览的位置（回显请求值；None = 根列表）。
        #[serde(default)]
        path: Option<String>,
        /// 浏览根（path=None 时列出）：label（展示名，如「家目录 (~)」）+
        /// path。远程节点浏览时 roots 来自对端。
        #[serde(default)]
        roots: Vec<BrowseRoot>,
        /// 目录条目（`path` 非空时；已过滤为子目录——选择器只选目录）。
        #[serde(default)]
        entries: Vec<WorkspaceFileEntry>,
        /// 失败原因（越界 / 不存在 / 对端离线等；成功为 None）。
        #[serde(default)]
        error: Option<String>,
    },
}

/// 目录选择器的浏览根（可进入浏览的位置）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowseRoot {
    /// 展示名（Panel 主标签），如「家目录」「<工作区名>」。
    pub label: String,
    /// 绝对路径（本机）或 `node://<peer>/<abs>`（远程）。
    pub path: String,
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
    /// Owning package id (e.g. "echo-agent.instance.qq"); None = standalone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
}

/// One team member (frontend display).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamInfo {
    pub id: String,
    /// 来源节点身份（federation NodeId）。多节点聚合时人格身份 = (node_id, id)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// 运行区域展示名（`[core].region_name`；空 = 回退 NodeId）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_name: Option<String>,
    pub name: String,
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
    ///
    /// **来源**维度：门控 manifest 编译在 Core 里，但实现可能在独立进程
    /// （QQ 插件实例即是；见 `runtime`）。
    #[serde(default = "default_true")]
    pub builtin: bool,
    /// 运行形态：`in_process`（实现编译在 Core 进程内）/ `subprocess`
    /// （实现由独立子进程提供，Core 侧只有启停门控）。
    ///
    /// 旧 Core 不发该字段 → 空串，Panel 按 `in_process` 兼容。
    #[serde(default)]
    pub runtime: String,
    /// 所属包 id（横跨 plugin+tool+skill 的组合标签；未声明 = 插件 id）。
    #[serde(default)]
    pub package: String,
    /// 挂载失败原因（`enabled == false` 且是内置插件时可能有值）。
    ///
    /// 面板据此区分「用户主动禁用」与「装配失败被跳过」——此前协议不带
    /// 该字段（`PluginDescriptor::error` 有、`PluginInfo` 丢了），失败原因
    /// 只能翻日志。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 声明该插件的配置层（`plugins.toml` / `bundle:base.toml` …）；
    /// 空 = 配置未提及（编译内置的缺省行为）。
    #[serde(default)]
    pub config_origin: String,
    /// **运行态**（子进程插件）：`pending` / `ready` / `draining` /
    /// `stopped` / `failed`。空 = 进程内实现（没有独立生命周期）。
    ///
    /// 与 `enabled` 正交：`enabled = true` + `state = failed` 是「配置开了、
    /// 进程起不来」——三种组合面板要能分辨。
    #[serde(default)]
    pub state: String,
    /// 崩溃后已自动重启次数（子进程插件）。
    #[serde(default)]
    pub restarts: u32,
    /// 连接代际（每次就绪自增；子进程插件）。
    #[serde(default)]
    pub generation: u64,
    /// 插件定义文件路径（`plugins.toml` / 数据插件的 `plugin.toml`）；
    /// 空 = 编译内置（无外部定义文件）。
    #[serde(default)]
    pub source_path: String,
    /// 子进程启动命令（空 = 进程内实现）。
    #[serde(default)]
    pub command: String,
    /// 启动参数。
    #[serde(default)]
    pub args: Vec<String>,
    /// 环境变量**键名**（值不下发——常含 token / 密钥）。
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// 本插件**要求的服务键**（`Inject::required`；空 = 无要求）。
    ///
    /// 与 `error` 一起回答「为什么不挂载」：Core 挂载前逐个校验，缺失即拒绝。
    #[serde(default)]
    pub required_services: Vec<String>,
    /// 软依赖（`Inject::optional`；缺失不阻止挂载）。
    #[serde(default)]
    pub optional_services: Vec<String>,
    /// 眼下**还缺**的硬依赖（空 = 不缺）。
    ///
    /// 与 `error` 的区别：`error` 是「上次挂载尝试的结果」，这里是「此刻的
    /// 事实」——依赖到位后 Core 会自动挂上并清空它。
    #[serde(default)]
    pub pending_services: Vec<String>,
    /// 订阅的事件名（`Contribution::Events` 的进程内等价物；空 = 不订阅）。
    #[serde(default)]
    pub subscribed_events: Vec<String>,
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
    /// 对端负载（活跃 turn 数；2026-10 P2 调度器输入——来自对端握手
    /// 时宣告的 NodeCaps.active_turns，无数据时为 0）。
    #[serde(default)]
    pub active_turns: u32,
    /// 对端 node_id（链路在线时由运行时映射填充；离线/未学习时为空）。
    ///
    /// 2026-10 联邦组建向导：Panel 以此精确判定「两节点是否已互联」
    /// （不依赖 peer 名启发式）、构建拓扑视图。旧 Core 不发该字段
    /// （serde default = None）——消费方需按缺失兜底。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// 连续拨号失败次数（成功建链清零；0 = 正常/无记录）。
    ///
    /// 2026-10 地址漂移事故新增：对端外部地址变化（NAT 映射更换）后，
    /// 本机对旧地址持续拨号失败、界面毫无提示（失败只有 debug 日志）。
    /// 该计数 + `last_error` 供 Panel 把"持续连不上"显性化。
    #[serde(default)]
    pub consecutive_failures: u32,
    /// 最近一次拨号失败原因（在线或从未失败时为空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// 最近一次成功建链时间（unix 秒；从未成功 = None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_online_at: Option<i64>,
    /// 最近一次从对端 Hello 学到的可拨地址（**跨断链保留**）。
    ///
    /// 地址漂移诊断：与配置 `url` 不同且离线时，Panel 提示"对端最近
    /// 宣告了 X，一键改用"（人工修复路径——不做自动覆盖，防误改
    /// 多网卡/手工配置场景）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_advertise: Option<String>,
}

/// 联邦节点目录里的一个**邻居**条目（`FederationDirectory.nodes[]`，P2 2026-11）。
///
/// 字段取舍的原则：Panel 需要「能否连上 + 连到哪个地址 + 是不是已经是我
/// 配置里的那台」，其余（工具白名单、策略）属于本节点与对端的私事，不下发。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FederatedNodeInfo {
    /// 该节点在本机配置里的 peer 名（`[federation.peers.<name>]`）。
    pub name: String,
    /// 对端 node_id（`node-<ulid>`）。离线且从未连上过时为空串。
    #[serde(default)]
    pub node_id: String,
    /// 对端人类可读别名（握手 Hello 携带；未知为空）。
    #[serde(default)]
    pub node_name: Option<String>,
    /// 本机配置的拨号地址（`ws://host:port`；空 = 仅接受连入，不可拨）。
    ///
    /// 注意这是**本机视角**的地址（可能是内网/VPN）——Panel 未必可达；
    /// 因此 `advertise` 优先。
    #[serde(default)]
    pub url: String,
    /// 对端最近宣告的可拨地址（握手学到；跨断链保留）。Panel 优先用它试连。
    #[serde(default)]
    pub advertise: Option<String>,
    /// 链路当前是否在线。
    #[serde(default)]
    pub online: bool,
    /// 对端是否开放 Panel 身份（来自 Hello 的能力声明；旧版对端恒 false
    /// = 未声明，Panel 仍可试连，失败即按「未开放」处理）。
    #[serde(default)]
    pub panel_ready: bool,
}

/// peer 链路状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FederationLinkState {
    #[default]
    Offline,
    Online,
}

/// 工作区内的一条对话（2026-10 多对话）：名称 + 通道 id 派生标记。
///
/// 每条对话对应一条独立的本地通道会话（独立上下文/预算/压缩清理）：
/// - `legacy = true`：存量通道 id `local:workspace:<ws>:local_user`（每工作区
///   至多一条；旧数据的首条对话——事件日志 append-only，id 不可改写）；
/// - 其余对话：`local:workspace:<ws>/<conv>:local_user`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceConversationInfo {
    /// 稳定 id（slug；工作区内唯一，不含 `/`）。
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// 通道 id 是否为存量裸 ws 格式（见类型注释）。
    #[serde(default)]
    pub legacy: bool,
    /// 会话级 API 覆盖（2026-10）：`Some(name)` = 该对话的 LLM 请求走
    /// 全局供应商池中该 profile 的 provider；`None` = 跟随 persona 级。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_profile: Option<String>,
}

/// 一个工作区会话（workspace 插件）：名称 + 多个工作区目录。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// 工作区内的对话列表（2026-10 多对话）。空 = 仅存量主对话
    /// （读取期惰性补位 `{id:"main", legacy:true}`，不落盘）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversations: Vec<WorkspaceConversationInfo>,
    /// 最后使用的对话 id（激活工作区不带对话时进入它）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_conversation: Option<String>,
}

/// 工作区目录条目（federation Phase 0）。
///
/// serde untagged：旧格式 `"\/abs\/path"`（本机）与新格式
/// `{ "path": "...", "node": "node-..." }`（声明远程归属）均可反序列化；
/// 序列化时本机条目仍写纯字符串（线格式不变），仅带 `node`
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
            // 单斜杠统一（2026-10）：`node://<peer>/<绝对路径>`——与提示词
            // 教模型的写法、coding 工具解析器（`node://<peer>/<路径>`）
            // 及 git 状态标注完全一致。path 以 `/` 开头，故这里去掉前导
            // 斜杠再拼（此前产生 `node://peer//srv` 双斜杠，解析侧会得到
            // `//srv` 路径）。
            Some(node) => write!(f, "node://{node}/{}", self.path().trim_start_matches('/')),
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

/// 单个 API profile 的指标序列（[`BackendEvent::ApiMetrics`] 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiMetricsEntry {
    /// 配置名（空 = 顶层默认配置）。
    pub name: String,
    /// 余额快照序列（成功快照；近 7 天，时间升序）。
    #[serde(default)]
    pub balance: Vec<ApiBalancePoint>,
    /// token 用量序列（按小时 × 模型聚合；近 7 天，时间升序）。
    #[serde(default)]
    pub usage: Vec<ApiUsagePoint>,
    /// 费用估算币种（如 "CNY"；无任何可估算用量时为空串）。
    #[serde(default)]
    pub cost_currency: String,
}

/// 单条余额快照（金额单位 = `currency`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiBalancePoint {
    /// 采样时间戳（Unix 毫秒）。
    pub ts_ms: i64,
    /// 总余额（含赠送）。
    pub total: f64,
    /// 赠送余额。
    pub granted: f64,
    /// 充值余额。
    pub topped_up: f64,
    /// 币种，如 "CNY"。
    #[serde(default)]
    pub currency: String,
}

/// 单条 token 用量（小时桶 × 模型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiUsagePoint {
    /// 小时桶起点（Unix 毫秒）。
    pub ts_ms: i64,
    /// 模型名（请求所用 model）。
    pub model: String,
    /// 输入（prompt）token 数。
    pub prompt_tokens: u64,
    /// 输出（completion）token 数。
    pub completion_tokens: u64,
    /// 缓存命中 tokens（prompt 内子集；DeepSeek 口径；旧数据缺省 0）。
    #[serde(default)]
    pub cache_hit_tokens: u64,
    /// 该桶内调用次数。
    pub calls: u64,
    /// 估算费用（按 `[agent.pricing]` 定价表；无匹配价格 = None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_cost: Option<f64>,
}

/// Full state snapshot sent to the TUI on connect.
#[derive(Debug, Clone, Default)]
pub struct BackendState {
    pub instances: HashMap<String, InstanceStatus>,
    pub active_model: String,
    pub active_provider: String,
    pub system_prompt: String,
    pub enabled_skills: Vec<String>,
    pub sessions: Vec<SessionInfo>,
}

/// `BackendEvent` is dispatchable on the harness event bus: the enum derives
/// Clone + Debug + Send + Sync, this impl declares the membership so
/// listeners can subscribe by event type (dsh typed events).
impl BackendEvent {
    /// 变体名的**零分配**取值（等于 serde 的线上 tag）。
    ///
    /// `BackendEvent` 没有 `rename_all`，所以线上 tag 就是 Rust 变体名——
    /// 本函数返回的字符串与 `serde_json::to_value(&event)` 的第一个键逐字
    /// 相同。（有测试守着这条等价关系。）
    ///
    /// 为什么需要它：插件事件通道要**先按名字判断有无订阅者，再决定要不要
    /// 序列化载荷**。流式事件（`AgentContentDelta` 等）每个 token 一条，若为了
    /// 拿名字而先 `to_value` 一遍，等于给热路径加一次全量序列化。这里是一次
    /// 无分配的 match。
    ///
    /// **穷举 match，没有 `_` 分支**：新增变体时编译器会逼你在这里起名——
    /// 忘了起名会让插件永远收不到该事件，而这种静默失败比编译错误难查得多。
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::InstanceStateChanged { .. } => "InstanceStateChanged",
            Self::MessageReceived { .. } => "MessageReceived",
            Self::AgentThinking { .. } => "AgentThinking",
            Self::LlmRequest { .. } => "LlmRequest",
            Self::LlmResponse { .. } => "LlmResponse",
            Self::AgentReasoning { .. } => "AgentReasoning",
            Self::SubagentStarted { .. } => "SubagentStarted",
            Self::SubagentCompleted { .. } => "SubagentCompleted",
            Self::ReplyBranchStarted { .. } => "ReplyBranchStarted",
            Self::ReplyBranchContent { .. } => "ReplyBranchContent",
            Self::ReplyBranchCompleted { .. } => "ReplyBranchCompleted",
            Self::ToolCall { .. } => "ToolCall",
            Self::ToolResult { .. } => "ToolResult",
            Self::AgentContentDelta { .. } => "AgentContentDelta",
            Self::AgentReasoningDelta { .. } => "AgentReasoningDelta",
            Self::ChecklistUpdated { .. } => "ChecklistUpdated",
            Self::AgentOutput { .. } => "AgentOutput",
            Self::AgentCompleted { .. } => "AgentCompleted",
            Self::SessionUpdated { .. } => "SessionUpdated",
            Self::ActiveTurnsSnapshot { .. } => "ActiveTurnsSnapshot",
            Self::FederationStatus { .. } => "FederationStatus",
            Self::FederationDirectory { .. } => "FederationDirectory",
            Self::SessionMigrated { .. } => "SessionMigrated",
            Self::SelfUpdateStatus { .. } => "SelfUpdateStatus",
            Self::FederationInvite { .. } => "FederationInvite",
            Self::CompactionProgress { .. } => "CompactionProgress",
            Self::ContextSnapshot { .. } => "ContextSnapshot",
            Self::ShellSessionsList { .. } => "ShellSessionsList",
            Self::ShellSessionStarted { .. } => "ShellSessionStarted",
            Self::ShellExecStarted { .. } => "ShellExecStarted",
            Self::ShellExecOutput { .. } => "ShellExecOutput",
            Self::ShellExecDone { .. } => "ShellExecDone",
            Self::ShellSessionClosed { .. } => "ShellSessionClosed",
            Self::TrunkTimeline { .. } => "TrunkTimeline",
            Self::TimelineEarlier { .. } => "TimelineEarlier",
            Self::SessionHistory { .. } => "SessionHistory",
            Self::ApiConfigUpdated { .. } => "ApiConfigUpdated",
            Self::RemoteApiProfiles { .. } => "RemoteApiProfiles",
            Self::ApiProfilesUpdated { .. } => "ApiProfilesUpdated",
            Self::SkillsList { .. } => "SkillsList",
            Self::ToolsList { .. } => "ToolsList",
            Self::PluginsList { .. } => "PluginsList",
            Self::PluginOpResult { .. } => "PluginOpResult",
            Self::PluginEvent { .. } => "PluginEvent",
            Self::TeamsList { .. } => "TeamsList",
            Self::ApiTestResult { .. } => "ApiTestResult",
            Self::ApiBalanceResult { .. } => "ApiBalanceResult",
            Self::ApiMetrics { .. } => "ApiMetrics",
            Self::SystemPrompt { .. } => "SystemPrompt",
            Self::Error { .. } => "Error",
            Self::InstanceList { .. } => "InstanceList",
            Self::InstanceOpResult { .. } => "InstanceOpResult",
            Self::InstanceState { .. } => "InstanceState",
            Self::WorkspaceSessions { .. } => "WorkspaceSessions",
            Self::WorkspaceGitStatus { .. } => "WorkspaceGitStatus",
            Self::WorkspaceFiles { .. } => "WorkspaceFiles",
            Self::BrowseDirectories { .. } => "BrowseDirectories",
        }
    }
}

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

    /// 序列化：本机条目仍写纯字符串（线格式不变）；带 node 的
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
            conversations: vec![],
            last_conversation: None,
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
        // 单斜杠统一：与提示词/工具解析器一致（2026-10）
        assert_eq!(
            WorkspaceDirectory::Qualified {
                path: "/srv/r".into(),
                node: Some("node-x".into()),
            }
            .to_string(),
            "node://node-x/srv/r"
        );
    }
}

#[cfg(test)]
mod instance_status_tests {
    use super::*;

    fn status_with_ui() -> InstanceStatus {
        InstanceStatus {
            name: "qq-default".into(),
            display_name: "QQ / OneBot".into(),
            connected: true,
            running: true,
            self_id: Some("10001".into()),
            bind_address: "127.0.0.1:3100".into(),
            started_at: Some(1),
            persona: None,
            metadata: vec![],
            platform: Some("qq".into()),
            ui: Some(serde_json::json!({"version": 1, "panels": []})),
            session_labels: Some(serde_json::json!({"dm": "私聊", "group": "群聊"})),
        }
    }

    /// S1：新三字段 round-trip（线上形状：platform / ui / session_labels）。
    #[test]
    fn instance_status_round_trips_ui_fields() {
        let status = status_with_ui();
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["platform"], serde_json::json!("qq"));
        assert_eq!(value["ui"]["version"], serde_json::json!(1));
        assert_eq!(value["session_labels"]["dm"], serde_json::json!("私聊"));
        let back: InstanceStatus = serde_json::from_value(value).unwrap();
        assert_eq!(back.platform.as_deref(), Some("qq"));
        assert_eq!(back.ui.unwrap()["version"], serde_json::json!(1));
        assert_eq!(
            back.session_labels.unwrap()["group"],
            serde_json::json!("群聊")
        );
    }

    /// S1：旧 JSON（缺三字段）可解；None 时序列化省略（旧 Panel 无感）。
    #[test]
    fn legacy_instance_status_json_without_new_fields_decodes() {
        let legacy = r#"{
            "name": "qq-default", "display_name": "QQ / OneBot",
            "connected": true, "running": true, "self_id": null,
            "bind_address": "127.0.0.1:3100", "started_at": null
        }"#;
        let status: InstanceStatus = serde_json::from_str(legacy).unwrap();
        assert!(status.platform.is_none());
        assert!(status.ui.is_none());
        assert!(status.session_labels.is_none());
        let value = serde_json::to_value(&status).unwrap();
        assert!(value.get("platform").is_none(), "None 不序列化");
        assert!(value.get("ui").is_none());
        assert!(value.get("session_labels").is_none());
    }
}

#[cfg(test)]
mod session_history_tests {
    use super::*;

    /// `SessionHistory` 线格式（2026-10 会话显示历史通道）：序列化含
    /// messages / total / max_seq，可完整往返（team_id / max_seq 缺省可解）。
    #[test]
    fn session_history_serde_roundtrip() {
        let event = BackendEvent::SessionHistory {
            team_id: Some("t".into()),
            session_id: "s".into(),
            messages: vec![TimelineMessage::user("你好", "s", 1700000000, None, vec![])],
            total: 42,
            truncated: true,
            max_seq: 7,
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value["SessionHistory"]["session_id"],
            serde_json::json!("s")
        );
        assert_eq!(value["SessionHistory"]["team_id"], serde_json::json!("t"));
        assert_eq!(value["SessionHistory"]["total"], serde_json::json!(42));
        assert_eq!(
            value["SessionHistory"]["truncated"],
            serde_json::json!(true)
        );
        assert_eq!(value["SessionHistory"]["max_seq"], serde_json::json!(7));
        assert!(value["SessionHistory"]["messages"].is_array());
        match serde_json::from_value::<BackendEvent>(value).unwrap() {
            BackendEvent::SessionHistory {
                team_id,
                session_id,
                messages,
                total,
                truncated,
                max_seq,
            } => {
                assert_eq!(team_id.as_deref(), Some("t"));
                assert_eq!(session_id, "s");
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].kind, "user");
                assert_eq!(total, 42);
                assert!(truncated);
                assert_eq!(max_seq, 7);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// 旧版/最小形态：team_id 与 max_seq 缺失时解码为 None / 0。
    #[test]
    fn session_history_defaults_decode() {
        let bare: BackendEvent = serde_json::from_str(
            r#"{"SessionHistory":{"session_id":"s","messages":[],"total":0,"truncated":false}}"#,
        )
        .unwrap();
        match bare {
            BackendEvent::SessionHistory {
                team_id, max_seq, ..
            } => {
                assert!(team_id.is_none());
                assert_eq!(max_seq, 0);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// `kind_name()` 必须与 serde 的线上 tag 逐字相同。
    ///
    /// 为什么值得一条测试：插件按 `kind_name()` 的名字订阅，而面板与子进程
    /// 插件按 serde 的 tag 匹配——两者一旦分叉，同一条事件在两条通路上会被
    /// 认成两件事，且**没有任何一方报错**。这里用手头几个代表变体做交叉校验
    /// （含一个流式变体、一个带 Option 字段的、一个新加的）。
    #[test]
    fn kind_name_matches_the_serde_tag() {
        let samples = [
            BackendEvent::AgentContentDelta {
                session_id: "s".into(),
                team_id: None,
                branch_id: String::new(),
                delta: "x".into(),
            },
            BackendEvent::Error {
                session_id: None,
                message: "boom".into(),
            },
            BackendEvent::PluginEvent {
                plugin: "p".into(),
                name: "x".into(),
                payload: serde_json::Value::Null,
                at_ms: 1,
            },
            BackendEvent::PluginOpResult {
                id: "p".into(),
                op: "restart".into(),
                ok: true,
                message: "ok".into(),
            },
        ];
        for event in &samples {
            let value = serde_json::to_value(event).expect("serialize");
            let tag = value
                .as_object()
                .and_then(|m| m.keys().next())
                .expect("externally tagged enum: 顶层只有一个键");
            assert_eq!(
                tag,
                event.kind_name(),
                "kind_name 与 serde tag 分叉（订阅侧会收不到）"
            );
        }
    }
}
