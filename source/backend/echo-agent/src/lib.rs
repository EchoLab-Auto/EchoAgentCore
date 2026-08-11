//! EchoAgentCore agent framework.
//!
//! Core abstractions: [`LlmProvider`](llm::LlmProvider), [`Tool`](tool::Tool),
//! [`Skill`](skill::Skill), and the [`Agent`](agent::Agent) loop that composes
//! them.
//!
//! Communicates with the outside world (TUI, API) through the
//! [`BackendBridge`] / [`BackendHandle`] mpsc channel pair.
//!
//! Platform adapters are managed through [`echo_adapter::AdapterRegistry`].

pub mod adapter_bridge;
pub mod agent;
pub mod bridge;
pub mod command;
pub mod config;
pub mod event;
pub mod llm;
pub mod session;
pub mod skill;
pub mod tool;

pub use adapter_bridge::AgentMessageHook;
pub use agent::Agent;
pub use bridge::{create_bridge, BackendBridge, BackendHandle, FanoutHandle};
pub use command::BackendCommand;
pub use config::{AgentConfig, ReasoningEffort, SelfUpdateConfig, ThinkingMode};
pub use event::{
    ApiProfileInfo, BackendEvent, BackendState, ContextMessageInfo, GroupInfo, SessionInfo,
    TimelineMessage, TimelineSource, TimelineTool,
};
pub use llm::LlmProvider;
pub use session::{Session, SessionKey, TrunkStore};
pub use skill::{Skill, SkillRegistry};
pub use tool::{Tool, ToolRegistry};
