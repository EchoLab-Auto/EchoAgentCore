//! Subagent 插件运行态管理：装配 / 取句柄 / hook 派发（宿主侧接线）。
//!
//! **执行体已搬进插件**（全插件化 S3，见 [`echo_plugin_subagent::execution`]）：
//! 受理、上下文装配、超时/取消、终态三分支、hook 文本构造共 280 行搬走，
//! 这里是 95 行——三件宿主才该做的事：
//!
//! | 本文件 | 为什么在宿主 |
//! |---|---|
//! | `attach_subagent_runtime` | 插件 `mount` 时由组合根调用，把 store 与 spawn 闭包接进 `Agent` |
//! | `spawn_subagent_execution` | 造 [`AgentSubagentHost`](crate::agent::subagent_host::AgentSubagentHost) 并递过去（端口实现需要 `&Arc<Agent>`） |
//! | `dispatch_subagent_hook` | 把 hook 文本变成主会话的**新入站分支**——走的是核心入站生命周期，不是子代理能力 |
//!
//! 远程委派的聚合登记经 `RemoteDelegationSink` 注入（本层不依赖联邦实现）。

use super::*;

impl Agent {
    /// 装配 subagent 插件运行态（插件 mount 时由组合根调用）。
    ///
    /// 同时接线 spawn 执行闭包：子任务以隔离上下文后台执行，完成时经
    /// `<subagent_event>` hook（`crate::subagent::wrap_subagent_event`）作为
    /// 新入站分支通知主 agent——hook 机制由 echo-loop 的
    /// `SubagentToolHooks` 定义，QQ 消息等入站复用同一「结构化 hook →
    /// 新 turn」路径。
    pub fn attach_subagent_runtime(self: &Arc<Self>, store: Arc<crate::subagent::SubagentStore>) {
        let agent = Arc::downgrade(self);
        let spawn: SubagentSpawnFn = Arc::new(move |request| {
            if let Some(agent) = agent.upgrade() {
                agent.spawn_subagent_execution(request);
            }
        });
        if let Ok(mut slot) = self.subagent.write() {
            *slot = Some(SubagentRuntime {
                store: store.clone(),
                spawn,
            });
        } else {
            tracing::warn!("subagent slot busy, ignoring attach_subagent_runtime");
            return;
        }
        store.spawn_sweeper(self.cancel.clone());
    }

    /// subagent 运行态（None = 插件未启用）。
    pub(crate) fn subagent_runtime(
        &self,
    ) -> Option<(Arc<crate::subagent::SubagentStore>, SubagentSpawnFn)> {
        let guard = self.subagent.read().ok()?;
        let runtime = guard.as_ref()?;
        Some((runtime.store.clone(), runtime.spawn.clone()))
    }

    /// spawn 执行体：**实现已搬进 `echo-plugin-subagent`**（全插件化 S3）。
    ///
    /// 本方法只剩"把宿主端口实现递过去"——受理、上下文装配、超时/取消、
    /// 终态三分支、hook 文本构造全在插件里（`execution::spawn_execution`）。
    fn spawn_subagent_execution(self: &Arc<Self>, request: crate::subagent::SpawnRequest) {
        let Some((store, _)) = self.subagent_runtime() else {
            // 插件未挂载：没有 store 就没有"受理"可言。此处只记日志——调用方
            // 是插件自己的 spawn 闭包，未挂载时它不会产生请求。
            tracing::warn!(
                task_id = %request.task_id,
                "subagent runtime detached; dropping spawn request"
            );
            return;
        };
        let host = Arc::new(crate::agent::subagent_host::AgentSubagentHost::new(Arc::clone(self)));
        crate::subagent::execution::spawn_execution(host, store, request);
    }

    /// 把子任务完成 hook 作为新入站分支注入主会话（内部复用
    /// `process_inbound_branch` 的完整生命周期；`group_id=None` = 后台来源，
    /// 回复只进后台，不推送外部平台）。
    pub(crate) async fn dispatch_subagent_hook(self: &Arc<Self>, session_id: &str, hook: String) {
        let Some(session) = self.trunk.get(session_id) else {
            tracing::warn!(session = %session_id, "subagent hook target session gone");
            return;
        };
        let message_sequence = self.next_message_sequence();
        self.emit(BackendEvent::MessageReceived {
            session_id: session_id.to_string(),
            instance_name: "subagent".into(),
            platform: "subagent".into(),
            user_id: "subagent".into(),
            user_name: "subagent".into(),
            channel: "direct".into(),
            group_name: None,
            content: hook.clone(),
            images: vec![],
            timestamp: chrono::Utc::now().timestamp(),
            received_at_ms: chrono::Utc::now().timestamp_millis(),
            message_sequence,
            team_id: None,
        });
        self.process_inbound_branch(&session, &hook, message_sequence)
            .await;
    }
}
