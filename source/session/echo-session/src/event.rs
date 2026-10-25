//! Durable session events: the append-only facts of a session log.

use serde::{Deserialize, Serialize};

use echo_defs::media::compact_embedded_media;
use echo_defs::message::{ChatMessage, ToolCall};

/// One durable fact in a session log.
///
/// Every variant is model-visible or a structural marker (compaction). The
/// serde representation is the on-disk format: fields must stay stable, new
/// fields need `#[serde(default)]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// A user message entered the session (from any platform/backend).
    UserMessage(UserMessage),
    /// An assistant message was produced (may carry tool calls).
    AssistantMessage(AssistantMessage),
    /// A tool call was executed; the result is a model-visible fact.
    ToolResult(ToolResultEvent),
    /// A tool call was requested by the assistant (part of the assistant
    /// message's `tool_calls`; kept as its own event for tool-loop clarity).
    ToolCall(ToolCallEvent),
    /// 一次推理（模型 thinking 正文）。**模型不可见**：投影为上下文时跳过
    /// （见 `derive.rs`）；只用于显示层（历史回放的头像/时序）。
    ///
    /// 为什么进事件日志（2026-10 根治）：显示历史原先按"事件日志派生"回填，
    /// 而推理从不落日志 → 派生结果里没有推理 → 前端只能靠实时 timeline
    /// 窗口补，导致「刷新后思考全堆到正文后面」这类时序错乱无法根除。
    /// 现在推理与工具调用在同一份日志里，派生与实时投影形状一致。
    Reasoning(ReasoningEvent),
    /// 一次模型往返（step）的观测事实。**模型不可见**：投影为上下文时跳过
    /// （见 `derive.rs`）；不进显示时间线（见 `history.rs`）；不进压缩转录。
    ///
    /// 为什么进事件日志（2026-10-19 审计缺口）：此前日志里只有逐个
    /// `ToolCall` 事件，没有"这一步模型一次性请求了几个工具"的事实，
    /// 审计只能用相邻 started_at_ms 间隔 <1s 去猜同轮打包，低估并行度、
    /// 也看不到被取消而未执行的调用。本事件把 step_index / 模型 / token /
    /// 请求→响应的耗时 / 本步工具调用 id 一次性固定下来。
    ModelStep(ModelStepEvent),
    /// A compaction replaced a prefix of the log with a summary.
    Compaction(CompactionEvent),
}

/// 一次推理（thinking 正文）。
///
/// `timestamp` 记录进入时间线的时刻（Unix 秒），显示历史用它排序并和实时
/// 条目对齐；`#[serde(default)]` 让缺失时间的旧数据落到 carry-forward。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningEvent {
    pub content: String,
    /// Unix seconds（模型产出该段推理的时刻）。
    #[serde(default)]
    pub timestamp: i64,
    /// 归属会话 id（多会话上下文投影分区；None = 旧版/未归因事件）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// A user message entered the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: String,
    /// Unix seconds (platform timestamp).
    pub timestamp: i64,
    /// Monotonic sequence assigned by the Core process; concurrent branches
    /// use it to merge replies back into the log in request order.
    #[serde(default)]
    pub message_sequence: Option<u64>,
    /// PluginInstance/platform provenance (optional; display-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<MessageSource>,
    /// Multimodal media attached to the message (image URLs / data URIs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// 归属会话 id（多会话上下文，2026-09）：投影按该字段分区——
    /// 每个来源（本地/QQ 私聊/群）有独立的模型上下文。
    /// None = 旧版事件（加载期归因迁移）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// Source provenance of an inbound message (display/routing only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSource {
    pub instance_name: String,
    pub platform: String,
    pub user_id: String,
    pub user_name: String,
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_name: Option<String>,
    #[serde(default)]
    pub received_at_ms: i64,
    #[serde(default)]
    pub message_sequence: u64,
}

