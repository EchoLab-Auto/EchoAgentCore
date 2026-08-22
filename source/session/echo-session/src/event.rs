//! Durable session events: the append-only facts of a session log.

use serde::{Deserialize, Serialize};

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
    /// pre-event-sourced format dropped these — see Phase 3 ADR).
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

/// A tool call executed by the harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallEvent {
    pub id: String,
    pub name: String,
    pub arguments: String,
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
}

/// A compaction replaced a prefix of the log with a summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionEvent {
    /// Number of leading events replaced by this summary.
    pub replaced_count: usize,
    /// The summary text that stands in for the replaced events.
    pub summary: String,
}

impl SessionEvent {
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
pub(crate) fn user_message(event: &UserMessage) -> ChatMessage {
    if event.images.is_empty() {
        ChatMessage::user(&event.content)
    } else {
        ChatMessage::user_with_images(&event.content, event.images.clone())
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
pub(crate) fn tool_result_message(event: &ToolResultEvent) -> ChatMessage {
    if event.images.is_empty() {
        ChatMessage::tool(&event.result, &event.tool_call_id)
    } else {
        ChatMessage::tool_with_images(&event.result, &event.tool_call_id, event.images.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_roundtrip_preserves_tool_calls() {
        let event = SessionEvent::AssistantMessage(AssistantMessage {
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
                    content: "ok".into(),
                    reasoning_content: None,
                    tool_calls: vec![],
                }),
                "assistant/message",
            ),
            (
                SessionEvent::ToolCall(ToolCallEvent {
                    id: "c".into(),
                    name: "t".into(),
                    arguments: "{}".into(),
                }),
                "tool/call",
            ),
            (
                SessionEvent::ToolResult(ToolResultEvent {
                    tool_call_id: "c".into(),
                    result: "r".into(),
                    images: vec![],
                }),
                "tool/result",
            ),
            (
                SessionEvent::Compaction(CompactionEvent {
                    replaced_count: 2,
                    summary: "s".into(),
                }),
                "session/compaction",
            ),
        ];
        for (event, kind) in cases {
            assert_eq!(event.kind(), kind);
        }
    }
}
