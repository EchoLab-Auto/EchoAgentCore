//! Chat-completion vocabulary: messages, tool calls, chunks, usage.

use serde::{Deserialize, Serialize};

use crate::tool::ToolDefinition;

/// Role of a message in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A single message in the conversation sent to the LLM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    /// Provider reasoning that must be returned verbatim during tool loops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Required for `role == Tool`: the id of the tool call being answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Multimodal media: image URLs or `data:` URIs attached to the message.
    ///
    /// When non-empty, providers serialize `content` as a content-part array
    /// instead of a plain string (OpenAI `image_url`, Anthropic `image` blocks).
    /// Keep the plain-text `content` so text-only pipelines are unaffected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
        }
    }

    /// Build a user message with attached images (URLs or data URIs).
    pub fn user_with_images(content: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
        }
    }
    pub fn assistant_with_reasoning(
        content: impl Into<String>,
        reasoning_content: Option<String>,
    ) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            reasoning_content,
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
        }
    }
    pub fn tool(content: impl Into<String>, tool_call_id: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Tool,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            images: vec![],
        }
    }

    /// Build a tool result message carrying attached images (URLs/data URIs).
    pub fn tool_with_images(
        content: impl Into<String>,
        tool_call_id: impl Into<String>,
        images: Vec<String>,
    ) -> Self {
        Self {
            role: ChatRole::Tool,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            images,
        }
    }
}

/// A tool call requested by the LLM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments.
    pub arguments: String,
}

/// Incremental chunk of a tool call during streaming.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// Streaming chunk from the LLM.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatChunk {
    pub content_delta: Option<String>,
    pub reasoning_delta: Option<String>,
    pub tool_call_delta: Option<ToolCallDelta>,
}

/// Token usage reported by the provider.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// A complete (non-streaming) chat response.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

/// A chat completion request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}
