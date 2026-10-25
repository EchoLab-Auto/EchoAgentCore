//! 入站 turn 生命周期：分支登记 / 串行闸门 / 取消 / 合并回主干，以及
//! `process_message` 与插件实例入站（`process_inbound_branch` →
//! `run_inbound_turn`）的完整流程。
//!
//! 2026-10 架构评审：自 `agent/mod.rs` 拆出（纯搬移，行为不变）。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tracing::Instrument as _;

use super::*;

/// 分支任务文本在**快照**里的截断长度（`ReplyBranchStarted` 仍发全文）。
///
/// 快照是"重连后重建 UI"用的，任务卡只显示摘要；长 prompt 可达数十 KB，
/// 每次 timeline 请求都跟着回一遍既浪费带宽又会把日志/事件流撑大。
const TASK_SNAPSHOT_CHARS: usize = 200;

/// 分支卡的默认目标标签（普通入站分支都回「源会话」；实例分支另有取值）。
const BRANCH_TARGET_SOURCE: &str = "源会话";

impl Agent {
    pub(crate) fn next_message_sequence(&self) -> u64 {
        self.turns.next_message_sequence.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn register_inbound_turn(
        &self,
        session_id: &str,
        message_sequence: u64,
        branch: bool,
        task: &str,
        target: &str,
        started_at_ms: i64,
    ) -> InboundTurnRegistration {
        let id = uuid::Uuid::new_v4().to_string();
        let cancel = tokio_util::sync::CancellationToken::new();
        self.turns.active_inbound_turns.insert(
            id.clone(),
            ActiveInboundTurn {
                session_id: session_id.to_string(),
                message_sequence,
                branch,
                cancel: cancel.clone(),
                // 截断与 `ReplyBranchStarted` 的展示口径一致：任务卡只显示
                // 一行摘要，快照没必要把整段提示词抄一遍（长 prompt 数 KB）。
                task: task.chars().take(TASK_SNAPSHOT_CHARS).collect(),
                target: target.to_string(),
                started_at_ms,
            },
        );
        InboundTurnRegistration {
            id,
            cancel,
            started_at_ms,
        }
    }

    /// 运行中的**回复分支**快照（`branch = true` 的入站 turn）。
    ///
    /// 与 [`Self::active_turn_session_ids`] 的区别：那个只回答"哪些会话在跑"
    /// （驱活动浮条），这个带上分支身份与任务文本，足以让 Panel 在重连后
    /// 把分支卡/任务卡原样重建（见 `ActiveTurnsSnapshot.branches`）。
    pub fn active_branch_snapshots(&self) -> Vec<super::ActiveBranchSnapshot> {
        let mut list: Vec<super::ActiveBranchSnapshot> = self
            .turns
            .active_inbound_turns
            .iter()
            .filter(|turn| turn.branch)
            .map(|turn| super::ActiveBranchSnapshot {
                session_id: turn.session_id.clone(),
                branch_id: turn.key().clone(),
                message_sequence: turn.message_sequence,
                task: turn.task.clone(),
                target: turn.target.clone(),
                started_at_ms: turn.started_at_ms,
            })
            .collect();
        list.sort_by_key(|b| (b.started_at_ms, b.message_sequence));
        list
    }

    pub(crate) fn finish_inbound_turn(&self, turn_id: &str) {
        self.turns.active_inbound_turns.remove(turn_id);
    }

    /// Cancel the newest active turn belonging to the command sender. Branches
    /// win ties so a follow-up cancellation targets the temporary work first.
    pub(crate) fn cancel_inbound_turns(&self, session_id: &str, all: bool) -> usize {
        let mut turns = self
            .turns.active_inbound_turns
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
            .turns.active_inbound_turns
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
        // 分支的快照元数据：与下面 `ReplyBranchStarted` 用的是同一份取值，
        // 保证"实时事件"与"重连快照"在 UI 上长得一模一样。
        let branch_task: String = content.chars().take(TASK_SNAPSHOT_CHARS).collect();
        let branch_target = BRANCH_TARGET_SOURCE;
        let branch_started_at_ms = chrono::Utc::now().timestamp_millis();
        let registration = if serial {
            self.register_inbound_turn(
                &session.id,
                message_sequence,
                true,
                &branch_task,
                branch_target,
                branch_started_at_ms,
            )
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
            self.register_inbound_turn(
                &session.id,
                message_sequence,
                true,
                &branch_task,
                branch_target,
                branch_started_at_ms,
            )
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
                // 落盘时刻 = 实时投影时刻（同一个事件前后脚发生），于是
                // "派生历史"与"实时副本"的同一条回答时间同源，面板的
                // `(role, time)` 身份才成立（见 `AssistantMessage::timestamp`）。
                timestamp: chrono::Utc::now().timestamp(),
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
            permit = self.turns.reply_branch_slots.acquire() => permit
                .map_err(|_| anyhow!("reply branch scheduler is closed"))?,
            _ = cancel.cancelled() => return Err(anyhow!(TURN_CANCELLED)),
        };
        // 一次 turn = 一个 span（2026-10 日志治理）：turn 内的全部事件
        // （模型调用、工具、子代理、压缩）因此共享同一 `session`/`branch_id`
        // 上下文。文本出口仍靠事件自带的显式字段（便于 grep），JSON 出口
        // 由 `with_current_span` 带出 span 树，无需每条事件手抄字段。
        let turn_span = tracing::info_span!(
            "agent_turn",
            session = %session.id,
            branch_id = %branch_id,
        );
        let result = self
            .process_message_inner(
                session,
                content,
                Some(history_snapshot),
                cancel,
                visible_reply,
                branch_id,
            )
            .instrument(turn_span)
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

    pub(crate) fn is_turn_cancelled(error: &anyhow::Error) -> bool {
        error.to_string() == TURN_CANCELLED
    }

    /// 当前正在执行 turn 的会话 id 去重列表（刷新恢复快照用，
    /// `ActiveTurnsSnapshot` 事件；见 protocol event 文档）。
    pub fn active_turn_session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .turns.active_inbound_turns
            .iter()
            .map(|t| t.session_id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    #[cfg(test)]
    pub(crate) fn active_inbound_turn_count(&self) -> usize {
        self.turns.active_inbound_turns.len()
    }

    #[cfg(test)]
    pub(crate) fn active_inbound_branch_count(&self) -> usize {
        self.turns.active_inbound_turns
            .iter()
            .filter(|turn| turn.branch)
            .count()
    }

    pub async fn process_message(&self, session: &Session, content: &str) -> Result<String> {
        if self.turns.draining.load(Ordering::Acquire) {
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
                target: BRANCH_TARGET_SOURCE.into(),
                // 与快照里的 started_at_ms 同一份取值——否则重连后重建的
                // 分支卡与实时收到的那张在排序/耗时上会相差几毫秒。
                started_at_ms: registration.started_at_ms,
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
    /// instance (QQ, ...) routes through it so the paths cannot drift apart.
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
    ) {
        if self.turns.draining.load(Ordering::Acquire) {
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
        let turn_cancel = registration.cancel.clone();
        let result = agent
            .process_recorded_message_with_progress(
                &session,
                &content,
                history_snapshot,
                turn_cancel.clone(),
                None,
                &branch_id,
            )
            .await;
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
                // 失败只在后端可见（`BackendEvent::Error`）：平台侧回执需要
                // 框架代发，已随插件实例概念一并移除（2026-10 选项 B）。
            }
        }
    }
}
