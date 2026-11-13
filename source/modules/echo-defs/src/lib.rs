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
//! | [`skill`] | the skill vocabulary: `Skill`, `SkillMetadata` |
//! | [`chat`] | chat vocabulary: platform-agnostic message/target/channel types |
//! | [`instance`] | the instance seam: `ModuleInstance` trait, `InstanceInfo`/`InstanceError`, `InstanceManagement` |
//! | [`module_id`] | module-id vocabulary: instance namespace prefixes + `canonical_module_id` |
//! | [`remote_tool`] | remote tool naming: `<peer>__<tool>` construction + prefix recovery |
//! | [`mode`] | shared policy enums (`ThinkingMode`, `ReasoningEffort`) |
//! | [`token`] | pure token-estimation/truncation helpers |
//! | [`media`] | multimodal payload hygiene: embedded base64 → placeholder text |
//! | [`sanitize`] | the sensitive-data seam: `Redactor` trait (scan / redact / register) |
//! # Dependency rules
//!
//! - This crate depends on nothing from the harness (no echo-* crates).
//! - It is deliberately transport-agnostic: `LlmError` carries strings, never
//!   an HTTP client error; providers map their own errors into it.
//! - Everything here is a definition: no filesystem, no network, no state.

pub mod api_profile;
pub mod chat;
pub mod control_command;
pub mod disposer;
pub mod execute;
pub mod input_marker;
pub mod llm;
pub mod media;
pub mod media_store;
pub mod message;
pub mod mode;
pub mod module_id;
pub mod module_instance;
pub mod node;
pub mod pricing;
pub mod prompt;
pub mod registry;
pub mod remote_tool;
pub mod sanitize;
pub mod session_query;
pub mod skill;
pub mod subagent;
pub mod token;
pub mod tool;

pub use chat::{ChannelType, IncomingFile, IncomingMessage};
pub use llm::{LlmError, LlmProvider};
pub use media::{compact_embedded_media, elide_inline_data_uris, image_placeholder};
pub use message::{
    ChatChunk, ChatMessage, ChatRequest, ChatResponse, ChatRole, ChatStreamAccumulator, ToolCall,
    ToolCallDelta, Usage,
};
pub use mode::{ReasoningEffort, ThinkingMode};
pub use module_instance::{
    InstanceConnectionState, InstanceError, InstanceInfo, InstanceManagement, ModuleInstance,
    ToolSpec,
};
pub use node::NodeId;
pub use token::{
    estimate_history_tokens, estimate_image_tokens, estimate_message_tokens, estimate_tokens,
    truncate, truncate_message_to_tokens, truncate_text_to_tokens,
};
pub use tool::{Tool, ToolDefinition, ToolError};

/// 模块 id：本 crate 在 `catalog` 里的身份。
///
/// **为什么 id 在这里，而 manifest 在宿主**：本 crate 的铁律是「零 `echo-*`
/// 依赖」——manifest 类型来自框架仓的 `echo-module`，引进来就破了这条线。
/// 所以 id 归本 crate（配置与 Panel 读的就是这个字面量），manifest 由组合根
/// 用 `push_builtin_identity` 声明（与 `echo-agent.subagent` 同款）。
///
/// 2026-11 之前本 crate 没有身份——它是"只是库"的中间层。取消那个二分之后，
/// 每个 crate 都以模块形态出现在 Panel 里，包括只有 trait 与纯函数的它。
/// 模块声明文件——本 crate 零 `echo-*` 依赖，因此只暴露字符串，
/// 由组合根解析成 manifest。
pub const MANIFEST_TOML: &str = include_str!("../module.toml");

pub const MODULE_ID: &str = "echo-agent.kernel.defs";
