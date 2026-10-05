//! `Agent` 装配 Builder（框架优化议题 2：装配期/运行期分离）。
//!
//! ## 动机
//!
//! 历史上 `Agent::new` 返回"半装配"对象，随后由组合根顺序调用
//! 13 个 setter/attach 填满槽位——非法状态（没装 plugin_host 的 Agent）
//! 在类型系统里完全合法，装配顺序是只能靠读源码确认的隐性知识，
//! 且 `set_*_now`（装配期专用）与 async 运行期版本共享命名空间。
//!
//! `AgentBuilder` 把"启动期一次性写入、之后不再变"的槽收进一次声明式
//! 装配：`build()` 返回完整装配的 `Agent`。此后：
//!
//! - **必填槽**（[`AgentBuilder::plugin_host`] / [`AgentBuilder::event_sink`]）：
//!   缺失即 bug——`build()` 直接 panic（fail-fast，不静默降级）。
//! - **可选槽**（workspace_store / team_id / persona_api / config_store /
//!   session_persist_path / subagent_store）：None 是合法运行时状态
//!   （如未启用 QQ 工作区插件）。
//! - **不进 Builder 的运行期接线**：`attach`（BackendHandle 桥）、
//!   `set_loop_runner` / `set_use_echo_loop`（echo-loop 驱动注入时机晚于
//!   装配）、`set_model` / `set_persona_api`（async 运行期版本）——
//!   保留在 `Agent` 上。
//!
//! 原 13 个 setter 全部保留但收敛为 `pub(crate)` 装配出口（Builder 与
//! 既有组合根同 crate；外部唯一生产调用方就是 core 的 make_agent）。

use std::sync::Arc;

use echo_adapter::{AdapterRegistry, ConfigStore};

use crate::agent::{Agent, EventSink};
use crate::config::AgentConfig;
use crate::llm::LlmProvider;
use crate::skill::SkillRegistry;
use crate::tool::ToolRegistry;

/// `Agent` 的声明式装配器（必填槽见模块文档）。
pub struct AgentBuilder {
    provider: Arc<dyn LlmProvider>,
    config: AgentConfig,
    skills: SkillRegistry,
    tools: ToolRegistry,
    adapters: Arc<AdapterRegistry>,
    // ── 必填槽（build 时校验）──
    plugin_host: Option<Arc<crate::plugins::PluginHost>>,
    event_sink: Option<EventSink>,
    // ── 可选槽 ──
    team_id: Option<String>,
    workspace_store: Option<Arc<crate::workspace::WorkspaceStore>>,
    persona_api: Option<String>,
    active_model: Option<String>,
    config_store: Option<ConfigStore>,
    session_persist_path: Option<std::path::PathBuf>,
    subagent_store: Option<Arc<crate::subagent::SubagentStore>>,
}

impl AgentBuilder {
    /// 装配的起点：`Agent::new` 的五个构造参数原样成为 Builder 入参。
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        config: AgentConfig,
        skills: SkillRegistry,
        tools: ToolRegistry,
        adapters: Arc<AdapterRegistry>,
    ) -> Self {
        Self {
            provider,
            config,
            skills,
            tools,
            adapters,
            plugin_host: None,
            event_sink: None,
            team_id: None,
            workspace_store: None,
            persona_api: None,
            active_model: None,
            config_store: None,
            session_persist_path: None,
            subagent_store: None,
        }
    }

    /// 进程级插件宿主（必填；所有人格共享同一个宿主实例）。
    pub fn plugin_host(mut self, host: Arc<crate::plugins::PluginHost>) -> Self {
        self.plugin_host = Some(host);
        self
    }

    /// 进程级事件汇聚点（必填；Panel 单连接看全部人格活动）。
    pub fn event_sink(mut self, sink: EventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// 人格身份（`__core` 服务代理为 None）。
    pub fn team_id(mut self, id: Option<String>) -> Self {
        self.team_id = id;
        self
    }

    /// 工作区会话存储（`echo-agent.workspace` 插件；未挂载 = None）。
    pub fn workspace_store(mut self, store: Arc<crate::workspace::WorkspaceStore>) -> Self {
        self.workspace_store = Some(store);
        self
    }

    /// Persona 级 API 供应商引用（None = 跟随全局默认）。
    pub fn persona_api(mut self, name: Option<String>) -> Self {
        self.persona_api = name;
        self
    }

    /// 启动期已按 persona profile 解析好的生效 model（不经 provider 重建）。
    pub fn active_model(mut self, model: String) -> Self {
        self.active_model = Some(model);
        self
    }

    /// 共享配置存储（`[agent]` TOML 持久化）。
    pub fn config_store(mut self, store: ConfigStore) -> Self {
        self.config_store = Some(store);
        self
    }

    /// 会话持久化路径（`echo-sessions-{id}.json`）。
    pub fn session_persist_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.session_persist_path = Some(path.into());
        self
    }

    /// Subagent 插件运行态所需的子任务注册表。
    pub fn subagent_store(mut self, store: Arc<crate::subagent::SubagentStore>) -> Self {
        self.subagent_store = Some(store);
        self
    }

    /// 完成装配，返回 `Arc<Agent>`（组合根本来就以 Arc 持有；subagent
    /// 运行态装配需要 `&Arc<Self>`）。必填槽缺失 = 装配 bug，fail-fast
    /// panic（消息指明缺哪个）。
    pub fn build(self) -> Arc<Agent> {
        let agent = Agent::new(
            self.provider,
            self.config,
            self.skills,
            self.tools,
            self.adapters,
        );
        // 必填槽：缺了就是组合根写错了，尽早炸。
        let plugin_host = self
            .plugin_host
            .expect("AgentBuilder: plugin_host is required (装配 bug)");
        let event_sink = self
            .event_sink
            .expect("AgentBuilder: event_sink is required (装配 bug)");
        agent.set_plugin_host(plugin_host);
        agent.attach_event_sink(event_sink);
        // 可选槽：按槽位语义装配。
        if self.team_id.is_some() {
            agent.set_team_id(self.team_id);
        }
        if self.persona_api.is_some() {
            agent.set_persona_api_now(self.persona_api);
        }
        if let Some(model) = self.active_model {
            agent.set_model_now(model);
        }
        if let Some(store) = self.config_store {
            agent.set_config_store(store);
        }
        if let Some(path) = self.session_persist_path {
            agent.set_session_persist_path(path);
        }
        let agent = Arc::new(agent);
        if let Some(store) = self.workspace_store {
            agent.set_workspace_store(store);
        }
        if let Some(store) = self.subagent_store {
            agent.attach_subagent_runtime(store);
        }
        agent
    }
}
