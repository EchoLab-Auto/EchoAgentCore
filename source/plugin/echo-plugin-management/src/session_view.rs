//! 会话视图命令域：面板的 5 条"把会话讲给你看"的命令由本插件处理。
//!
//! ## 为什么这个域归本 crate
//!
//! 本 crate 已经拥有**显示时间线的投影策略**（[`super::timeline`]：哪些事件
//! 进面板、字段怎么映射、工具完成态怎么原地合并、推理条目发前端怎么瘦身）
//! 与**按会话派生显示历史**（[`super::history`]）。而 `RequestState` /
//! `RequestContext` / `RequestTrunkTimeline` / `RequestTimelineEarlier` /
//! `RequestSessionHistory` 正好是同一件事的命令面：**"面板看到什么"**。
//!
//! 此前它们住在宿主 `agent/commands.rs`（约 260 行），于是"投影规则"在插件、
//! "什么时候发什么"在宿主——两份知识得对齐着改。现在一处。
//!
//! ## 宿主接缝 = 五项能力（都只有宿主做得了）
//!
//! | 方法 | 为什么在宿主 |
//! |---|---|
//! | [`SessionViewHost::state_snapshot`] | 全量快照要**跨全部人格**遍历会话 + 实例清单/状态 |
//! | [`SessionViewHost::context_snapshot`] | 上下文要从 `TrunkStore` 的历史里算 token |
//! | [`SessionViewHost::trunk_timeline`] | 显示时间线窗口存在 `TrunkStore`（限长、落盘、单调 seq） |
//! | [`SessionViewHost::timeline_earlier`] | 向上翻页读同一份窗口 |
//! | [`SessionViewHost::session_history`] | 派生要读**事件日志**（按会话分区，唯一事实来源） |
//! | [`SessionViewHost::active_turns`] | 运行中 turn 的集合由 turn 驱动维护 |
//!
//! 反过来：**线上瘦身（推理正文截断）在这里做**——它是展示规则，宿主不该
//! 知道"面板嫌推理正文太长"。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use echo_protocol::{BackendCommand, BackendEvent, TimelineMessage};

use crate::timeline::elide_reasoning_for_wire;

/// 本插件处理的命令名（都属"会话视图投影"，写操作不在此列）。
pub const COMMANDS: [&str; 5] = [
    "RequestState",
    "RequestContext",
    "RequestTrunkTimeline",
    "RequestTimelineEarlier",
    "RequestSessionHistory",
];

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 一次时间线请求的结果（**未瘦身**——瘦身是投影的事，见本模块文档）。
#[derive(Debug, Clone, Default)]
pub struct TimelineView {
    /// 时间升序的条目。
    pub messages: Vec<TimelineMessage>,
    /// 该 agent timeline 的最新单调序号（增量游标）。
    pub seq: u64,
    /// `true` = 完整快照（前端替换缓存）；`false` = 增量（追加/修补）。
    pub full: bool,
    /// `true` = 这不是全部（前端可向上翻页）。
    pub has_more: bool,
}

/// 向上翻页的一页。
#[derive(Debug, Clone, Default)]
pub struct EarlierView {
    /// 时间升序、`seq < before_ord` 的条目。
    pub messages: Vec<TimelineMessage>,
    /// 是否还有更早的条目。
    pub has_more: bool,
}

/// 按会话派生的显示历史。
#[derive(Debug, Clone, Default)]
pub struct HistoryView {
    /// 派生出的条目（时间升序）。
    pub messages: Vec<TimelineMessage>,
    /// 截断前的总数。
    pub total: usize,
    /// `true` = 因 limit 只含最近 limit 条。
    pub truncated: bool,
    /// 合并水位线：该会话在显示时间线里的最大 seq（0 = 不在窗口内）。
    pub max_seq: u64,
}

