//! 子代理**执行体**：把一次委派从受理跑到终态，宿主只注入端口。
//!
//! ## 为什么执行体归插件（全插件化 S3）
//!
//! 此前这段住在宿主 `agent/subagent_runtime.rs`（401 行）：受理 → 建上下文 →
//! spawn → 超时/取消 → 终态广播 → hook 回灌。它的每一行都是**子代理这个
//! 能力的实现**——只是恰好需要几样宿主的东西。
//!
//! 拆开后的分工：
//!
//! | 机制（本模块） | 宿主（[`SubagentExecutionHost`]） |
//! |---|---|
//! | 上下文装配（提示词 / 工具视图 / 远程工具剥前缀） | 提示词、工具定义、注册表从哪来 |
//! | spawn + span + 起始/终态日志 | 事件出口长什么样 |
//! | 超时 / 取消 / 终态三分支 | profile → provider 的解析 |
//! | 终态回报（store 销账 + 联邦通知 + hook 文本） | hook 投递到会话的那一步 |
//!
//! 最后一行是唯一"重"的接缝：把 hook 文本变成主会话里的新入站分支要走宿主
//! 的入站生命周期（`process_inbound_branch`）。插件的做法是**只表达意图**
//! （"把这个 hook 送到这个会话"），流程留在宿主——与 workspace 的 store
//! 变更钩子同一手法：插件改状态，宿主自己走完整流程。
//!
//! ## 为什么 `profile` 解析在宿主
//!
//! `spawn_subagent` 的 `profile` 参数要按名从**供应商池**解析出 provider，
//! 解析失败 fail-closed（不回退主模型）。供应商池是宿主的配置面
//! （`core.toml`），provider 工厂在 `echo-plugin-provider-llm`——两者都在
//! 宿主装配期才连得上。

use std::sync::Arc;

use echo_defs::llm::LlmProvider;
use echo_defs::remote_tool::remote_tool_prefix;
use echo_defs::tool::ToolDefinition;
use echo_tool::ToolRegistry;
use serde_json::json;

use crate::{wrap_subagent_event, SubagentStatus, SubagentStore, SPAWN_SUBAGENT_TOOL};

