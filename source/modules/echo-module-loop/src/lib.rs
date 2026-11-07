//! EchoAgentCore agent framework.
//!
//! Core abstractions: [`LlmProvider`], [`Tool`], [`Skill`], and the [`Agent`]
//! loop that composes them.
//!
//! 物理布局：框架核心在 `agent/` 等顶层模块；各「包」（模块）的实现按模块名
//! 收拢在 `packages/`（见该模块文档的目录 ↔ 模块 id 映射表）。
//!
//! Communicates with the outside world (TUI, API) through the
//! [`BackendBridge`] / [`BackendHandle`] mpsc channel pair.
//!
//! Platform instances are managed through [`echo_defs::module_instance::InstanceRegistry`].

/// 节点身份词汇已下沉到 `echo-defs`（会话模块也要用它，而模块不能依赖宿主）。
pub use echo_defs::node::NodeIdentity;

// 原通用循环驱动与模块装配：现在与 Agent 业务同 crate 发布。
pub mod compaction;
pub mod loop_event;
pub mod pipeline;
pub mod runner;
pub mod loop_module;

pub use loop_event::{
    AgentPreStep, AgentRequest, ModelResponse, StepEnd, StepStart, ToolCallRequested, ToolResult,
    TurnEnd, TurnStart, TurnStopping,
};
pub use pipeline::{ToolPipeline, ToolPipelineResult};
pub use runner::{ChatExecutor, LoopError, LoopOptions, ToolExecutor, ToolOutcome, TurnRunner};
pub use loop_module::{declarations, modules, SINGLE_MODULE_ID, REMOVED_PARALLEL_MODULE_ID, SERVICE_KEY};


pub mod agent;
pub mod agent_manager;
pub mod api_pool;
pub mod bridge;
pub mod command;
pub mod config;
pub mod event;
pub mod input_marker;
pub mod llm;
pub mod metrics;
/// 模型出口服务（S4a）：provider / 配置 / 模型名 / profile 覆盖的唯一持有者。
pub mod model_service;
pub mod modules;
/// 进程级命令（团队 CRUD）：不属于任何人格，命令泵在路由前先问这里。
/// 进程级命令（团队 CRUD）：不属于任何人格，命令泵在路由前先问这里。
pub mod process_commands;
/// 提示词服务（S4c）：技能表 + 装配缓存 + 按会话的最近快照。
pub mod prompt_service;
pub mod session;
pub mod shell;
pub mod shims_tool;
pub mod timeline;

/// 各「包」（模块）实现的聚合目录（按模块名分目录，2026-09-28 重组；
/// 物理组织，不新增公开路径——见下方兼容 re-export 与模块自身文档）。
mod packages;

// ── 包（模块）实现的兼容 re-export：公开路径保持重组前原样，避免破坏
//    core 组合根与既有引用（`echo_agent::subagent::…` 等）。──
pub use echo_module_skills as skill;
pub use echo_module_skills::install as skill_install;
pub use echo_module_subagent as subagent;
pub use echo_module_workspace as workspace;
pub use packages::federation;
pub use packages::instance_ops::bridge as instance_bridge;
pub use shims_tool as tool;

pub use agent::{Agent, EventSink};
pub use agent_manager::AgentManager;
pub use bridge::{create_bridge, BackendBridge, BackendHandle, FanoutHandle};
pub use command::BackendCommand;
pub use config::{AgentConfig, AgentProfile, ReasoningEffort, TeamMember, ThinkingMode};
pub use echo_module_skills::{Skill, SkillRegistry};
pub use event::{
    ApiProfileInfo, BackendEvent, ContextMessageInfo, SessionInfo, TimelineMessage, TimelineSource,
    TimelineTool,
};
pub use llm::LlmProvider;
pub use packages::instance_ops::bridge::AgentMessageHook;
pub use session::{Session, SessionKey, TrunkStore};
pub use tool::{ModuleTool, ModuleToolRegistration, Tool, ToolRegistry};