/// 宿主接缝：会话视图需要而插件不该知道的五项能力。
pub trait SessionViewHost: Send + Sync {
    /// 面板重连时的**全量状态快照**（按发出顺序排列的事件）。
    fn state_snapshot(&self) -> BoxFuture<'_, Vec<BackendEvent>>;
    /// 指定人格 / 会话的上下文快照。
    fn context_snapshot(
        &self,
        team_id: Option<String>,
        session_id: Option<String>,
    ) -> BoxFuture<'_, Vec<BackendEvent>>;
    /// 时间线（增量 / 全量 + 分页）。`None` = 本轮跳过（降级路径）。
    fn trunk_timeline(
        &self,
        team_id: Option<String>,
        since_seq: u64,
        limit: Option<usize>,
    ) -> BoxFuture<'_, Option<TimelineView>>;
    /// 向上翻页。
    fn timeline_earlier(
        &self,
        team_id: Option<String>,
        before_ord: u64,
        limit: Option<usize>,
    ) -> BoxFuture<'_, EarlierView>;
    /// 按会话派生显示历史。
    fn session_history(
        &self,
        team_id: Option<String>,
        session_id: String,
        limit: Option<usize>,
    ) -> BoxFuture<'_, HistoryView>;
    /// 正在执行 turn 的会话 id（随时间线快照补发）。
    fn active_turns(&self, team_id: Option<String>) -> BoxFuture<'_, Vec<String>>;
    /// 正在运行的回复分支（随时间线快照补发）。
    ///
    /// 默认空实现：TTL/测试宿主不必关心——但生产宿主必须实现，否则重连后
    /// 分支卡与任务卡仍然只能等新事件（正是本接口要消除的行为）。
    fn active_branches(
        &self,
        _team_id: Option<String>,
    ) -> BoxFuture<'_, Vec<echo_protocol::event::ActiveBranchInfo>> {
        Box::pin(async { Vec::new() })
    }
    /// 事件出口。
    fn emit(&self, event: BackendEvent);
}

/// 命令处理器。
pub struct SessionViewHandler {
    host: Arc<dyn SessionViewHost>,
}

impl SessionViewHandler {
    /// 用宿主接缝构造处理器。
    pub fn new(host: Arc<dyn SessionViewHost>) -> Self {
        Self { host }
    }

    /// 处理一条命令；`None` = 不是本域。
    pub async fn handle(
        &self,
        name: &str,
        payload: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        if !COMMANDS.contains(&name) {
            return None;
        }
        let tagged = serde_json::json!({ name: payload });
        let cmd: BackendCommand = match serde_json::from_value(tagged) {
            Ok(cmd) => cmd,
            Err(error) => {
                return Some(Err(format!(
                    "session-view 命令载荷解析失败（{name}）: {error}"
                )))
            }
        };
        Some(self.apply(cmd).await.map(|()| serde_json::Value::Null))
    }