/// 宿主端口：执行体需要的九件事（每一项都是"只有宿主知道"）。
pub trait SubagentExecutionHost: Send + Sync {
    /// 主会话的系统提示词（子任务继承它作为 base）。
    fn base_prompt(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + '_>>;
    /// 本机工具定义（执行体会剥掉 `spawn_subagent`，单层委派）。
    fn tool_definitions(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ToolDefinition>> + Send + '_>>;
    /// 工具注册表（子任务直呼工具用）。
    fn tool_registry(&self) -> Arc<ToolRegistry>;
    /// 继承的 provider（`profile` 未指定时用）。
    fn inherit_provider(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Arc<dyn LlmProvider>> + Send + '_>>;
    /// 按 `profile` 名解析 provider（fail-closed：解析失败即拒绝）。
    fn provider_for<'a>(
        &'a self,
        profile: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Arc<dyn LlmProvider>, String>> + Send + 'a>,
    >;
    /// 循环参数（`max_tool_iterations` / `max_tokens`）。
    fn loop_limits(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = (usize, Option<u32>)> + Send + '_>>;
    /// 事件出口。
    fn emit(&self, event: echo_protocol::BackendEvent);
    /// 远程 subagent 观测（`node` 指定时通知对端；宿主把联邦出口接上）。
    fn notify_remote(
        &self,
        peer: &str,
        task_id: &str,
        task: &str,
        timeout_secs: Option<u64>,
        status: SubagentStatus,
        result: Option<String>,
    );
    /// 把 hook 文本作为**后台入站分支**投递到主会话。
    ///
    /// 插件只表达意图；入站生命周期（会话查找、消息序号、事件、后台边界块）
    /// 全在宿主——那是核心编排，不该由子代理插件复制一遍。
    fn deliver_hook<'a>(
        &'a self,
        session_id: &'a str,
        hook: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

/// 一次委派请求（与本 crate 的 [`crate::SpawnRequest`] 同形，这里再列一遍
/// 是为了让执行体的入口签名自解释——它不接受"半个请求"）。
pub use crate::SpawnRequest as ExecutionRequest;

/// 执行一次委派：受理 → spawn → 终态回报。**立即返回**（后台跑）。
///
/// `store` 是插件自己的存储（登记 / 销账 / 取消令牌），执行体与它同处一地，
/// 不需要宿主中转。
pub fn spawn_execution(
    host: Arc<dyn SubagentExecutionHost>,
    store: Arc<SubagentStore>,
    request: crate::SpawnRequest,
) {
    // 子任务 = 一个独立 span（2026-10 日志治理）：`tokio::spawn` 不继承主
    // turn 的 span，所以显式带上 `parent_branch_id`——排障时能回答"这次委派
    // 挂在哪个 turn 下、跑在哪个节点、用的哪个模型"。
    let span = tracing::info_span!(
        "subagent_task",
        task_id = %request.task_id,
        session = %request.session_id,
        parent_branch_id = %request.parent_branch_id,
        node = request.node.as_deref().unwrap_or("local"),
        profile = request.profile.as_deref().unwrap_or("inherit"),
    );
    tokio::spawn(
        async move {
            let crate::SpawnRequest {
                task_id,
                session_id,
                task,
                timeout,
                parent_cancel,
                parent_branch_id,
                node,
                profile,
            } = request;
            let started = std::time::Instant::now();
            // 起始日志与 span 同字段：卡死的子任务在日志里表现为"有 started
            // 无 finished"，一眼可判。
            tracing::info!(
                task_id = %task_id,
                session = %session_id,
                parent_branch_id = %parent_branch_id,
                node = ?node,
                profile = ?profile,
                "subagent started"
            );
            // 监听**注册表条目的令牌**（`cancel_all` / 逐项 `finish` 都取消
            // 它）；与 `parent_cancel` 是同一条传播链（登记时存的就是 parent
            // 的 child_token），主 turn 取消同样经 parent 链传导到它。
            let cancel = store
                .cancel_token_of(&task_id)
                .unwrap_or_else(|| parent_cancel.child_token());
            host.emit(echo_protocol::BackendEvent::SubagentStarted {
                session_id: session_id.clone(),
                task: task.clone(),
            });

            // ── 上下文：提示词 + 工具视图（剥 spawn_subagent，单层委派）──
            let base = host.base_prompt().await;
            let mut tools = host.tool_definitions().await;
            tools.retain(|definition| definition.name != SPAWN_SUBAGENT_TOOL);
            let registry = host.tool_registry();

            // `node` 指定时子任务工具视图指向远程节点——LLM 仍在本机推理，
            // 工具定义替换为该 peer 的代理工具描述（`<peer>__<tool>` 已注册
            // 进注册表，执行经 Invoke 路由）。语义（RFC §5）：远程 subagent
            // 的任务文本不感知本机工作区，工具列表即"在远程能做什么"的完整
            // 能力面。
            if let Some(peer) = node.as_deref() {
                let prefix = remote_tool_prefix(peer);
                let remote_defs: Vec<ToolDefinition> = tools
                    .iter()
                    .filter(|definition| definition.name.starts_with(&prefix))
                    .cloned()
                    .map(|mut definition| {
                        // 剥前缀：模型在远程语境下用本机工具名调用，执行端
                        // （router 的执行端入口）按前缀还原。
                        definition.name =
                            definition.name.trim_start_matches(&prefix).to_string();
                        definition
                    })
                    .collect();
                if remote_defs.is_empty() {
                    // fail-closed（2026-10 审查修复）：peer 离线 / 错名时
                    // **不**静默回本机执行（错机操作风险）——立即以 Failed
                    // 终态回报并销账（聚合组同步闭合）。
                    reject(
                        &host,
                        &store,
                        Reject {
                            task_id: &task_id,
                            session_id: &session_id,
                            task: &task,
                            parent_branch_id: &parent_branch_id,
                            node: Some(peer),
                            reason: format!(
                                "远程节点「{peer}」无可用代理工具（peer 离线、名称错误或联邦链路未建立）"
                            ),
                        },
                    )
                    .await;
                    return;
                }
                tools = remote_defs;
            }

            // ── 模型：`profile` 指定时按名解析（fail-closed，不回退主模型）；
            // 未指定 = 继承本 agent 的 provider（原有语义）。──
            let provider = match profile.as_deref() {
                Some(name) => match host.provider_for(name).await {
                    Ok(provider) => provider,
                    Err(reason) => {
                        reject(
                            &host,
                            &store,
                            Reject {
                                task_id: &task_id,
                                session_id: &session_id,
                                task: &task,
                                parent_branch_id: &parent_branch_id,
                                node: node.as_deref(),
                                reason,
                            },
                        )
                        .await;
                        return;
                    }
                },
                None => host.inherit_provider().await,
            };
            let (max_iterations, max_tokens) = host.loop_limits().await;

            // 联邦对端观测：远程子任务受理时通知对端（SubagentSpawn），
            // 对端 Panel 后台任务列表据此可见、可强制取消。
            if let Some(peer) = node.as_deref() {
                host.notify_remote(
                    peer,
                    &task_id,
                    &task,
                    Some(timeout.as_secs()),
                    SubagentStatus::Running,
                    None,
                );
            }

            let run = crate::run_subagent_turn(
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
                Ok(Ok(reply)) => (true, false, crate::truncate_result(&reply)),
                Ok(Err(error)) if crate::is_turn_cancelled(&error) => {
                    (false, true, "子任务已随主任务取消".into())
                }
                Ok(Err(error)) => (false, false, format!("子任务失败：{error}")),
                Err("timeout") => (
                    false,
                    false,
                    format!(
                        "子任务超时（{} 秒）已中止；可以拆小任务或稍后重试。",
                        timeout.as_secs()
                    ),
                ),
                Err(_) => (false, true, "子任务已随主任务取消".into()),
            };

            // ── 终态：store 销账 + 事件 + 联邦通知 + hook 回灌（恰好一次）──
            let status = match (success, cancelled) {
                (true, _) => SubagentStatus::Completed,
                (_, true) => SubagentStatus::Cancelled,
                _ => SubagentStatus::Failed,
            };
            store.finish(&task_id, status);
            tracing::info!(
                task_id = %task_id,
                success,
                cancelled,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "subagent finished"
            );
            if let Some(peer) = node.as_deref() {
                host.notify_remote(
                    peer,
                    &task_id,
                    &task,
                    None,
                    status,
                    Some(detail.clone()),
                );
            }
            host.emit(echo_protocol::BackendEvent::SubagentCompleted {
                session_id: session_id.clone(),
                success,
            });
            let payload = json!({
                "event": "subagent_event",
                "subagent_id": task_id,
                "session_id": session_id,
                "task": crate::subagent_task_field(&task),
                "success": success,
                "result": detail,
                "parent_branch_id": parent_branch_id,
                "completed_at_ms": now_millis(),
            });
            host.deliver_hook(&session_id, wrap_subagent_event(&payload))
                .await;
        }
        .instrument(span),
    );
}

/// 受理前的拒绝（peer 离线 / profile 解析失败）：与正常终态走**同一套**
/// 回报路径（store 销账 + 事件 + 联邦通知 + hook），只是 `success = false`。
///
/// 抽成函数而不是重复四段：拒绝也是一次终态，漏掉任何一步都会让面板上的
/// 子任务永远停在"运行中"，或让聚合组永远等不到它。
struct Reject<'a> {
    task_id: &'a str,
    session_id: &'a str,
    task: &'a str,
    parent_branch_id: &'a str,
    node: Option<&'a str>,
    reason: String,
}

async fn reject(host: &Arc<dyn SubagentExecutionHost>, store: &Arc<SubagentStore>, reject: Reject<'_>) {
    let Reject {
        task_id,
        session_id,
        task,
        parent_branch_id,
        node,
        reason,
    } = reject;
    tracing::warn!(
        task_id = %task_id,
        node = ?node,
        %reason,
        "subagent rejected before start"
    );
    store.finish(task_id, SubagentStatus::Failed);
    if let Some(peer) = node {
        host.notify_remote(
            peer,
            task_id,
            task,
            None,
            SubagentStatus::Failed,
            Some(reason.clone()),
        );
    }
    host.emit(echo_protocol::BackendEvent::SubagentCompleted {
        session_id: session_id.to_string(),
        success: false,
    });
    let payload = json!({
        "event": "subagent_event",
        "subagent_id": task_id,
        "session_id": session_id,
        "task": crate::subagent_task_field(task),
        "success": false,
        "result": format!("子任务受理失败：{reason}"),
        "parent_branch_id": parent_branch_id,
        "completed_at_ms": now_millis(),
    });
    host.deliver_hook(session_id, wrap_subagent_event(&payload))
        .await;
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

use tracing::Instrument as _;
