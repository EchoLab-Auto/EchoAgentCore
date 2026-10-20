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
//! Platform instances are managed through [`echo_defs::plugin_instance::InstanceRegistry`].

/// 本进程的节点身份：federation `NodeId`（`node-<ulid>`）+ 运行区域展示名。
///
/// `SessionInfo` / `TeamInfo` 等线格式携带它们，供多节点聚合客户端区分同名
/// 会话 / 人格（每个 Core 的 `local:tui::local_user`、`default` 都可能相同），
/// 并把「agent 运行在哪个区域」作为一等属性。
///
/// **装配方式（P3 终态）**：组合根在启动时构造**一份**，显式交给
/// [`agent_manager::AgentManager`] 与每个 [`agent::Agent`]——不再经进程级
/// kernel cell 存取，也不再为存放它而泄漏字符串。多节点/多区域场景下
/// 「身份」是构造参数，不是进程全局事实。
///
/// 为什么不再是 `'static`：旧实现把两个 `String` 各 `Box::leak` 一次以塞进
/// 进程级槽位；改成参数后按值持有（`Clone` 开销 = 两次小字符串拷贝），
/// 展示路径（`SessionInfo`/`TeamInfo` 构造）本就每次都要 `String`。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeIdentity {
    /// federation NodeId（空 = 未注入，纯嵌入式用法）。
    pub node_id: String,
    /// 运行区域展示名（空 = 回退 NodeId）。
    pub region_name: String,
}

impl NodeIdentity {
    /// 构造一份身份（`region_name` 可为空串 = 回退 NodeId）。
    pub fn new(node_id: impl Into<String>, region_name: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            region_name: region_name.into(),
        }
    }

    /// 写出线格式用的 `node_id`（空 → `None`，字段不下发）。
    pub fn node_id_field(&self) -> Option<String> {
        (!self.node_id.is_empty()).then(|| self.node_id.clone())
    }

    /// 写出线格式用的 `region_name`（空 → `None`，字段不下发）。
    pub fn region_name_field(&self) -> Option<String> {
        (!self.region_name.is_empty()).then(|| self.region_name.clone())
    }
}

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
pub mod plugins;
pub mod session;
pub mod shell;
pub mod shims_tool;
pub mod timeline;

/// 各「包」（插件）实现的聚合目录（按插件名分目录，2026-09-28 重组；
/// 物理组织，不新增公开路径——见下方兼容 re-export 与模块自身文档）。
mod packages;

// ── 包（插件）实现的兼容 re-export：公开路径保持重组前原样，避免破坏
//    core 组合根与既有引用（`echo_agent::subagent::…` 等）。──
pub use echo_plugin_skills as skill;
pub use echo_plugin_skills::install as skill_install;
pub use echo_plugin_subagent as subagent;
pub use echo_plugin_workspace as workspace;
pub use packages::federation;
pub use packages::instance_ops::bridge as instance_bridge;
pub use shims_tool as tool;

pub use agent::{Agent, EventSink};
pub use agent_manager::AgentManager;
pub use bridge::{create_bridge, BackendBridge, BackendHandle, FanoutHandle};
pub use command::BackendCommand;
pub use config::{AgentConfig, AgentProfile, ReasoningEffort, TeamMember, ThinkingMode};
pub use echo_plugin_skills::{Skill, SkillRegistry};
pub use event::{
    ApiProfileInfo, BackendEvent, BackendState, ContextMessageInfo, SessionInfo, TimelineMessage,
    TimelineSource, TimelineTool,
};
pub use llm::LlmProvider;
pub use packages::instance_ops::bridge::AgentMessageHook;
pub use session::{Session, SessionKey, TrunkStore};
pub use tool::{PluginTool, PluginToolRegistration, Tool, ToolRegistry};
