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
//! | [`chat`] | chat vocabulary: platform-agnostic message/target/channel types |
//! | [`instance`] | the instance seam: `PluginInstance` trait, `InstanceInfo`/`InstanceError`, `InstanceManagement` |
//! | [`plugin_id`] | plugin-id vocabulary: instance namespace prefixes + `canonical_plugin_id` |
//! | [`remote_tool`] | remote tool naming: `<peer>__<tool>` construction + prefix recovery |
//! | [`mode`] | shared policy enums (`ThinkingMode`, `ReasoningEffort`, `LoopMode`) |
//! | [`token`] | pure token-estimation/truncation helpers |
//! | [`media`] | multimodal payload hygiene: embedded base64 → placeholder text |
//! | [`session`] | the event-sourced session seam: `SessionEvent` + `SessionStore` traits |
//! | [`sanitize`] | the sensitive-data seam: `Redactor` trait (scan / redact / register) |
//! # Dependency rules
//!
//! - This crate depends on nothing from the harness (no echo-* crates).
//! - It is deliberately transport-agnostic: `LlmError` carries strings, never
//!   an HTTP client error; providers map their own errors into it.
//! - Everything here is a definition: no filesystem, no network, no state.

pub mod api_profile;
pub mod chat;
pub mod input_marker;
pub mod llm;
pub mod media;
pub mod media_store;
pub mod message;
pub mod mode;
pub mod node;
pub mod plugin_id;
pub mod pricing;
pub mod plugin_instance;
pub mod remote_tool;
pub mod sanitize;
pub mod session;
pub mod skill;
pub mod token;
pub mod tool;

pub use chat::{ChannelType, IncomingFile, IncomingMessage};
pub use llm::{LlmError, LlmProvider};
pub use media::{compact_embedded_media, elide_inline_data_uris, image_placeholder};
pub use message::{
    ChatChunk, ChatMessage, ChatRequest, ChatResponse, ChatRole, ChatStreamAccumulator, ToolCall,
    ToolCallDelta, Usage,
};
pub use mode::{LoopMode, ReasoningEffort, ThinkingMode};
pub use node::NodeId;
pub use plugin_instance::{
    InstanceConnectionState, InstanceError, InstanceInfo, InstanceManagement, PluginInstance,
    ToolSpec,
};
pub use session::{SessionEvent, SessionStore};
pub use token::{
    estimate_history_tokens, estimate_image_tokens, estimate_message_tokens, estimate_tokens,
    truncate, truncate_message_to_tokens, truncate_text_to_tokens,
};
pub use tool::{Tool, ToolDefinition, ToolError};
