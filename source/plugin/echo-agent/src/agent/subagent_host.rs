//! Subagent 插件**执行体的宿主端口实现**（全插件化 S3）。
//!
//! 执行体（受理 → 建上下文 → spawn → 超时/取消 → 终态三分支 → hook 回灌，
//! 401 行）已搬进 [`echo_plugin_subagent::execution`]。本文件只剩**端口实现**
//! ——九件"只有宿主知道"的事。
//!
//! ## 为什么 hook 投递留在宿主
//!
//! 把一段 hook 文本变成主会话里的新入站分支，要走宿主完整的入站生命周期
//! （查会话、取消息序号、发 `MessageReceived`、后台边界块、`process_inbound_
//! branch`）。那是**核心编排**，不是子代理能力——插件只表达意图
//! （`deliver_hook(session_id, text)`），流程留在宿主。这与 workspace 的
//! store 变更钩子同一手法：插件改状态，宿主自己走完整流程。
//!
//! ## 为什么 `provider_for` 留在宿主
//!
//! `spawn_subagent` 的 `profile` 要按名从**供应商池**（`core.toml`）解析出
//! provider，解析失败 fail-closed（不回退主模型）。供应商池是宿主的配置面，
//! provider 工厂在 `echo-plugin-provider-llm`——两者都在装配期才连得上。

use std::sync::Arc;

use echo_plugin_subagent::execution::SubagentExecutionHost;
use echo_plugin_subagent::SubagentStatus;
use echo_protocol::BackendEvent;

use crate::agent::Agent;

/// 宿主侧端口实现：借用 `&Arc<Agent>`（事件与 hook 投递都需要它）。
pub struct AgentSubagentHost {
    agent: Arc<Agent>,
}

impl AgentSubagentHost {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self { agent }
    }
}

impl SubagentExecutionHost for AgentSubagentHost {
    fn base_prompt(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + '_>> {
        let agent = Arc::clone(&self.agent);
        Box::pin(async move { agent.model.config.read().await.system_prompt.clone() })
    }

    fn tool_definitions(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<echo_defs::tool::ToolDefinition>> + Send + '_>>
    {
        let agent = Arc::clone(&self.agent);
        Box::pin(async move { (*agent.tools.definitions().await).clone() })
    }

    fn tool_registry(&self) -> Arc<crate::tool::ToolRegistry> {
        Arc::clone(&self.agent.tools)
    }

    fn inherit_provider(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Arc<dyn crate::llm::LlmProvider>> + Send + '_>,
    > {
        let agent = Arc::clone(&self.agent);
        Box::pin(async move { agent.model.provider.read().await.clone() })
    }

    fn provider_for<'a>(
        &'a self,
        profile: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Arc<dyn crate::llm::LlmProvider>, String>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.agent.subagent_provider_for(profile).await })
    }

    fn loop_limits(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = (usize, Option<u32>)> + Send + '_>>
    {
        let agent = Arc::clone(&self.agent);
        Box::pin(async move {
            let config = agent.model.config.read().await;
            (
                config.max_tool_iterations,
                config.effective_max_tokens(),
            )
        })
    }

    fn emit(&self, event: BackendEvent) {
        self.agent.emit(event);
    }

    fn notify_remote(
        &self,
        peer: &str,
        task_id: &str,
        task: &str,
        timeout_secs: Option<u64>,
        status: SubagentStatus,
        result: Option<String>,
    ) {
        crate::federation::notify_remote_subagent(
            &self.agent.federation(),
            peer,
            task_id,
            task,
            timeout_secs,
            echo_plugin_subagent::wire_status(status),
            result,
        );
    }

    fn deliver_hook<'a>(
        &'a self,
        session_id: &'a str,
        hook: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.agent.dispatch_subagent_hook(session_id, hook).await;
        })
    }
}
