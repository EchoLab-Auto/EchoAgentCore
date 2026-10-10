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

/// 一轮内允许的截断自动续跑次数上限：输出被 max_tokens 截断时把残片
/// 入栈让模型接着写；超过上限按错误上报，不再静默重试。
///
/// **口径单源**（2026-10 巡检）：内置循环（echo-agent）与 echo-loop 曾各
/// 持一份（已漂移）——统一在此定义，两处引用同一常量。
pub const MAX_TRUNCATION_CONTINUES: usize = 4;

/// 截断续跑时喂给模型的提示（user 角色，区别于真实用户输入）。
pub const TRUNCATION_CONTINUE_PROMPT: &str = "[system notice] Your previous output was cut off by the max token limit before the turn was complete. Continue exactly from where you stopped. If you were composing a tool call, discard the partial call and re-issue it in full.";

/// Abstraction over a chat-completion backend.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Provider display name, e.g. "openai".
    fn name(&self) -> &str;

    /// Default model for this provider.
    fn default_model(&self) -> &str;

    /// Complete a chat without streaming.
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError>;

    /// Stream a chat response, forwarding deltas into `tx` **and returning
    /// the assembled full response** once the stream ends.
    ///
    /// - `tx`：增量出口（正文/推理/工具调用按到达顺序）。**本层不做节流
    ///   合并**——消费方负责按窗口合并后转投 UI。
    /// - 返回值与 [`Self::chat`] 同型：调用方无需改动装配即可在
    ///   流式/非流式间切换（usage / stop_reason / tool_calls 由 provider
    ///   内部经 [`crate::message::ChatStreamAccumulator`] 累积）。
    /// - 流中途出错返回 `Err`；已送入 `tx` 的增量由消费方自行处置
    ///   （本层不重发、不回滚）。
    /// - 常规终止（`[DONE]` / 流自然关闭）按 `Ok` 返回，与 `chat` 的
    ///   "成功响应"口径一致。
    async fn chat_stream(
        &self,
        request: &ChatRequest,
        tx: mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError>;
}