    async fn apply(&self, cmd: BackendCommand) -> Result<(), String> {
        match cmd {
            BackendCommand::RequestState => {
                for event in self.host.state_snapshot().await {
                    self.host.emit(event);
                }
                Ok(())
            }
            BackendCommand::RequestContext {
                team_id,
                session_id,
            } => {
                for event in self.host.context_snapshot(team_id, session_id).await {
                    self.host.emit(event);
                }
                Ok(())
            }
            BackendCommand::RequestTrunkTimeline {
                team_id,
                since_seq,
                limit,
            } => {
                let Some(view) = self
                    .host
                    .trunk_timeline(team_id.clone(), since_seq, limit)
                    .await
                else {
                    // 降级路径：本轮不发（保持既有语义——跳过一轮同步比发
                    // 半份快照安全）。
                    tracing::warn!("timeline snapshot degraded; skip this sync round");
                    return Ok(());
                };
                let has_more = view.has_more;
                self.host.emit(BackendEvent::TrunkTimeline {
                    messages: elide_reasoning_for_wire(view.messages),
                    seq: view.seq,
                    team_id: team_id.clone(),
                    full: view.full,
                    has_more,
                });
                // 活跃 turn 快照（刷新恢复，2026-10）：运行状态本是瞬时事件流，
                // 前端刷新重连后仅靠 TrunkTimeline 看不到正在运行的会话（活动
                // 浮条 / 取消按钮丢失）。timeline 之后立刻补发——增量与全量都
                // 发，前端按快照重建 running 集合。
                let session_ids = self.host.active_turns(team_id.clone()).await;
                let branches = self.host.active_branches(team_id.clone()).await;
                self.host.emit(BackendEvent::ActiveTurnsSnapshot {
                    session_ids,
                    team_id,
                    branches,
                });
                Ok(())
            }
            BackendCommand::RequestTimelineEarlier {
                team_id,
                before_ord,
                limit,
            } => {
                let view = self
                    .host
                    .timeline_earlier(team_id.clone(), before_ord, limit)
                    .await;
                self.host.emit(BackendEvent::TimelineEarlier {
                    team_id,
                    // 与全量快照同一套线上瘦身（推理正文截断），保证前端拿到的
                    // 形状与首屏一致。
                    messages: elide_reasoning_for_wire(view.messages),
                    has_more: view.has_more,
                });
                Ok(())
            }
            BackendCommand::RequestSessionHistory {
                team_id,
                session_id,
                limit,
            } => {
                let view = self
                    .host
                    .session_history(team_id.clone(), session_id.clone(), limit)
                    .await;
                self.host.emit(BackendEvent::SessionHistory {
                    team_id,
                    session_id,
                    messages: view.messages,
                    total: view.total,
                    truncated: view.truncated,
                    max_seq: view.max_seq,
                });
                Ok(())
            }
            other => Err(format!(
                "session-view 命令域不处理 {:?}（注册关系与命令泵不一致）",
                other.kind_name()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_protocol::TimelineMessage;
    use std::sync::Mutex;

    /// 假宿主：断言"哪条命令读哪个接缝、发了哪些事件、顺序如何"。
    #[derive(Default)]
    struct FakeHost {
        state: Mutex<Vec<BackendEvent>>,
        context: Mutex<Vec<BackendEvent>>,
        timeline: Mutex<Option<TimelineView>>,
        earlier: Mutex<EarlierView>,
        history: Mutex<HistoryView>,
        turns: Mutex<Vec<String>>,
        events: Mutex<Vec<BackendEvent>>,
    }

    fn msg(seq: u64, kind: &str, content: &str) -> TimelineMessage {
        TimelineMessage {
            seq,
            kind: kind.into(),
            content: content.into(),
            session_id: String::new(),
            time: 0,
            source: None,
            reasoning: None,
            tool: None,
            images: None,
            ord: seq,
        }
    }

    impl SessionViewHost for FakeHost {
        fn state_snapshot(&self) -> BoxFuture<'_, Vec<BackendEvent>> {
            let v = self.state.lock().unwrap().clone();
            Box::pin(async move { v })
        }
        fn context_snapshot(
            &self,
            _team_id: Option<String>,
            _session_id: Option<String>,
        ) -> BoxFuture<'_, Vec<BackendEvent>> {
            let v = self.context.lock().unwrap().clone();
            Box::pin(async move { v })
        }
        fn trunk_timeline(
            &self,
            _team_id: Option<String>,
            _since_seq: u64,
            _limit: Option<usize>,
        ) -> BoxFuture<'_, Option<TimelineView>> {
            let v = self.timeline.lock().unwrap().clone();
            Box::pin(async move { v })
        }
        fn timeline_earlier(
            &self,
            _team_id: Option<String>,
            _before_ord: u64,
            _limit: Option<usize>,
        ) -> BoxFuture<'_, EarlierView> {
            let v = self.earlier.lock().unwrap().clone();
            Box::pin(async move { v })
        }
        fn session_history(
            &self,
            _team_id: Option<String>,
            _session_id: String,
            _limit: Option<usize>,
        ) -> BoxFuture<'_, HistoryView> {
            let v = self.history.lock().unwrap().clone();
            Box::pin(async move { v })
        }
        fn active_turns(&self, _team_id: Option<String>) -> BoxFuture<'_, Vec<String>> {
            let v = self.turns.lock().unwrap().clone();
            Box::pin(async move { v })
        }
        fn emit(&self, event: BackendEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn handler() -> (SessionViewHandler, Arc<FakeHost>) {
        let host = Arc::new(FakeHost::default());
        (SessionViewHandler::new(host.clone()), host)
    }

    #[tokio::test]
    async fn write_commands_are_not_claimed() {
        let (h, host) = handler();
        assert!(h.handle("ClearHistory", serde_json::json!({})).await.is_none());
        assert!(h.handle("SendMessage", serde_json::json!({})).await.is_none());
        assert!(host.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn state_snapshot_events_are_emitted_in_order() {
        let (h, host) = handler();
        *host.state.lock().unwrap() = vec![
            BackendEvent::ActiveTurnsSnapshot {
                session_ids: vec![],
                team_id: None,
                branches: vec![],
            },
            BackendEvent::TeamsList { teams: vec![] },
        ];
        h.handle("RequestState", serde_json::Value::Null)
            .await
            .unwrap()
            .unwrap();
        let names: Vec<&str> = host
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.kind_name())
            .collect();
        assert_eq!(names, ["ActiveTurnsSnapshot", "TeamsList"]);
    }

    #[tokio::test]
    async fn timeline_is_elided_here_and_followed_by_active_turns() {
        let (h, host) = handler();
        *host.timeline.lock().unwrap() = Some(TimelineView {
            // 超过"保留全文"阈值（`WIRE_REASONING_KEEP_FULL` = 40）后，
            // 较早的推理正文必须被截断——这一步属于展示规则，在插件里做。
            messages: (0..45)
                .map(|i| msg(i as u64, "reasoning", &"x".repeat(400)))
                .collect(),
            seq: 46,
            full: true,
            has_more: true,
        });
        *host.turns.lock().unwrap() = vec!["tui:local".into()];

        h.handle(
            "RequestTrunkTimeline",
            serde_json::json!({"team_id":"alix","since_seq":0,"limit":10}),
        )
        .await
        .unwrap()
        .unwrap();

        let events = host.events.lock().unwrap();
        assert_eq!(events.len(), 2, "timeline + active turns");
        match &events[0] {
            BackendEvent::TrunkTimeline {
                messages,
                seq,
                full,
                has_more,
                team_id,
            } => {
                assert_eq!(*seq, 46);
                assert!(*full);
                assert!(*has_more);
                assert_eq!(team_id.as_deref(), Some("alix"));
                // 瘦身生效：窗口外的推理正文被截断（不再等于原文长度）。
                assert!(
                    messages.iter().any(|m| m.content.len() < 400),
                    "线上瘦身必须在本插件里发生"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
        match &events[1] {
            BackendEvent::ActiveTurnsSnapshot {
                session_ids,
                team_id,
                ..
            } => {
                assert_eq!(session_ids, &vec!["tui:local".to_string()]);
                assert_eq!(team_id.as_deref(), Some("alix"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn degraded_timeline_emits_nothing() {
        let (h, host) = handler();
        *host.timeline.lock().unwrap() = None;
        h.handle("RequestTrunkTimeline", serde_json::json!({"since_seq":0}))
            .await
            .unwrap()
            .unwrap();
        assert!(
            host.events.lock().unwrap().is_empty(),
            "降级路径不发半份快照"
        );
    }

    #[tokio::test]
    async fn earlier_page_is_elided_too() {
        let (h, host) = handler();
        *host.earlier.lock().unwrap() = EarlierView {
            messages: (0..3).map(|i| msg(i, "user", "hi")).collect(),
            has_more: false,
        };
        h.handle(
            "RequestTimelineEarlier",
            serde_json::json!({"before_ord":5,"limit":200}),
        )
        .await
        .unwrap()
        .unwrap();
        let events = host.events.lock().unwrap();
        match &events[0] {
            BackendEvent::TimelineEarlier {
                messages, has_more, ..
            } => {
                assert_eq!(messages.len(), 3);
                assert!(!has_more);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_history_carries_the_watermark() {
        let (h, host) = handler();
        *host.history.lock().unwrap() = HistoryView {
            messages: vec![msg(7, "assistant", "yo")],
            total: 120,
            truncated: true,
            max_seq: 7,
        };
        h.handle(
            "RequestSessionHistory",
            serde_json::json!({"session_id":"tui:local","limit":500}),
        )
        .await
        .unwrap()
        .unwrap();
        let events = host.events.lock().unwrap();
        match &events[0] {
            BackendEvent::SessionHistory {
                session_id,
                total,
                truncated,
                max_seq,
                ..
            } => {
                assert_eq!(session_id, "tui:local");
                assert_eq!(*total, 120);
                assert!(*truncated);
                assert_eq!(*max_seq, 7);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_declared_command_is_dispatchable() {
        let (h, host) = handler();
        *host.timeline.lock().unwrap() = Some(TimelineView::default());
        let payloads = serde_json::json!({
            "RequestState": serde_json::Value::Null,
            "RequestContext": {"team_id":"t","session_id":"s"},
            "RequestTrunkTimeline": {"team_id":"t","since_seq":0,"limit":10},
            "RequestTimelineEarlier": {"before_ord":5,"limit":200},
            "RequestSessionHistory": {"session_id":"s","limit":500},
        });
        for name in COMMANDS {
            let out = h.handle(name, payloads[name].clone()).await;
            assert!(out.is_some(), "{name} 未被认领");
            assert!(out.unwrap().is_ok(), "{name} 解析失败");
        }
    }
}
