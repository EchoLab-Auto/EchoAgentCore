//! 会话视图命令域的**宿主能力实现**（全插件化 S5）。
//!
//! 与 `provider_host` / `skills_host` / `tools_host` 同构：**"发什么、按什么
//! 顺序、怎么瘦身"在插件**（`echo_plugin_management::session_view`），宿主只
//! 提供"只有它读得到"的那些数据：
//!
//! | 宿主提供 | 唯一性来源 |
//! |---|---|
//! | [`state_snapshot`](AgentSessionViewHost) | 全量快照要遍历**全部人格**的会话 + 实例清单/状态 |
//! | [`context_snapshot`](AgentSessionViewHost) | 上下文 token 数要 `TrunkStore` 的历史 |
//! | [`trunk_timeline`](AgentSessionViewHost) | 显示时间线窗口在 `TrunkStore`（限长 / 落盘 / 单调 seq） |
//! | [`timeline_earlier`](AgentSessionViewHost) | 向上翻页读同一份窗口 |
//! | [`session_history`](AgentSessionViewHost) | 派生要读**事件日志**（唯一事实来源） |
//!
//! **接缝刻意不返回 `TrunkStore` 引用**：那会让插件认识宿主的锁类型与窗口
//! 实现（"seam 变影子"的经典形状）。这里每个方法都是**一次语义快照**。

use std::sync::Arc;

use echo_plugin_management::session_view::{
    EarlierView, HistoryView, SessionViewHost, TimelineView,
};
use echo_protocol::BackendEvent;

use crate::agent::Agent;

/// 宿主适配器：以"某个 persona"为缺省视角的会话视图。
pub struct AgentSessionViewHost {
    agent: Arc<Agent>,
}

impl AgentSessionViewHost {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self { agent }
    }

    /// 解析目标 persona（`None` / 解析不到 = 本 agent）。
    fn target(&self, team_id: Option<&str>) -> Arc<Agent> {
        team_id
            .and_then(|id| self.agent.registry().and_then(|m| m.resolve(id)))
            .unwrap_or_else(|| Arc::clone(&self.agent))
    }
}

/// 全量路径取窗口尾部：`limit` 为 None/0 时返回全部。
fn window_tail(trunk: &crate::session::TrunkStore, limit: Option<usize>) -> (Vec<crate::event::TimelineMessage>, bool) {
    match limit {
        Some(n) if n > 0 => trunk.timeline_window_tail(n),
        _ => (trunk.timeline_snapshot(), false),
    }
}

impl SessionViewHost for AgentSessionViewHost {
    fn state_snapshot(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<BackendEvent>> + Send + '_>> {
        let agent = Arc::clone(&self.agent);
        Box::pin(async move {
            let mut events = agent.api_config_events().await;
            // 会话列表要覆盖**所有人格**：每个会话由事件自带 team_id
            //（SessionInfo.team_id / emit 的 annotate_team），Panel 按当前
            // agent 过滤展示。只遍历默认 agent 会让其他人格的会话缺失
            //（或混入主 agent 会话，刷新后侧边栏串显）。
            let mut emitted_sessions = 0usize;
            for s in agent.trunk.all() {
                let last = s
                    .history
                    .lock()
                    .last()
                    .cloned()
                    .map(|m| m.content)
                    .unwrap_or_default();
                let identity = agent.node_identity();
                events.push(BackendEvent::SessionUpdated {
                    session: s.info(last, &identity),
                });
                emitted_sessions += 1;
            }
            if let Some(mgr) = agent.registry() {
                for running in mgr.all() {
                    if running.agent.team_id() == agent.team_id() {
                        continue; // 已由上方覆盖
                    }
                    let identity = running.agent.node_identity();
                    for s in running.agent.trunk.all() {
                        let last = s
                            .history
                            .lock()
                            .last()
                            .cloned()
                            .map(|m| m.content)
                            .unwrap_or_default();
                        events.push(BackendEvent::SessionUpdated {
                            session: s.info(last, &identity),
                        });
                        emitted_sessions += 1;
                    }
                }
            }
            // DEBUG：这是「面板重连后的一次全量快照」，频次跟随连接抖动
            //（实测 151 条/天），不是状态变化——默认级别不该看见。
            tracing::debug!(
                sessions = emitted_sessions,
                "state sessions emitted for all personas"
            );
            // 实例清单 + 实例管理状态（TUI 启动时要拿到持久化的值）。
            events.push(crate::agent::instance_list_event(&agent));
            for name in agent.instances.names() {
                let Some(instance) = agent.instances.get(&name) else {
                    continue;
                };
                // 状态下发单源（投影在插件里）：`InstanceState` 的构造归
                // `echo-plugin-tools`，宿主只决定"何时发"。
                if let Some(management) = instance.management() {
                    if let Some(state) = management.cached_state() {
                        events.push(BackendEvent::InstanceState {
                            instance: instance.name().to_string(),
                            state,
                        });
                    }
                }
            }
            events
        })
    }