/// An assistant message produced by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: String,
    /// 这条回答**落盘的时刻**（Unix 秒；0 = 旧数据未知）。
    ///
    /// 为什么需要（2026-10 显示顺序根治）：显示历史（`history.rs`）原先给
    /// backend 条目用 carry-forward 时间——沿用最近一条带时间的事件，通常
    /// 就是上一个工具的 `started_at_ms`；而实时投影（`timeline.rs`）给同一个
    /// 回答的是 `Utc::now()`。两条路径的时间因此能差**数十分钟**，而面板要
    /// 拿 `(role, time)` 把"派生历史"与"实时副本"认成同一条：时间不等就认
    /// 不出，表现为同一条回答显示两次、其中一份被插到列表靠后位置。
    ///
    /// 本字段把口径统一（落盘时刻 ≈ 实时投影时刻）。
    /// `#[serde(default)]` 让旧日志（字段缺失）落到 carry-forward，行为不变。
    #[serde(default)]
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Tool calls requested alongside this message; serialized fully (the
    /// pre-event-sourced format dropped these).
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// 归属会话 id（多会话上下文投影分区；None = 旧版事件）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// 一次模型往返（step）的观测事实。
///
/// 全部字段带 `#[serde(default)]`：新事件类型只在 2026-10-19 之后的日志里
/// 出现，旧日志不会解析它；将来加字段也不破坏已落盘数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStepEvent {
    /// 本 turn 内的第几步（0 起；与 echo-loop `StepStart.step_index` 同口径，
    /// 但由 harness 自增——`StepStart` 没有 turn_id，并发分支无法归因）。
    #[serde(default)]
    pub step_index: usize,
    /// 实际使用的模型名。
    #[serde(default)]
    pub model: String,
    /// 本步请求的 prompt token（provider usage）。
    #[serde(default)]
    pub prompt_tokens: u32,
    /// 本步响应生成的 completion token。
    #[serde(default)]
    pub completion_tokens: u32,
    /// 本步响应携带的工具调用 id（顺序 = 模型给出的顺序；空 = 纯文本
    /// 收尾步）。执行细节（name/arguments/耗时）由后续 `ToolCall`/
    /// `ToolResult` 事件补充，这里只做**步骤分组**。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_call_ids: Vec<String>,
    /// 调用方标识（分支 id）：同一会话并发分支时区分事件归属。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    /// 请求发出 → 响应到达的耗时（毫秒，含网络与排队）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// 事件落库时刻（epoch 毫秒），显示/排序用。
    #[serde(default)]
    pub started_at_ms: i64,
    /// 归属会话 id（多会话上下文投影分区；None = 旧版/未归因事件）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// A tool call executed by the harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallEvent {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// 归属会话 id（多会话上下文投影分区；None = 旧版事件）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// 工具开始执行的 epoch 毫秒时间戳（面板「运行中实时计时」用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
}

/// The result of a tool call, fed back to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultEvent {
    /// The tool call id this answers.
    pub tool_call_id: String,
    /// The model-visible result text (may start with "error:").
    pub result: String,
    /// Multimodal media produced by the tool (image URLs / data URIs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// 归属会话 id（多会话上下文投影分区；None = 旧版事件）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// 工具执行总耗时（毫秒；中断场景 = 开始到中断判定为止）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// A compaction replaced a prefix of the log with a summary.
///
/// `summary` carries the **body only** — no `[历史摘要]` marker. The marker
/// is added once at projection time ([`crate::derive::project_messages`]),
/// so summaries do not stack prefixes (the pre-2026-09 writer embedded its
/// own marker and every projection doubled it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionEvent {
    /// Number of leading events replaced by this summary.
    pub replaced_count: usize,
    /// The summary body that stands in for the replaced events.
    pub summary: String,
    /// Path of the archive snapshot taken **before** the compaction (if any).
    /// Display/diagnostics only: lets operators recover the original events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<String>,
    /// 归属会话 id（多会话下按会话分别压缩；None = 旧版全局压缩）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// The marker prefixed to every compaction summary at projection time.
