//! Events emitted by the agent backend to subscribers (TUI, API, ...).
//!
//! The definitions live in the standalone [`agent_module_management::protocol`] crate (the
//! frontend–Core wire contract); this module re-exports them so existing
//! `echo_agent::event::…` paths keep working.

pub use agent_module_management::protocol::{
    ApiBalancePoint, ApiMetricsEntry, ApiProfileInfo, ApiUsagePoint, BackendEvent, BrowseRoot,
    ContextBlockInfo, ContextMessageInfo, InstanceMetadataEntry, InstanceStatus, ModuleInfo,
    SessionInfo, ShellSessionInfo, SkillInfo, SkillSourceInfo, TimelineMessage, TimelineSource,
    TimelineTool, ToolInfo, WorkspaceFileEntry, WorkspaceGitInfo, WorkspaceSessionInfo,
};
