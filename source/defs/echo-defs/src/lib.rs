//! EchoAgentCore Service Definition layer.
//!
//! This crate is the **contract** of every swappable capability in the
//! harness — vocabulary types and traits with zero implementation. Providers
//! (LLM backends, chat platforms, tool executors, skill sources) implement
//! these traits; consumers (the agent loop, tools, frontends) depend only on
//! this crate, never on a concrete provider.
//!
//! # Crate layout
//!
//! | module | owns |
//! |---|---|
//! | [`message`] | chat-completion vocabulary: `ChatRole`, `ChatMessage`, `ToolCall`, `ChatRequest`, ... |
//! | [`llm`] | the `LlmProvider` seam: trait + transport-agnostic `LlmError` |
//! | [`tool`] | the `Tool` seam: trait, `ToolError`, `ToolDefinition` |
//! | [`skill`] | the skill seam: `Skill`, `SkillMetadata`, `SkillProvider` trait |
//! | [`chat`] | the chat-platform seam: platform-agnostic message/target types + `ChatAdapter` trait |
//! | [`mode`] | shared policy enums (`GateMode`, `ThinkingMode`, `ReasoningEffort`) |
//! | [`token`] | pure token-estimation/truncation helpers |
//! | [`session`] | the event-sourced session seam: `SessionEvent` + `SessionStore` traits |
//!
//! # Dependency rules
//!
//! - This crate depends on nothing from the harness (no echo-* crates).
//! - It is deliberately transport-agnostic: `LlmError` carries strings, never
//!   an HTTP client error; providers map their own errors into it.
//! - Everything here is a definition: no filesystem, no network, no state.

pub mod chat;
pub mod llm;
pub mod message;
pub mod mode;
pub mod session;
pub mod skill;
pub mod token;
pub mod tool;

pub use chat::{
    AdapterEvent, ChannelType, ChatAdapter, IncomingMessage, MessageTarget, SendResult,
};
pub use llm::{LlmError, LlmProvider};
pub use message::{
    ChatChunk, ChatMessage, ChatRequest, ChatResponse, ChatRole, ToolCall, ToolCallDelta, Usage,
};
pub use mode::{GateMode, ReasoningEffort, ThinkingMode};
pub use session::{SessionEvent, SessionStore};
pub use skill::{Skill, SkillMetadata, SkillProvider};
pub use token::{
    estimate_history_tokens, estimate_message_tokens, estimate_tokens, truncate,
    truncate_message_to_tokens, truncate_text_to_tokens,
};
pub use tool::{Tool, ToolDefinition, ToolError};