pub const COMPACTION_MARKER: &str = "[历史摘要]";

/// Render a compaction summary exactly once: legacy summaries that already
/// embed the marker (written before the 2026-09 split) pass through, others
/// gain it here.
pub fn render_compaction_summary(summary: &str) -> String {
    let trimmed = summary.trim_start();
    if trimmed.starts_with(COMPACTION_MARKER) {
        trimmed.to_string()
    } else {
        format!("{COMPACTION_MARKER} {trimmed}")
    }
}

impl SessionEvent {
    /// 事件的归属会话 id（多会话上下文投影分区；None = 旧版/未归因事件）。
    pub fn session(&self) -> Option<&str> {
        match self {
            SessionEvent::UserMessage(event) => event.session.as_deref(),
            SessionEvent::AssistantMessage(event) => event.session.as_deref(),
            SessionEvent::ToolResult(event) => event.session.as_deref(),
            SessionEvent::ToolCall(event) => event.session.as_deref(),
            SessionEvent::Reasoning(event) => event.session.as_deref(),
            SessionEvent::ModelStep(event) => event.session.as_deref(),
            SessionEvent::Compaction(event) => event.session.as_deref(),
        }
    }

    /// Mutable access to the attribution field (migration).
    pub fn session_mut(&mut self) -> &mut Option<String> {
        match self {
            SessionEvent::UserMessage(event) => &mut event.session,
            SessionEvent::AssistantMessage(event) => &mut event.session,
            SessionEvent::ToolResult(event) => &mut event.session,
            SessionEvent::ToolCall(event) => &mut event.session,
            SessionEvent::Reasoning(event) => &mut event.session,
            SessionEvent::ModelStep(event) => &mut event.session,
            SessionEvent::Compaction(event) => &mut event.session,
        }
    }

    /// Stable event kind for wire/persistence tagging.
    pub fn kind(&self) -> &'static str {
        match self {
            SessionEvent::UserMessage(_) => "user/message",
            SessionEvent::AssistantMessage(_) => "assistant/message",
            SessionEvent::ToolCall(_) => "tool/call",
            SessionEvent::ToolResult(_) => "tool/result",
            SessionEvent::Reasoning(_) => "assistant/reasoning",
            SessionEvent::ModelStep(_) => "step/model",
            SessionEvent::Compaction(_) => "session/compaction",
        }
    }
}

impl SessionEvent {
    /// 该事件是否改变**模型可见投影**（`derive::project_messages`）。
    ///
    /// `false` 的事件（推理 / 模型往返观测）只服务显示与审计：追加时不必
    /// 重建整份会话投影——长会话里那是 O(事件数) 的字符串克隆，实测每次
    /// 工具调用都要重付一遍，推理与 step 事件再各加一份纯属浪费。
    pub fn affects_projection(&self) -> bool {
        !matches!(
            self,
            SessionEvent::Reasoning(_) | SessionEvent::ModelStep(_)
        )
    }
}

impl echo_defs::SessionEvent for SessionEvent {
    fn kind(&self) -> &'static str {
        self.kind()
    }
}

impl From<UserMessage> for SessionEvent {
    fn from(value: UserMessage) -> Self {
        SessionEvent::UserMessage(value)
    }
}
impl From<AssistantMessage> for SessionEvent {
    fn from(value: AssistantMessage) -> Self {
        SessionEvent::AssistantMessage(value)
    }
}
impl From<ToolCallEvent> for SessionEvent {
    fn from(value: ToolCallEvent) -> Self {
        SessionEvent::ToolCall(value)
    }
}
impl From<ToolResultEvent> for SessionEvent {
    fn from(value: ToolResultEvent) -> Self {
        SessionEvent::ToolResult(value)
    }
}
impl From<ReasoningEvent> for SessionEvent {
    fn from(value: ReasoningEvent) -> Self {
        SessionEvent::Reasoning(value)
    }
}
impl From<CompactionEvent> for SessionEvent {
    fn from(value: CompactionEvent) -> Self {
        SessionEvent::Compaction(value)
    }
}

