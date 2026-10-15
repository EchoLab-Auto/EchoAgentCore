//! 入站 turn 生命周期：分支登记 / 串行闸门 / 取消 / 合并回主干，以及
//! `process_message` 与适配器入站（`process_inbound_branch` →
//! `run_inbound_turn`）的完整流程。
//!
//! 2026-10 架构评审：自 `agent/mod.rs` 拆出（纯搬移，行为不变）。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{anyhow, Result};

use super::*;

impl Agent {
    pub(crate) fn next_message_sequence(&self) -> u64 {
        self.next_message_sequence.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn register_inbound_turn(
        &self,
        session_id: &str,
        message_sequence: u64,
        branch: bool,
    ) -> InboundTurnRegistration {
        let id = uuid::Uuid::new_v4().to_string();
        let cancel = tokio_util::sync::CancellationToken::new();
        self.active_inbound_turns.insert(
            id.clone(),
            ActiveInboundTurn {
                session_id: session_id.to_string(),
                message_sequence,
                branch,
                cancel: cancel.clone(),
            },
        );
        InboundTurnRegistration { id, cancel }
    }

    pub(crate) fn finish_inbound_turn(&self, turn_id: &str) {
        self.active_inbound_turns.remove(turn_id);
    }

    /// Cancel the newest active turn belonging to the command sender. Branches
    /// win ties so a follow-up cancellation targets the temporary work first.
    pub(crate) fn cancel_inbound_turns(&self, session_id: &str, all: bool) -> usize {
        let mut turns = self
            .active_inbound_turns
            .iter()
            .filter(|turn| turn.session_id == session_id)
            .map(|turn| (turn.message_sequence, turn.branch, turn.cancel.clone()))
            .collect::<Vec<_>>();
        turns.sort_by_key(|(sequence, branch, _)| (*sequence, *branch));
        if !all {
            turns = turns.into_iter().rev().take(1).collect();
        }
        let count = turns.len();
        for (_, _, cancel) in turns {
            cancel.cancel();
        }
        count
    }

    pub(crate) fn cancel_all_inbound_turns(&self) -> usize {
        let turns = self
            .active_inbound_turns
            .iter()
            .map(|turn| turn.cancel.clone())
            .collect::<Vec<_>>();
        let count = turns.len();
        for cancel in turns {
            cancel.cancel();
        }
        count
    }

    pub(crate) async fn cancel_requested_work(&self, session_id: &str, all: bool) -> usize {
        self.cancel_inbound_turns(session_id, all)
    }

    /// Commit an inbound event and capture the branch's point-in-time context
    /// as one operation. The conversation lock is never held while the model
    /// or tools are running.
    pub(crate) async fn record_incoming_and_snapshot(
        &self,
        session: &Session,
        content: &str,
    ) -> Vec<ChatMessage> {
        let _turn = session.turn_lock.lock().await;
        // 入站内容脱敏（2026-10 安全）：会话事件（及其投影出的历史）
        // 永不落明文——用户粘贴的密钥不得进会话日志 / 归档 / 模型上下文。
        let content = self.redact_text(content);
        self.trunk
            .append_event(echo_session::SessionEvent::UserMessage(
                echo_session::event::UserMessage {
                    content: content.clone(),
                    timestamp: chrono::Utc::now().timestamp(),
                    message_sequence: structured_message_sequence(&content),
                    source: None,
                    images: extract_input_images(&content),
                    session: Some(session.id.clone()),
                },
            ));
        let snapshot = session.history.lock().clone();
        snapshot
    }

    /// Acquire this session's serial turn slot when the agent runs in
    /// single-session mode (`LoopMode::Single`, the default).
    ///
    /// tokio's `Mutex` is a fair FIFO queue, so queued turns start in arrival
    /// order; the slot is released when the returned guard drops (end of the
    /// turn). Parallel mode returns `None` and never blocks. Cancellation
    /// while queued also resolves to `None` (the caller gives up the turn).
    pub(super) async fn acquire_turn_slot(
        &self,
        session: &Session,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        if self.loop_mode() != echo_defs::LoopMode::Single {
            return None;
        }
        tokio::select! {
            slot = Arc::clone(&session.turn_queue).lock_owned() => Some(slot),
            _ = cancel.cancelled() => None,
        }
    }

    /// Register a cancellable branch and record its input under the same
    /// conversation lock used by clear and merge operations.
    ///
    /// 单会话模式（默认）：先注册可取消的 turn（排队期间同样可被取消），
    /// 拿到该会话的串行闸门后才记录输入并取快照——排队的第二个 turn 因此
    /// 看到的是第一轮结束后的上下文；闸门随返回值交给调用方，turn 结束
    /// （guard drop）才让位。并行模式：注册与记录一次完成，各分支并发执行
    /// （到达时快照）。
    ///
    /// 返回 `None` = 排队期间被取消，调用方应直接收尾（不执行 turn）。
    #[allow(clippy::type_complexity)]
    pub(crate) async fn register_incoming_branch(
        &self,
        session: &Session,
        content: &str,
        message_sequence: u64,
    ) -> Option<(
        InboundTurnRegistration,
        Vec<ChatMessage>,
        Option<tokio::sync::OwnedMutexGuard<()>>,
    )> {
        let serial = self.loop_mode() == echo_defs::LoopMode::Single;
        let registration = if serial {
            self.register_inbound_turn(&session.id, message_sequence, true)
        } else {
            InboundTurnRegistration::default()
        };
        let slot = if serial {
            match self.acquire_turn_slot(session, &registration.cancel).await {
                Some(slot) => Some(slot),
                None => {
                    self.finish_inbound_turn(&registration.id);
                    return None;
                }
            }
        } else {
            None
        };
        let _turn = session.turn_lock.lock().await;
        let registration = if serial {
            registration
        } else {
            self.register_inbound_turn(&session.id, message_sequence, true)
        };
        self.trunk
            .append_event(echo_session::SessionEvent::UserMessage(
                echo_session::event::UserMessage {
                    content: content.to_string(),
                    timestamp: chrono::Utc::now().timestamp(),
                    message_sequence: Some(message_sequence),
                    source: None,
                    images: extract_input_images(content),
                    session: Some(session.id.clone()),
                },
            ));
        let snapshot = session.history.lock().clone();
        Some((registration, snapshot, slot))
    }

    pub(super) async fn record_assistant_reply(
        &self,
        session: &Session,
        reply: String,
        message_sequence: Option<u64>,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> bool {
        if reply.trim().is_empty() {
            return true;
        }
        let _turn = session.turn_lock.lock().await;
        if cancel.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            return false;
        }
        // The assistant reply is a durable event; the event log re-derives
        // the trunk projection. When the reply answers a sequenced request
        // (concurrent branches), insert it after that request's user event so
        // the projected order matches request order.
        let event =
            echo_session::SessionEvent::AssistantMessage(echo_session::event::AssistantMessage {
                content: reply,
                reasoning_content: None,
                tool_calls: vec![],
                session: Some(session.id.clone()),
            });
        match message_sequence {
            Some(sequence) => self.trunk.insert_event_after_sequence(sequence, event),
            None => self.trunk.append_event(event),
        }
        true
    }

    pub(crate) async fn record_control_reply(
        &self,
        session: &Session,
        reply: String,
        message_sequence: u64,
    ) {
        self.record_assistant_reply(session, reply, Some(message_sequence), None)
            .await;
    }

    pub(crate) async fn process_recorded_message(
        &self,
        session: &Session,
        content: &str,
        history_snapshot: Vec<ChatMessage>,
        cancel: tokio_util::sync::CancellationToken,
        branch_id: &str,
    ) -> Result<String> {
        self.process_recorded_message_with_progress(
            session,
            content,
            history_snapshot,
            cancel,
            None,
            branch_id,
        )
        .await
    }

    pub(crate) async fn process_recorded_message_with_progress(
        &self,
        session: &Session,
        content: &str,
        history_snapshot: Vec<ChatMessage>,
        cancel: tokio_util::sync::CancellationToken,
        visible_reply: Option<tokio::sync::watch::Sender<bool>>,
        branch_id: &str,
    ) -> Result<String> {
        let message_sequence = structured_message_sequence(content);
        let merge_cancel = cancel.clone();
        let branch_slot = tokio::select! {
            permit = self.reply_branch_slots.acquire() => permit
                .map_err(|_| anyhow!("reply branch scheduler is closed"))?,
            _ = cancel.cancelled() => return Err(anyhow!(TURN_CANCELLED)),
        };
        let result = self
            .process_message_inner(
                session,
                content,
                Some(history_snapshot),
                cancel,
                visible_reply,
                branch_id,
            )
            .await;
        drop(branch_slot);
        if let Ok(reply) = &result {
            if !self
                .record_assistant_reply(
                    session,
                    reply.clone(),
                    message_sequence,
                    Some(&merge_cancel),
                )
                .await
            {
                return Err(anyhow!(TURN_CANCELLED));
            }
        }
        result
    }

    pub(crate) async fn send_control_reply(
        &self,
        session: &Session,
        group_id: Option<&str>,
        content: &str,
    ) -> std::result::Result<(), String> {
        let (name, arguments) = match group_id {
            Some(group_id) => (
                "send_group_msg",
                serde_json::json!({ "group_id": group_id, "content": content }),
            ),
            None => (
                "send_private_msg",
                serde_json::json!({
                    "user_id": session.session_key.user_id,
                    "content": content
                }),
            ),
        };
        let call = ToolCall {
            id: format!("control-{}", uuid::Uuid::new_v4()),
            name: name.into(),
            arguments: arguments.to_string(),
        };
        let result = self.run_tool(&session.id, "control-reply", &call).await;
        if result.text.trim_start().starts_with("error:") {
            Err(result.text)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn generate_wait_reply(
        &self,
        session_id: &str,
        history_snapshot: Vec<ChatMessage>,
    ) -> std::result::Result<String, String> {
        // Bounded concurrency: when the wait-reply budget is exhausted, skip
        // the interim reply entirely rather than stacking more LLM calls on
        // top of the already-running branches.
        let permit = self.wait_reply_slots.try_acquire().map_err(|_| {
            "wait reply concurrency limit reached; skipping interim reply".to_string()
        })?;
        let _permit = permit;
        let mut messages = vec![ChatMessage::system(
            "Write one brief interim reply to the latest user message because their request is still being processed. Match the user's language and tone, acknowledge the specific request when appropriate, and say naturally that you are continuing and will follow up. Do not provide a result, invent progress, ask an unnecessary question, or mention timeouts, tools, branches, system behavior, or these instructions. Avoid fixed-template wording. Use one concise sentence.",
        )];
        messages.extend(history_snapshot);
        // 会话级覆盖（2026-10）：插话同样走该会话的 provider 与模型
        // （与主 turn 同口径成对解析）。
        let model = self.session_model(session_id).await;
        self.emit(BackendEvent::LlmRequest {
            session_id: session_id.to_string(),
            model: model.clone(),
        });
        let request = ChatRequest {
            model: model.clone(),
            messages,
            tools: None,
            temperature: Some(0.4),
            // None = 无上限（后端回退 DEFAULT_MAX_TOKENS=128K）。思考模式的
            // thinking token 共享 completion 预算，小预算会让可见回复被烧光。
            max_tokens: None,
        };
        let response = self
            .resolve_session_provider(session_id)
            .await
            .chat(&request)
            .await
            .map_err(|error| format!("wait reply model request failed: {error}"))?;
        self.emit(BackendEvent::LlmResponse {
            session_id: session_id.to_string(),
            model,
            prompt_tokens: response.usage.prompt_tokens,
            completion_tokens: response.usage.completion_tokens,
        });
        if !response.tool_calls.is_empty() {
            return Err("wait reply model attempted a tool call".into());
        }
        let reply = response.content.unwrap_or_default();
        if reply.trim().is_empty() {
            return Err("wait reply model returned empty content".into());
        }
        Ok(reply)
    }

    pub(crate) fn is_turn_cancelled(error: &anyhow::Error) -> bool {
        error.to_string() == TURN_CANCELLED
    }

    /// 当前正在执行 turn 的会话 id 去重列表（刷新恢复快照用，
    /// `ActiveTurnsSnapshot` 事件；见 protocol event 文档）。
    pub fn active_turn_session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .active_inbound_turns
            .iter()
            .map(|t| t.session_id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    #[cfg(test)]
    pub(crate) fn active_inbound_turn_count(&self) -> usize {
        self.active_inbound_turns.len()
    }

    #[cfg(test)]
    pub(crate) fn active_inbound_branch_count(&self) -> usize {
        self.active_inbound_turns
            .iter()
            .filter(|turn| turn.branch)
            .count()
    }

    pub async fn process_message(&self, session: &Session, content: &str) -> Result<String> {
        if self.draining.load(Ordering::Acquire) {
            return Err(anyhow!("Core 正在重启，消息暂未处理，请稍后重试"));
        }
        // 入站内容脱敏（2026-10 安全）：本 turn 及后续记录 / 模型上下文
        // 只使用脱敏后的文本（原 content 的调用方不受影响）。
        let content = self.redact_text(content);
        let content = content.as_str();
        let message_sequence =
            structured_message_sequence(content).unwrap_or_else(|| self.next_message_sequence());
        // `_serial`：单会话模式的排队闸门，持有到本轮结束（drop 即让位）。
        // 排队期间被取消 → None：入站事件已记录，但没有可回复的内容。
        let Some((registration, history_snapshot, _serial)) = self
            .register_incoming_branch(session, content, message_sequence)
            .await
        else {
            return Err(anyhow!(TURN_CANCELLED));
        };
        let branch_id = registration.id.clone();
        // 分支可见性（ReplyBranch* 事件）由能力开关决定：禁用时前端不展示
        // 分支卡/任务卡，但分支照常执行（取消/合并语义不变）。
        let show_branch = self.shows_reply_branches();
        if show_branch {
            self.emit(BackendEvent::ReplyBranchStarted {
                session_id: session.id.clone(),
                branch_id: branch_id.clone(),
                message_sequence,
                task: content.to_string(),
                target: "源会话".into(),
                started_at_ms: chrono::Utc::now().timestamp_millis(),
            });
        }
        let result = self
            .process_recorded_message(
                session,
                content,
                history_snapshot,
                registration.cancel.clone(),
                &branch_id,
            )
            .await;
        let cancelled = registration.cancel.is_cancelled()
            || result.as_ref().err().is_some_and(Self::is_turn_cancelled);
        self.finish_inbound_turn(&branch_id);
        if show_branch {
            self.emit(BackendEvent::ReplyBranchCompleted {
                session_id: session.id.clone(),
                branch_id,
                message_sequence,
                success: result.is_ok() && !cancelled,
                cancelled,
                completed_at_ms: chrono::Utc::now().timestamp_millis(),
            });
        }
        // 取消也要发 AgentCompleted（与 QQ 路径一致）：
        // 前端据此把 activity phase 置 completed —— 否则 busy 卡住、
        // 取消按钮/活动浮条/思考动画永远不中断。
        if cancelled {
            self.emit(BackendEvent::AgentCompleted {
                session_id: session.id.clone(),
            });
        }
        result
    }

    /// Full lifecycle for an inbound platform message. This is the single
    /// implementation of the "register branch → emit lifecycle events →
    /// bounded wait reply → execute → merge → surface outcome" flow; every
    /// adapter (QQ, ...) routes through it so the paths cannot drift apart.
    ///
    /// Runs the branch in the background and returns immediately, matching the
    /// one-way inbound hook contract.
    ///
    /// 单会话模式（默认）：注册（含会话串行闸门）在分支任务内完成——QQ 入站
    /// 不因排队而阻塞，排队中的分支同样可被取消，且本轮拿到的是前一轮结束后
    /// 的上下文快照。并行多会话模式：到达即注册并取快照（与旧行为一致），
    /// 各分支并发执行。
    pub(crate) async fn process_inbound_branch(
        self: &Arc<Self>,
        session: &Session,
        content: &str,
        message_sequence: u64,
        group_id: Option<String>,
        wait_reply_after: std::time::Duration,
    ) {
        if self.draining.load(Ordering::Acquire) {
            tracing::info!(session = %session.id, "inbound message dropped: core draining for restart");
            return;
        }
        // 入站内容脱敏（2026-10 安全）：QQ hook 等外部输入先脱敏再进入
        // 记录 / 模型上下文（覆盖单会话与并行两条路径）。
        let content = self.redact_text(content);
        let session = session.clone();
        // 能力开关：禁用时跳过 ReplyBranch* 可见性事件（分支照常执行）。
        let show_branch = self.shows_reply_branches();
        let serial = self.loop_mode() == echo_defs::LoopMode::Single;
        if serial {
            let agent = Arc::clone(self);
            tokio::spawn(async move {
                let Some((registration, history_snapshot, slot)) = agent
                    .register_incoming_branch(&session, &content, message_sequence)
                    .await
                else {
                    tracing::info!(
                        session = %session.id,
                        message_sequence,
                        "queued inbound branch cancelled before it started"
                    );
                    return;
                };
                agent
                    .run_inbound_turn(
                        session,
                        content,
                        message_sequence,
                        group_id,
                        wait_reply_after,
                        show_branch,
                        registration,
                        history_snapshot,
                        slot,
                    )
                    .await;
            });
            return;
        }
        // 运行期可能从 Parallel 切回 Single（能力热更新），且排队期间可被
        // 取消——`register_incoming_branch` 此时返回 None，不再 expect
        // （2026-10：TOCTOU panic 修复）。
        let Some((registration, history_snapshot, slot)) = self
            .register_incoming_branch(&session, &content, message_sequence)
            .await
        else {
            tracing::info!(
                session = %session.id,
                message_sequence,
                "queued inbound branch cancelled before it started"
            );
            return;
        };
        let agent = Arc::clone(self);
        tokio::spawn(async move {
            agent
                .run_inbound_turn(
                    session,
                    content,
                    message_sequence,
                    group_id,
                    wait_reply_after,
                    show_branch,
                    registration,
                    history_snapshot,
                    slot,
                )
                .await;
        });
    }

    /// 分支主体（两模式共用）：可见性事件 + 有界等待回复 + 执行 + 收尾。
    ///
    /// `_serial` 在单会话模式下是本会话的排队闸门，持有到本轮结束（任何
    /// 返回路径都会 drop 让位）。
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_inbound_turn(
        self: Arc<Self>,
        session: Session,
        content: String,
        message_sequence: u64,
        group_id: Option<String>,
        wait_reply_after: std::time::Duration,
        show_branch: bool,
        registration: InboundTurnRegistration,
        history_snapshot: Vec<ChatMessage>,
        _serial: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) {
        let agent = Arc::clone(&self);
        let session_id = session.id.clone();
        let branch_id = registration.id.clone();
        tracing::info!(
            session = %session_id,
            message_sequence,
            branch_id = %branch_id,
            started_at_ms = chrono::Utc::now().timestamp_millis(),
            "temporary inbound reply branch started"
        );
        if show_branch {
            agent.emit(BackendEvent::ReplyBranchStarted {
                session_id: session_id.clone(),
                branch_id: branch_id.clone(),
                message_sequence,
                task: content.clone(),
                target: "源会话".into(),
                started_at_ms: chrono::Utc::now().timestamp_millis(),
            });
        }
        let (visible_reply_tx, visible_reply_rx) = tokio::sync::watch::channel(false);
        let branch_completed = tokio_util::sync::CancellationToken::new();
        let group_id_for_wait = group_id.clone();
        spawn_contextual_wait_reply(
            Arc::clone(&agent),
            session.clone(),
            branch_id.clone(),
            group_id_for_wait,
            history_snapshot.clone(),
            visible_reply_rx,
            branch_completed.clone(),
            wait_reply_after,
        );
        let turn_cancel = registration.cancel.clone();
        let result = agent
            .process_recorded_message_with_progress(
                &session,
                &content,
                history_snapshot,
                turn_cancel.clone(),
                Some(visible_reply_tx),
                &branch_id,
            )
            .await;
        branch_completed.cancel();
        let cancelled = turn_cancel.is_cancelled()
            || result.as_ref().err().is_some_and(Self::is_turn_cancelled);
        agent.finish_inbound_turn(&branch_id);

        tracing::info!(
            session = %session_id,
            message_sequence,
            branch_id = %branch_id,
            success = result.is_ok() && !cancelled,
            cancelled,
            completed_at_ms = chrono::Utc::now().timestamp_millis(),
            "temporary inbound reply branch finished"
        );
        if show_branch {
            agent.emit(BackendEvent::ReplyBranchCompleted {
                session_id: session_id.clone(),
                branch_id: branch_id.clone(),
                message_sequence,
                success: result.is_ok() && !cancelled,
                cancelled,
                completed_at_ms: chrono::Utc::now().timestamp_millis(),
            });
        }
        match result {
            Ok(output) => {
                if !output.trim().is_empty() {
                    agent.emit(BackendEvent::AgentOutput {
                        session_id: session_id.clone(),
                        content: output,
                        branch_id: Some(branch_id.clone()),
                        team_id: None,
                    });
                }
                tracing::info!(
                    session = %session_id,
                    message_sequence,
                    "inbound adapter message processed"
                );
            }
            Err(error) if Self::is_turn_cancelled(&error) => {
                agent.emit(BackendEvent::AgentCompleted {
                    session_id: session_id.clone(),
                });
                tracing::info!(
                    session = %session_id,
                    message_sequence,
                    "inbound adapter task cancelled"
                );
            }
            Err(error) => {
                tracing::warn!(%error, session = %session_id, "agent processing failed");
                agent.emit(BackendEvent::Error {
                    session_id: Some(session_id.clone()),
                    message: error.to_string(),
                });
                // The platform user has no view of backend Error events —
                // send a bounded failure notice so they are not left
                // wondering why no reply arrived.
                let agent = Arc::clone(&agent);
                let session = session.clone();
                let session_id = session_id.clone();
                let group_id = group_id.clone();
                tokio::spawn(async move {
                    let notice = format!("处理失败：{error}");
                    let notice: String = notice.chars().take(500).collect();
                    if let Err(send_error) = agent
                        .send_control_reply(&session, group_id.as_deref(), &notice)
                        .await
                    {
                        tracing::warn!(%send_error, session = %session_id, "failure notice could not be delivered");
                    }
                });
            }
        }
    }
}
