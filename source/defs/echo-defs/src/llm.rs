//! The `LlmProvider` capability seam — Service Definition role.

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::message::{ChatChunk, ChatRequest, ChatResponse};

/// Provider errors, transport-agnostic by design.
///
/// The definition layer must not depend on an HTTP client: providers map
/// their own transport errors into [`LlmError::Http`] as a string.
#[derive(Debug, Error)]
pub enum LlmError {
    /// A transport-level failure (connection, timeout, malformed response).
    #[error("HTTP 请求失败: {0}")]
    Http(String),
    /// The API returned an error payload.
    #[error("API 返回错误: {0}")]
    Api(String),
    /// The response could not be parsed.
    #[error("响应解析失败: {0}")]
    Parse(String),
    /// Provider configuration is invalid.
    #[error("provider 配置错误: {0}")]
    Config(String),
    /// The stream ended before completion.
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
