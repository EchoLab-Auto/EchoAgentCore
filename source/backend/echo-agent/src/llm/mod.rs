//! LLM provider abstraction.
//!
//! A [`LlmProvider`] wraps any chat-completion backend (OpenAI-compatible,
//! Anthropic, Ollama, ...) behind one trait with a streaming variant.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

pub mod anthropic;
pub mod ollama;
pub mod openai;

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
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
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
        }
    }
    pub fn tool(content: impl Into<String>, tool_call_id: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Tool,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// Tool definition sent to the LLM (OpenAI function-calling format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// A tool call requested by the LLM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP 请求失败: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API 返回错误: {0}")]
    Api(String),
    #[error("响应解析失败: {0}")]
    Parse(String),
    #[error("provider 配置错误: {0}")]
    Config(String),
    #[error("流式传输中断")]
    StreamClosed,
}

/// Abstraction over a chat-completion backend.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Provider display name, e.g. "openai".
    fn name(&self) -> &str;

    /// Default model for this provider.
    fn default_model(&self) -> &str;

    /// Complete a chat without streaming.
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError>;

    /// Stream a chat response, sending chunks into `tx`. The stream ends when
    /// the channel is dropped by the consumer or the provider finishes.
    async fn chat_stream(
        &self,
        request: &ChatRequest,
        tx: mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<(), LlmError>;
}

/// Char-safe truncation for error messages (avoids panicking mid-UTF-8).
pub(crate) fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// Whether a character is a CJK character (conservative superset: Han
/// ideographs, radicals, kana, CJK punctuation, full-width forms).
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x2E80..=0x303F   // radicals + CJK punctuation
        | 0x3040..=0x30FF // kana
        | 0x31C0..=0x31EF // CJK strokes
        | 0x3400..=0x9FFF // unified ideographs
        | 0xF900..=0xFAFF // compatibility ideographs
        | 0xFE30..=0xFE4F // compatibility forms
        | 0xFF00..=0xFFEF // full-width forms
        | 0x20000..=0x2FA1F // extension A–F
    )
}

/// Estimate the number of tokens a text consumes.
///
/// Conservative (deliberately over-estimates) so that token-budget trimming
/// never leaves the context over the limit:
/// - CJK characters count as 1 token each (real models average ~0.6–1).
/// - Everything else counts as 1 token per 3 characters (English is ~1/4).
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(3)
}

/// Estimate the tokens of one chat message, including the fixed per-message
/// structural overhead (role label, separators) charged by the APIs.
pub fn estimate_message_tokens(message: &ChatMessage) -> usize {
    estimate_tokens(&message.content)
        + message
            .reasoning_content
            .as_deref()
            .map(estimate_tokens)
            .unwrap_or_default()
        + 4
}

/// Estimate the total tokens of a history slice.
pub fn estimate_history_tokens(history: &[ChatMessage]) -> usize {
    history.iter().map(estimate_message_tokens).sum()
}

/// Truncate `text` so its estimated token count stays at or below `budget`.
///
/// Keeps the head of the text (structured inputs carry their sequence at the
/// start) and appends a truncation marker. When the original already fits, the
/// original is returned unchanged.
pub fn truncate_text_to_tokens(text: &str, budget: usize) -> String {
    const MARKER: &str = "\n…[内容过长已截断]";
    if estimate_tokens(text) <= budget {
        return text.to_string();
    }
    let marker_tokens = estimate_tokens(MARKER);
    let available = budget.saturating_sub(marker_tokens).max(1);
    let mut cost = 0usize;
    let mut non_cjk = 0usize;
    let mut cut = text.len();
    for (index, ch) in text.char_indices() {
        let inc = if is_cjk(ch) {
            1
        } else {
            non_cjk += 1;
            if non_cjk % 3 == 1 {
                1
            } else {
                0
            }
        };
        if cost + inc > available {
            cut = index;
            break;
        }
        cost += inc;
    }
    if cut == 0 {
        // Nothing fits even without the marker — keep a single char.
        return text
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_default();
    }
    format!("{}{}", &text[..cut], MARKER)
}

/// Truncate a single message to fit `budget` estimated tokens (content only).
pub fn truncate_message_to_tokens(message: &mut ChatMessage, budget: usize) {
    let current = estimate_message_tokens(message);
    if current <= budget {
        return;
    }
    // Content budget after accounting for the 4-token structural overhead and
    // any reasoning content.
    let reasoning_budget = message
        .reasoning_content
        .as_deref()
        .map(estimate_tokens)
        .unwrap_or(0);
    let content_budget = budget.saturating_sub(4 + reasoning_budget).max(1);
    message.content = truncate_text_to_tokens(&message.content, content_budget);
}