    fn context_snapshot(
        &self,
        team_id: Option<String>,
        session_id: Option<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<BackendEvent>> + Send + '_>> {
        // 会话隔离：team 指定时用该成员自己的 trunk（不同记忆）。
        let target = self.target(team_id.as_deref());
        Box::pin(async move {
            target
                .context_snapshot_events(session_id.as_deref())
                .await
        })
    }

    fn trunk_timeline(
        &self,
        team_id: Option<String>,
        since_seq: u64,
        limit: Option<usize>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<TimelineView>> + Send + '_>> {
        let target = self.target(team_id.as_deref());
        Box::pin(async move {
            // `since_seq > 0` 走增量快照；增量窗口不完整（None）回退全量。
            // `full` 标志告知前端本次是替换还是追加/修补——前端不再凭 seq
            // 大小猜测（全量误当增量会整段重复，空增量误当全量会清空聊天）。
            //
            // `has_more`（2026-10 分页）：全量路径按 `limit` 只回**最新** N 条
            // 时置 true；增量路径**不截断**——那是缺口补齐，截断会让前端游标
            // 与实际内容不一致。
            if since_seq > 0 {
                match target.trunk.timeline_snapshot_since(since_seq) {
                    Some((messages, seq)) => Some(TimelineView {
                        messages,
                        seq,
                        full: false,
                        has_more: false,
                    }),
                    None => {
                        let (messages, has_more) = window_tail(&target.trunk, limit);
                        Some(TimelineView {
                            messages,
                            seq: target.trunk.timeline_seq(),
                            full: true,
                            has_more,
                        })
                    }
                }
            } else {
                let (messages, has_more) = window_tail(&target.trunk, limit);
                Some(TimelineView {
                    messages,
                    seq: target.trunk.timeline_seq(),
                    full: true,
                    has_more,
                })
            }
        })
    }

    fn timeline_earlier(
        &self,
        team_id: Option<String>,
        before_ord: u64,
        limit: Option<usize>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EarlierView> + Send + '_>> {
        // 向上翻页（2026-10 分页）：返回 `ord < before_ord` 的最近 N 条。
        // 与 `RequestTrunkTimeline` 分开的理由见协议注释：翻页是"前插到已
        // 加载列表头部"，不动同步游标；混在 full/增量语义里会让前端难以判断
        // 该替换还是该前插。
        let limit = limit.unwrap_or(200).clamp(1, 500);
        let target = self.target(team_id.as_deref());
        Box::pin(async move {
            let (messages, has_more) = target.trunk.timeline_page(before_ord, limit);
            EarlierView { messages, has_more }
        })
    }

    fn session_history(
        &self,
        team_id: Option<String>,
        session_id: String,
        limit: Option<usize>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HistoryView> + Send + '_>> {
        // 按会话派生显示历史（2026-10 面板「会话无消息」修复）：共享显示
        // 时间线有 1024 条上限、跨会话 FIFO 淘汰，旧会话条目可能被整体挤出；
        // 事件日志按会话分区且完整，因此从这里按需派生。
        let limit = limit.unwrap_or(500).clamp(1, 2000);
        let target = self.target(team_id.as_deref());
        Box::pin(async move {
            let (messages, total, truncated) =
                target.trunk.display_history_for_session(&session_id, limit);
            // 合并水位线：派生时该会话在显示时间线中的最大 seq。
            // **必须与派生范围一致**（`history_watermark` 排除推理——推理不落
            // 事件日志、派生不含；若把它计进水位线，前端会把实时推理判成
            // "快照已覆盖"而整条丢弃 → 看不到思考 chip）。
            let max_seq = crate::session::history_watermark(
                target.trunk.timeline_snapshot(),
                session_id.as_str(),
            );
            HistoryView {
                messages,
                total,
                truncated,
                max_seq,
            }
        })
    }

    fn active_turns(
        &self,
        team_id: Option<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<String>> + Send + '_>> {
        let target = self.target(team_id.as_deref());
        Box::pin(async move { target.active_turn_session_ids() })
    }

    /// 运行中的回复分支：把 Core 的入站 turn 表翻译成**线上形状**。
    ///
    /// 只回 `branch = true` 的入站 turn（单会话串行模式的 turn 不是分支，
    /// 它的运行态由 `active_turns` 的会话列表表达）。
    fn active_branches(
        &self,
        team_id: Option<String>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Vec<echo_protocol::event::ActiveBranchInfo>,
                > + Send
                + '_,
        >,
    > {
        let target = self.target(team_id.as_deref());
        Box::pin(async move {
            target
                .active_branch_snapshots()
                .into_iter()
                .map(|b| echo_protocol::event::ActiveBranchInfo {
                    session_id: b.session_id,
                    branch_id: b.branch_id,
                    message_sequence: b.message_sequence,
                    task: b.task,
                    target: b.target,
                    started_at_ms: b.started_at_ms,
                })
                .collect()
        })
    }

    fn emit(&self, event: BackendEvent) {
        self.agent.emit(event);
    }
}
