//! Subagent 插件运行态管理（attach/spawn 执行/hook 派发）。
//!
//! 从 `agent/mod.rs` 拆出（框架优化）：`Agent` 侧的 subagent 运行时
//! 装配（attach_subagent_runtime）、spawn 执行体
//! （spawn_subagent_execution）与 `<subagent_event>` hook 派发
//! （dispatch_subagent_hook）。与 `packages/subagent` 呼应：那边是
//! 工具与存储，这边是 Agent 上的接线与执行。纯移动零行为变化。

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

    /// spawn 执行体：后台以隔离上下文跑子任务，完成/失败/超时/取消都经
    /// hook 通知主 agent（恰好一次）。
    fn spawn_subagent_execution(self: &Arc<Self>, request: crate::subagent::SpawnRequest) {
        let agent = Arc::clone(self);
        tokio::spawn(async move {
            let crate::subagent::SpawnRequest {
                task_id,
                session_id,
                task,
                timeout,
                parent_cancel,
                parent_branch_id,
                node,
                profile,
            } = request;
            let (store, _) = match agent.subagent_runtime() {
                Some(runtime) => runtime,
                None => return,
            };
            // 执行体监听**注册表条目的令牌**（store.cancel_all / 逐项 finish 都取消
            // 它）；它与 parent_cancel 是同一传播链（spawn 时 register 存的就是
            // parent 的 child_token），主 turn 取消同样经 parent 链传导到该令牌。
            let cancel = store
                .cancel_token_of(&task_id)
                .unwrap_or_else(|| parent_cancel.child_token());
            agent.emit(BackendEvent::SubagentStarted {
                session_id: session_id.clone(),
                task: task.clone(),
            });
            // 子 agent 上下文：base 提示词 + 工具集（剥离 spawn_subagent，
            // 单层委派）。
            let base = agent.config.read().await.system_prompt.clone();
            let mut tools = (*agent.tools.definitions().await).clone();
            tools.retain(|d| d.name != crate::subagent::SPAWN_SUBAGENT_TOOL);
            let registry = Arc::clone(&agent.tools);
            // federation Phase 3：`node` 指定时子任务工具视图指向远程
            // 节点——LLM 仍在本机推理，工具定义替换为该 peer 的代理工具
            // 描述（`<peer>:<tool>` 已注册进注册表，执行经 Invoke 路由）。
            // 语义说明（RFC §5）：远程 subagent 的任务文本不感知本机
            // 工作区，工具列表即"在远程能做什么"的完整能力面。
            if let Some(ref peer) = node {
                let prefix = format!("{peer}:");
                let remote_defs: Vec<echo_defs::tool::ToolDefinition> = tools
                    .iter()
                    .filter(|d| d.name.starts_with(&prefix))
                    .cloned()
                    .map(|mut d| {
                        // 剥前缀：模型在远程语境下用本机工具名调用，
                        // 执行端（packages/federation 的路由）按前缀还原。
                        d.name = d.name.trim_start_matches(&prefix).to_string();
                        d
                    })
                    .collect();
                if remote_defs.is_empty() {
                    // fail-closed（2026-10 审查修复）：peer 离线/错名时
                    // **不**静默回本机执行（错机操作风险）——立即以
                    // Failed 终态回报并销账（聚合组同步闭合）。
                    let reason = format!(
                        "远程节点「{peer}」无可用代理工具（peer 离线、名称错误或联邦未启用）"
                    );
                    agent
                        .reject_subagent(
                            &task_id,
                            &session_id,
                            &task,
                            &parent_branch_id,
                            Some(peer),
                            reason,
                        )
                        .await;
                    return;
                }
                tools = remote_defs;
            }
            // 子代理模型：`profile` 指定时按名从供应商池解析（fail-closed——
            // 解析失败立即以 Failed 终态 + hook 回报，不回退主模型）；
            // 未指定 = 继承本 agent 的 provider（原有语义）。
            let provider = match profile.as_deref() {
                Some(name) => match agent.subagent_provider_for(name).await {
                    Ok(provider) => provider,
                    Err(reason) => {
                        agent
                            .reject_subagent(
                                &task_id,
                                &session_id,
                                &task,
                                &parent_branch_id,
                                node.as_deref(),
                                reason,
                            )
                            .await;
                        return;
                    }
                },
                None => agent.provider.read().await.clone(),
            };
            let max_iterations = agent.config.read().await.max_tool_iterations;
            let max_tokens = agent.config.read().await.effective_max_tokens();
            // federation Phase 3 对端观测：远程子任务受理时通知对端
            // （SubagentSpawn），完成/失败/取消时回报（SubagentEvent）——
            // 对端 Panel 后台任务列表据此可见、可强制取消。
            // 通知经进程级 federation 出口（组合根装配时注入）。
            if let Some(ref peer) = node {
                crate::federation::notify_remote_subagent(
                    peer,
                    &task_id,
                    &task,
                    Some(timeout.as_secs()),
                    crate::federation::SubagentStatus::Running,
                    None,
                );
            }
            let run = crate::subagent::run_subagent_turn(
                provider,
                tools,
                registry,
                base,
                task.clone(),
                cancel.clone(),
                max_iterations,
                max_tokens,
                node.as_deref(),
            );
            let outcome = tokio::select! {
                result = run => Ok(result),
                _ = tokio::time::sleep(timeout) => Err("timeout"),
                _ = cancel.cancelled() => Err("cancelled"),
            };
            let (success, cancelled, detail) = match outcome {
                Ok(Ok(reply)) => (true, false, crate::subagent::truncate_result(&reply)),
                Ok(Err(error)) if Agent::is_turn_cancelled(&error) => {
                    (false, true, "子任务已随主任务取消".into())
                }
                Ok(Err(error)) => (false, false, format!("子任务执行失败：{error}")),
                Err("timeout") => (
                    false,
                    false,
                    format!("子任务超时（{}s）", timeout.as_secs()),
                ),
                Err(_) => (false, true, "子任务已随主任务取消".into()),
            };
            let status = if success {
                crate::subagent::SubagentStatus::Completed
            } else if cancelled {
                crate::subagent::SubagentStatus::Cancelled
            } else {
                crate::subagent::SubagentStatus::Failed
            };
            store.finish(&task_id, status);
            if let Some(ref peer) = node {
                let fed_status = match status {
                    crate::subagent::SubagentStatus::Completed => {
                        crate::federation::SubagentStatus::Completed
                    }
                    crate::subagent::SubagentStatus::Cancelled => {
                        crate::federation::SubagentStatus::Cancelled
                    }
                    _ => crate::federation::SubagentStatus::Failed,
                };
                crate::federation::notify_remote_subagent(
                    peer,
                    &task_id,
                    &task,
                    None,
                    fed_status,
                    Some(detail.clone()),
                );
            }
            agent.emit(BackendEvent::SubagentCompleted {
                session_id: session_id.clone(),
                success,
            });
            // hook 回灌主 agent：作为该会话的全新入站分支（主 turn 已结束，
            // 结论需要新的 turn 来消化；与 QQ hook / timer 事件同族）。
            let payload = serde_json::json!({
                "event": "subagent_event",
                "subagent_id": task_id,
                "session_id": session_id,
                "task": crate::llm::truncate(&task, 500),
                "success": success,
                "result": detail,
                "parent_branch_id": parent_branch_id,
                "completed_at_ms": chrono::Utc::now().timestamp_millis(),
            });
            let hook = crate::subagent::wrap_subagent_event(&payload);
            agent.dispatch_subagent_hook(&session_id, hook).await;
        });
    }

    /// 子任务在**开始执行之前**被拒绝的收尾（profile 解析失败 / 远程节点
    /// 无可用工具）：store 落 `Failed`、通知对端（联邦下同时为本机聚合组
    /// 销账）、emit `SubagentCompleted{success:false}`、经 `<subagent_event>`
    /// hook 回报主 agent——与正常完成路径同一条收尾链，**绝不静默**：
    /// 模型据此纠错重试（如 profile 名写错时按回报的可用清单重发），
    /// 面板的委派行也不会悬挂在"运行中"。
    async fn reject_subagent(
        self: &Arc<Self>,
        task_id: &str,
        session_id: &str,
        task: &str,
        parent_branch_id: &str,
        node: Option<&str>,
        reason: String,
    ) {
        tracing::warn!(
            task_id = %task_id,
            node = ?node,
            %reason,
            "subagent rejected before start"
        );
        if let Some((store, _)) = self.subagent_runtime() {
            store.finish(task_id, crate::subagent::SubagentStatus::Failed);
        }
        if let Some(peer) = node {
            crate::federation::notify_remote_subagent(
                peer,
                task_id,
                task,
                None,
                crate::federation::SubagentStatus::Failed,
                Some(reason.clone()),
            );
        }
        self.emit(BackendEvent::SubagentCompleted {
            session_id: session_id.to_string(),
            success: false,
        });
        let payload = serde_json::json!({
            "event": "subagent_event",
            "subagent_id": task_id,
            "session_id": session_id,
            "task": crate::llm::truncate(task, 500),
            "success": false,
            "result": format!("子任务受理失败：{reason}"),
            "parent_branch_id": parent_branch_id,
            "completed_at_ms": chrono::Utc::now().timestamp_millis(),
        });
        let hook = crate::subagent::wrap_subagent_event(&payload);
        self.dispatch_subagent_hook(session_id, hook).await;
    }

    /// 把子任务完成 hook 作为新入站分支注入主会话（内部复用
    /// `process_inbound_branch` 的完整生命周期；`group_id=None` = 后台来源，
    /// 回复只进后台，不推送外部平台）。
    async fn dispatch_subagent_hook(self: &Arc<Self>, session_id: &str, hook: String) {
        let Some(session) = self.trunk.get(session_id) else {
            tracing::warn!(session = %session_id, "subagent hook target session gone");
            return;
        };
        let message_sequence = self.next_message_sequence();
        self.emit(BackendEvent::MessageReceived {
            session_id: session_id.to_string(),
            adapter_name: "subagent".into(),
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
        self.process_inbound_branch(
            &session,
            &hook,
            message_sequence,
            None,
            std::time::Duration::from_secs(u64::MAX),
        )
        .await;
    }
}