/// Build a provider from configuration.
///
/// base_url 以 `/anthropic` 结尾时（如 DeepSeek 的 Anthropic 兼容端点
/// `https://api.deepseek.com/anthropic`）自动走 Anthropic Messages 格式客户端，
/// 即使 provider 名为 openai/deepseek。
pub fn create_provider(cfg: &crate::config::AgentConfig) -> Result<Box<dyn LlmProvider>, LlmError> {
    // 空 provider = 未配置 API，按 OpenAI 兼容处理（请求会因无 key 失败，
    // 等用户在 TUI /api 中配置后生效）。
    let provider_kind = if cfg.provider.is_empty() {
        return Err(LlmError::Config(
            "no LLM provider configured — use /api in TUI or set [agent] provider in config".into(),
        ));
    } else {
        cfg.provider.as_str()
    };
    let anthropic_style = cfg.base_url.trim_end_matches('/').ends_with("/anthropic");
    let deepseek_reasoning = (provider_kind == "deepseek"
        || cfg.base_url.contains("api.deepseek.com"))
    .then_some((cfg.thinking, cfg.reasoning_effort));
    match provider_kind {
        "openai" | "deepseek" if anthropic_style => {
            // DeepSeek 的 /anthropic 端点只实现 Messages API（POST /v1/messages）
            let mut provider =
                anthropic::AnthropicProvider::new(&cfg.base_url, &cfg.api_key, &cfg.model);
            if let Some((thinking, effort)) = deepseek_reasoning {
                provider = provider.with_reasoning(thinking, effort);
            }
            Ok(Box::new(provider))
        }
        "openai" | "deepseek" => {
            let mut provider = openai::OpenAiProvider::new(&cfg.base_url, &cfg.api_key, &cfg.model);
            if let Some((thinking, effort)) = deepseek_reasoning {
                provider = provider.with_reasoning(thinking, effort);
            }
            Ok(Box::new(provider))
        }
        "anthropic" | "claude" => Ok(Box::new(anthropic::AnthropicProvider::new(
            &cfg.base_url,
            &cfg.api_key,
            &cfg.model,
        ))),
        "ollama" => Ok(Box::new(ollama::OllamaProvider::new(
            &cfg.base_url,
            &cfg.model,
        ))),
        other => Err(LlmError::Config(format!(
            "不支持的 provider: {other} (可选: openai/deepseek/anthropic/ollama)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;

    fn cfg(provider: &str, base_url: &str) -> AgentConfig {
        AgentConfig {
            provider: provider.into(),
            base_url: base_url.into(),
            model: "test-model".into(),
            api_key: "test-key".into(),
            ..Default::default()
        }
    }

    #[test]
    fn deepseek_anthropic_endpoint_routes_to_anthropic_client() {
        let p = create_provider(&cfg("deepseek", "https://api.deepseek.com/anthropic")).unwrap();
        assert_eq!(p.name(), "anthropic");
    }

    #[test]
    fn deepseek_openai_endpoint_routes_to_openai_client() {
        let p = create_provider(&cfg("deepseek", "https://api.deepseek.com")).unwrap();
        assert_eq!(p.name(), "openai");
        let p = create_provider(&cfg("deepseek", "")).unwrap();
        assert_eq!(p.name(), "openai");
    }

    #[test]
    fn unknown_provider_is_config_error() {
        assert!(create_provider(&cfg("gpt-x", "")).is_err());
    }

    // ── Token estimation ───────────────────────────────────────────────────

    #[test]
    fn cjk_chars_count_one_token_each() {
        assert_eq!(estimate_tokens("你好世界"), 4);
        assert_eq!(estimate_tokens("中文标点：，。"), 7);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn ascii_chars_count_one_token_per_three() {
        assert_eq!(estimate_tokens("abcdef"), 2);
        assert_eq!(estimate_tokens("a"), 1, "at least one token");
        assert_eq!(estimate_tokens("abcd"), 2);
    }

    #[test]
    fn mixed_text_is_summed() {
        // 你好 = 2 CJK；" hello world" 11 个非 CJK 字符 → ceil(11/3) = 4。
        assert_eq!(estimate_tokens("你好 hello world"), 6);
    }

    #[test]
    fn message_tokens_include_structure_overhead() {
        let msg = ChatMessage::user("你好");
        assert_eq!(estimate_message_tokens(&msg), 2 + 4);
    }

    #[test]
    fn history_tokens_sum_messages() {
        let history = vec![ChatMessage::user("你好"), ChatMessage::assistant("ok")];
        assert_eq!(estimate_history_tokens(&history), (2 + 4) + (1 + 4));
    }

    // ── Property tests ─────────────────────────────────────────────────────

    use proptest::prelude::Just;
    use proptest::prop_assert_eq;

    proptest::proptest! {
        /// ChatMessage JSON round-trip for any plausible message.
        #[test]
        fn chat_message_json_roundtrip(
            role in proptest::prop_oneof![
                Just(ChatRole::System),
                Just(ChatRole::User),
                Just(ChatRole::Assistant),
                Just(ChatRole::Tool),
            ],
            content in ".*",
            has_tool_call in proptest::bool::ANY,
            tool_call_id in "[a-z0-9_-]{0,20}",
        ) {
            let tool_calls = if has_tool_call {
                Some(vec![ToolCall {
                    id: "call_1".into(),
                    name: "calculator".into(),
                    arguments: "{\"expr\":\"1+1\"}".into(),
                }])
            } else {
                None
            };
            let msg = ChatMessage {
                role,
                content,
                reasoning_content: None,
                tool_calls,
                tool_call_id: (role == ChatRole::Tool).then_some(tool_call_id),
            };
            let json = serde_json::to_string(&msg).expect("serialize");
            let back: ChatMessage = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(back.role, msg.role);
            prop_assert_eq!(back.content, msg.content);
            prop_assert_eq!(back.tool_calls, msg.tool_calls);
            prop_assert_eq!(back.tool_call_id, msg.tool_call_id);
        }

        /// ToolCall JSON round-trip.
        #[test]
        fn tool_call_json_roundtrip(
            id in "[a-z0-9_-]{1,20}",
            name in "[a-z0-9_]{1,20}",
            arguments in ".*",
        ) {
            let call = ToolCall { id, name, arguments };
            let json = serde_json::to_string(&call).expect("serialize");
            let back: ToolCall = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(back, call);
        }
    }
}
