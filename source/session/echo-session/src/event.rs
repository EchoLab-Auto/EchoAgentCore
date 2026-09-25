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
    /// A compaction replaced a prefix of the log with a summary.
    Compaction(CompactionEvent),
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
    /// Adapter/platform provenance (optional; display-only).
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
    pub adapter_name: String,
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

/// A tool call executed by the harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallEvent {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// 归属会话 id（多会话上下文投影分区；None = 旧版事件）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
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
            SessionEvent::Compaction(_) => "session/compaction",
        }
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
        };
        let message = tool_result_message(&event);
        assert_eq!(message.images, vec![uri]);
        assert!(message.content.contains("[图片#1]"));
        assert!(!message.content.contains("BBBB"));
    }

    #[test]
    fn event_roundtrip_preserves_tool_calls() {
        let event = SessionEvent::AssistantMessage(AssistantMessage {
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
                }),
                "tool/call",
            ),
            (
                SessionEvent::ToolResult(ToolResultEvent {
                    session: None,
                    tool_call_id: "c".into(),
                    result: "r".into(),
                    images: vec![],
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