/// Build the model-facing message for a user event.
///
/// 文本里的内嵌图片数据（hook JSON 的 `"images"` 字段、平台带过来的 data
/// URI）先压成 `[图片#n]` 占位符再进模型：图片本身走 `images` 的 image 块，
/// 文本里再留一份 base64 会被端点按文本 token 计费（实测约 0.7–1 token/字符，
/// 一份 1.3MB 截图 ≈ 125 万 token）——这正是 1M 窗口被单张图片打爆的原因。
pub(crate) fn user_message(event: &UserMessage) -> ChatMessage {
    let content = compact_embedded_media(&event.content, &event.images);
    if event.images.is_empty() {
        ChatMessage::user(&content)
    } else {
        ChatMessage::user_with_images(&content, event.images.clone())
    }
}

/// Build the model-facing assistant message, preserving tool calls.
pub(crate) fn assistant_message(event: &AssistantMessage) -> ChatMessage {
    let mut message =
        ChatMessage::assistant_with_reasoning(&event.content, event.reasoning_content.clone());
    if !event.tool_calls.is_empty() {
        message.tool_calls = Some(event.tool_calls.clone());
    }
    message
}

/// Build the model-facing tool message answering a call id.
///
/// 工具输出里的内嵌图片同样按多模态约定处理：文本只留占位，图片走 image 块。
pub(crate) fn tool_result_message(event: &ToolResultEvent) -> ChatMessage {
    let result = compact_embedded_media(&event.result, &event.images);
    if event.images.is_empty() {
        ChatMessage::tool(&result, &event.tool_call_id)
    } else {
        ChatMessage::tool_with_images(&result, &event.tool_call_id, event.images.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **落盘兼容**：旧日志里的 assistant 事件没有 `timestamp` 字段，必须
    /// 仍能解析（缺省 0 = 未知 → 派生退回 carry-forward，行为与改前一致）。
    #[test]
    fn assistant_message_without_timestamp_deserializes_as_zero() {
        let raw = r#"{"kind":"assistant_message","content":"旧日志","tool_calls":[]}"#;
        let event: SessionEvent = serde_json::from_str(raw).expect("旧格式必须可解析");
        match event {
            SessionEvent::AssistantMessage(a) => {
                assert_eq!(a.timestamp, 0);
                assert_eq!(a.content, "旧日志");
            }
            other => panic!("解析成了别的变体：{other:?}"),
        }
    }

    #[test]
    fn user_message_keeps_image_blocks_but_strips_base64_from_text() {
        let payload = "A".repeat(4096);
        let uri = format!("data:image/png;base64,{payload}");
        let content = format!(
            "<backend_message_hook>\n{{\"content\":\"看图\",\"images\":[\"{uri}\"]}}\n</backend_message_hook>"
        );
        let event = UserMessage {
            content,
            timestamp: 0,
            message_sequence: Some(7),
            source: None,
            images: vec![uri.clone()],
            session: None,
        };
        let message = user_message(&event);
        assert_eq!(message.images, vec![uri], "image block keeps the payload");
        assert!(message.content.contains("[图片#1]"), "{}", message.content);
        assert!(
            !message.content.contains("AAAA"),
            "base64 no longer in text"
        );
        // 模型面对的最大单条消息现在远小于编码前。
        assert!(
            echo_defs::token::estimate_message_tokens(&message) < 1000,
            "compacted message stays small"
        );
    }

    #[test]
    fn tool_result_message_strips_embedded_payload_from_text() {
        let payload = "B".repeat(2048);
        let uri = format!("data:image/jpeg;base64,{payload}");
        let event = ToolResultEvent {
            session: None,
            tool_call_id: "call_1".into(),
            result: format!("screenshot: {uri}"),
            images: vec![uri.clone()],
            elapsed_ms: None,
        };
        let message = tool_result_message(&event);
        assert_eq!(message.images, vec![uri]);
        assert!(message.content.contains("[图片#1]"));
        assert!(!message.content.contains("BBBB"));
    }

    #[test]
    fn event_roundtrip_preserves_tool_calls() {
        let event = SessionEvent::AssistantMessage(AssistantMessage {
            timestamp: 0,
            session: None,
            content: String::new(),
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "send_private_msg".into(),
                arguments: r#"{"user_id":123,"content":"hi"}"#.into(),
            }],
        });
        let json = serde_json::to_string(&event).unwrap();
        let back: SessionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event, "tool_calls survive the wire format");
    }

    #[test]
    fn all_kinds_tag_their_kind() {
        let cases = [
            (
                SessionEvent::UserMessage(UserMessage {
                    session: None,
                    content: "hi".into(),
                    timestamp: 0,
                    message_sequence: None,
                    source: None,
                    images: vec![],
                }),
                "user/message",
            ),
            (
                SessionEvent::AssistantMessage(AssistantMessage {
                    timestamp: 0,
                    session: None,
                    content: "ok".into(),
                    reasoning_content: None,
                    tool_calls: vec![],
                }),
                "assistant/message",
            ),
            (
                SessionEvent::ToolCall(ToolCallEvent {
                    session: None,
                    id: "c".into(),
                    name: "t".into(),
                    arguments: "{}".into(),
                    started_at_ms: None,
                }),
                "tool/call",
            ),
            (
                SessionEvent::ToolResult(ToolResultEvent {
                    session: None,
                    tool_call_id: "c".into(),
                    result: "r".into(),
                    images: vec![],
                    elapsed_ms: None,
                }),
                "tool/result",
            ),
            (
                SessionEvent::Compaction(CompactionEvent {
                    session: None,
                    replaced_count: 2,
                    summary: "s".into(),
                    archive: None,
                }),
                "session/compaction",
            ),
        ];
        for (event, kind) in cases {
            assert_eq!(event.kind(), kind);
        }
    }

    #[test]
    fn compaction_summary_renders_marker_exactly_once() {
        // New-style (body only) gains the marker.
        assert_eq!(
            render_compaction_summary("已压缩 10 条历史事件"),
            "[历史摘要] 已压缩 10 条历史事件"
        );
        // Legacy summaries already carry it — pass through, never double.
        assert_eq!(
            render_compaction_summary("[历史摘要] 已压缩 10 条历史事件"),
            "[历史摘要] 已压缩 10 条历史事件"
        );
        // Leading whitespace does not defeat the check.
        assert_eq!(
            render_compaction_summary("\n[历史摘要] 旧格式"),
            "[历史摘要] 旧格式"
        );
    }

    #[test]
    fn compaction_event_roundtrips_with_and_without_archive() {
        // The archive field is optional on the wire: pre-2026-09 files lack
        // it, and new ones may too (archive step failed).
        let legacy: CompactionEvent =
            serde_json::from_str(r#"{"replaced_count":3,"summary":"旧摘要","session":"s"}"#)
                .expect("legacy payload without archive must parse");
        assert_eq!(legacy.archive, None);
        assert_eq!(legacy.replaced_count, 3);

        let event = CompactionEvent {
            replaced_count: 3,
            summary: "新摘要".into(),
            archive: Some("/tmp/archives/x-precompact.json".into()),
            session: Some("s".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("precompact"));
        let back: CompactionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);

        // Absent archive is skipped entirely, keeping old files byte-stable.
        let bare = CompactionEvent {
            archive: None,
            ..event
        };
        assert!(!serde_json::to_string(&bare).unwrap().contains("archive"));
    }
}
