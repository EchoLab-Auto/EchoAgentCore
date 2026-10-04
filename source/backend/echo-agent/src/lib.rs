//! EchoAgentCore agent framework.
//!
//! Core abstractions: [`LlmProvider`], [`Tool`], [`Skill`], and the [`Agent`]
//! loop that composes them.
//!
//! 物理布局：框架核心在 `agent/` 等顶层模块；各「包」（插件）的实现按插件名
//! 收拢在 `packages/`（见该模块文档的目录 ↔ 插件 id 映射表）。
//!
//! Communicates with the outside world (TUI, API) through the
//! [`BackendBridge`] / [`BackendHandle`] mpsc channel pair.
//!
//! Platform adapters are managed through [`echo_adapter::AdapterRegistry`].

/// 进程级节点身份（federation NodeId，`node-<ulid>`）。组合根启动时注入；
/// `SessionInfo`/`TeamInfo` 等线格式携带它，供多节点聚合客户端区分同名
/// 会话/人格（每个 Core 的 `local:tui::local_user`、`default` 都可能相同）。
static NODE_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// 注入本进程的节点身份（幂等；仅首次生效）。
pub fn set_node_id(id: String) {
    let _ = NODE_ID.set(id);
}

/// 本进程的节点身份（未注入 = None，旧/嵌入式用法）。
pub fn node_id() -> Option<&'static str> {
    NODE_ID.get().map(|s| s.as_str())
}

pub mod agent;
pub mod agent_manager;
pub mod bridge;
pub mod command;
pub mod config;
pub mod event;
pub mod input_marker;
pub mod plugins;
pub mod session;
pub mod shell;
pub mod timeline;

/// 各「包」（插件）实现的聚合目录（按插件名分目录，2026-09-28 重组；
/// 物理组织，不新增公开路径——见下方兼容 re-export 与模块自身文档）。
mod packages;

// ── 包（插件）实现的兼容 re-export：公开路径保持重组前原样，避免破坏
//    core 组合根与既有引用（`echo_agent::subagent::…` 等）。──
pub use packages::adapter_qq::bridge as adapter_bridge;
pub use packages::federation;
pub use packages::provider_llm as llm;
pub use packages::skills_dir as skill;
pub use packages::skills_dir::install as skill_install;
pub use packages::subagent;
pub use packages::tool;
pub use packages::workspace;

pub use agent::{Agent, EventSink};
pub use agent_manager::AgentManager;
pub use bridge::{create_bridge, BackendBridge, BackendHandle, FanoutHandle};
pub use command::BackendCommand;
pub use config::{AgentConfig, AgentProfile, ReasoningEffort, TeamMember, ThinkingMode};
pub use event::{
    ApiProfileInfo, BackendEvent, BackendState, ContextMessageInfo, GroupInfo, SessionInfo,
    TimelineMessage, TimelineSource, TimelineTool,
};
pub use packages::adapter_qq::bridge::AgentMessageHook;
pub use packages::provider_llm::LlmProvider;
pub use packages::skills_dir::{Skill, SkillRegistry};
pub use session::{Session, SessionKey, TrunkStore};
pub use tool::{Tool, ToolRegistry};
