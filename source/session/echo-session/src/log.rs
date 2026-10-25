//! The append-only event log and its durable persistence.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event::SessionEvent;
use crate::LOG_FORMAT_VERSION;

/// Persistence errors.
#[derive(Debug, Error)]
pub enum EventLogError {
    #[error("log read failed: {0}")]
    Read(String),
    #[error("log parse failed: {0}")]
    Parse(String),
    #[error("log serialize failed: {0}")]
    Serialize(String),
    #[error("log write failed: {0}")]
    Write(String),
    #[error("unsupported log version {0}")]
    UnsupportedVersion(u32),
}

/// An append-only, per-session event log.
///
/// Events are only appended; compaction is expressed as a
/// [`SessionEvent::Compaction`] event (a summary replacing a prefix), never
/// by mutating or deleting earlier events. The log is the single source of
/// truth — the model context is derived from it, never stored alongside it.
///
/// Persistence is a whole-log JSON document (atomic tmp+rename), versioned by
/// [`LOG_FORMAT_VERSION`].
#[derive(Debug, Clone, Default)]
pub struct EventLog {
    events: Arc<Mutex<Vec<SessionEvent>>>,
}

impl EventLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one event (durable on the next save).
    pub fn append(&self, event: SessionEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }

    /// Append many events in order.
    pub fn extend(&self, events: impl IntoIterator<Item = SessionEvent>) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(events);
    }

    /// Read the full log, oldest first.
    ///
    /// **深克隆整份日志**——只用于确实需要全量的场景（持久化序列化、压缩
    /// 预览、全量重投影）。每次 append 后热路径上的单会话投影请用
    /// [`Self::events_for_session`]，否则代价是 O(全部会话的事件数)
    /// （实测：13 个会话 / 4 MB 日志时，每步工具调用要多拷 4 MB）。
    pub fn log(&self) -> Vec<SessionEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 只克隆归属 `session_id` 的事件（顺序与全量日志一致）。
    ///
    /// 单会话投影（`TrunkStore::reproject`）的热路径：过滤在锁内走一遍指针
    /// 比较，只有匹配的事件才深克隆。全量日志里通常有多个会话（同人格下
    /// 每个工作区/工作流一条），按会话取能把克隆量降到 1/N。
    ///
    /// 语义与 `log().into_iter().filter(|e| e.session() == Some(id))` 完全
    /// 一致——包括 `session == None` 的未归因旧事件**不会**被任何会话取到
    /// （与 `derive_messages_for` 的过滤条件同义）。
    pub fn events_for_session(&self, session_id: &str) -> Vec<SessionEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|event| event.session() == Some(session_id))
            .cloned()
            .collect()
    }

    /// 持锁只读访问全量事件（**不克隆**）。
    ///
    /// 用于"读一遍就走"的场景（序列化到磁盘）。注意闭包在锁内执行：不要
    /// 在里面做重 I/O，也**不要**再调用本 `EventLog` 的任何加锁方法
    /// （`parking_lot::Mutex` 不可重入 → 死锁）。
    pub fn with_events<T>(&self, f: impl FnOnce(&[SessionEvent]) -> T) -> T {
        let events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&events)
    }

    /// Insert an event after the last `UserMessage` carrying `sequence`.
    ///
    /// Concurrent branches merge their replies back into the log in request
    /// order: a reply for request `N` is inserted directly after that
    /// request's user event, so the projected order matches the requests'
    /// arrival order even when replies finish out of order. Falls back to
    /// appending when no matching user event exists.
    ///
    /// A tool turn appends `ToolCall`/`ToolResult` events after its user
    /// event; the reply is inserted past them so the projected order never
    /// shows the final reply before the tool round it answers.
    pub fn insert_after_sequence(&self, sequence: u64, event: SessionEvent) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let insert_at = events.iter().rposition(|existing| match existing {
            SessionEvent::UserMessage(message) => message.message_sequence == Some(sequence),
            _ => false,
        });
        match insert_at {
            Some(index) => {
                let mut insert_at = index + 1;
                // 跳过本轮的工具往返、推理**与模型往返观测**：后两者是
                // 显示层/审计条目，读起来属于它们前面那条请求，最终回答
                // 必须排在整轮之后（否则回答会插到 thinking 或 step 之前）。
                while insert_at < events.len()
                    && matches!(
                        events[insert_at],
                        SessionEvent::ToolCall(_)
                            | SessionEvent::ToolResult(_)
                            | SessionEvent::Reasoning(_)
                            | SessionEvent::ModelStep(_)
                    )
                {
                    insert_at += 1;
                }
                events.insert(insert_at, event);
            }
            None => events.push(event),
        }
    }

    pub fn len(&self) -> usize {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Drop every event. Callers clearing conversation memory use this
    /// together with resetting the derived projections.
    pub fn clear(&self) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// 原子整体替换（单次取锁）——压缩落地等「读-改-写」场景的
    /// 正确姿势：调用方先按当前 `log()` 计算新日志，再一次性替换。
    /// 此前 `clear()` + `extend()` 分两次取锁，中间窗口里并发读者
    /// 会看到空日志（2026-10 压缩巡检 🔴）。
    pub fn replace_all(&self, events: Vec<SessionEvent>) {
        *self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = events;
    }

    /// Atomically read-modify-write the whole log under the internal lock.
    ///
    /// The closure receives the current events; return `Some((replacement,
    /// outcome))` to commit `replacement` as the new log (returning `outcome`
    /// to the caller) or `None` to leave the log untouched. Concurrent
    /// appends are ordered against the transform: they either land before it
    /// (visible to the closure) or after it (appended to the replacement) —
    /// an update can never be lost between a separate read and a later write
    /// (the window that a clone-then-`replace_all` pair leaves open).
    pub fn transform<F, T>(&self, f: F) -> Option<T>
    where
        F: FnOnce(&[SessionEvent]) -> Option<(Vec<SessionEvent>, T)>,
    {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (replacement, outcome) = f(&events)?;
        *events = replacement;
        Some(outcome)
    }

    /// 原子删除**归属指定会话**的全部事件（单次取锁），返回删除条数。
    ///
    /// 用于「清理当前会话记忆」——绝不动其他会话（2026-10 用户报告：
    /// 面板「清理历史记忆」实为全量清空，同人格下其他会话记忆一并被
    /// 抹掉）。`session == None` 的旧版事件**不删**（无归属信息，
    /// 宁保守不误删）；调用方语义上应配合 `session_id` 精确传入。
    pub fn retain_excluding_session(&self, session_id: &str) -> usize {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = events.len();
        events.retain(|event| event.session() != Some(session_id));
        before - events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Serialize the whole log (the durable representation).
    pub fn serialize(&self) -> Result<String, EventLogError> {
        // 持锁序列化（`with_events`）而不是先 `log()` 全量克隆：省掉一次
        // MB 级深拷。锁在序列化期间被持有——调用方若在意"别阻塞 append"
        // （如 30s 周期落盘），自己先 `log()` 克隆再序列化（`persist.rs`
        // 就是这么做的），代价是那次克隆。
        self.with_events(|events| {
            serde_json::to_string_pretty(&LogDocument {
                version: LOG_FORMAT_VERSION,
                events: events.to_vec(),
            })
        })
        .map_err(|e| EventLogError::Serialize(e.to_string()))
    }

    /// Parse a serialized log document.
    pub fn deserialize(data: &str) -> Result<Vec<SessionEvent>, EventLogError> {
        let doc: LogDocument =
            serde_json::from_str(data).map_err(|e| EventLogError::Parse(e.to_string()))?;
        if doc.version != LOG_FORMAT_VERSION {
            return Err(EventLogError::UnsupportedVersion(doc.version));
        }
        Ok(doc.events)
    }

    /// Atomically persist the log to `path` (tmp + rename).
    pub fn save_to(&self, path: &Path) -> Result<(), EventLogError> {
        let data = self.serialize()?;
        let tmp = format!("{}.tmp", path.display());
        std::fs::write(&tmp, &data).map_err(|e| EventLogError::Write(e.to_string()))?;
        std::fs::rename(&tmp, path).map_err(|e| EventLogError::Write(e.to_string()))?;
        Ok(())
    }

    /// Load a log from `path`; a missing file yields an empty log.
    pub fn load_from(&self, path: &Path) -> Result<Vec<SessionEvent>, EventLogError> {
        let data = std::fs::read_to_string(path).map_err(|e| EventLogError::Read(e.to_string()))?;
        Self::deserialize(&data)
    }

    /// The persistence path convention for a session id.
    pub fn path_for(dir: &Path, session_id: &str) -> PathBuf {
        // Session ids contain ':' which is invalid on Windows; sanitize.
        let safe = session_id.replace(':', "_");
        dir.join(format!("{safe}.json"))
    }
}

/// On-disk envelope for the event log.
#[derive(Debug, Serialize, Deserialize)]
struct LogDocument {
    version: u32,
    events: Vec<SessionEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AssistantMessage, UserMessage};

    fn sample_log() -> EventLog {
        let log = EventLog::new();
        log.append(SessionEvent::UserMessage(UserMessage {
            session: None,
            content: "你好".into(),
            timestamp: 1700000000,
            message_sequence: None,
            source: None,
            images: vec![],
        }));
        log.append(SessionEvent::AssistantMessage(AssistantMessage {
            timestamp: 0,
            session: None,
            content: "回复".into(),
            reasoning_content: None,
            tool_calls: vec![],
        }));
        log
    }

    #[test]
    fn append_and_read_in_order() {
        let log = sample_log();
        assert_eq!(log.len(), 2);
        let events = log.log();
        assert_eq!(events[0].kind(), "user/message");
        assert_eq!(events[1].kind(), "assistant/message");
    }

    #[test]
    fn transform_commits_or_keeps_atomically() {
        let log = sample_log();
        // Some: 提交替换并返回 outcome。
        let seen = log.transform(|events| {
            assert_eq!(events.len(), 2);
            Some((events.iter().take(1).cloned().collect(), events.len()))
        });
        assert_eq!(seen, Some(2));
        assert_eq!(log.len(), 1, "replacement committed");
        // None: 日志保持不变。
        let untouched = log.transform(|_| None::<(Vec<SessionEvent>, ())>);
        assert_eq!(untouched, None);
        assert_eq!(log.len(), 1, "None leaves the log untouched");
    }

    #[test]
    fn insert_after_sequence_skips_tool_events() {
        use crate::event::{ToolCallEvent, ToolResultEvent};
        let log = EventLog::new();
        log.append(SessionEvent::UserMessage(UserMessage {
            session: None,
            content: "跑一下".into(),
            timestamp: 1700000000,
            message_sequence: Some(1),
            source: None,
            images: vec![],
        }));
        log.append(SessionEvent::ToolCall(ToolCallEvent {
            session: None,
            id: "call_1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
            started_at_ms: None,
        }));
        log.append(SessionEvent::ToolResult(ToolResultEvent {
            session: None,
            tool_call_id: "call_1".into(),
            result: "ok".into(),
            images: vec![],
            elapsed_ms: None,
        }));
        // The final reply must land after the turn's own tool events, not
        // between the user event and them.
        log.insert_after_sequence(
            1,
            SessionEvent::AssistantMessage(AssistantMessage {
                timestamp: 0,
                session: None,
                content: "完成".into(),
                reasoning_content: None,
                tool_calls: vec![],
            }),
        );
        let events = log.log();
        assert_eq!(events.len(), 4);
        assert!(matches!(events[1], SessionEvent::ToolCall(_)));
        assert!(matches!(events[2], SessionEvent::ToolResult(_)));
        assert!(matches!(events[3], SessionEvent::AssistantMessage(_)));
    }

    // ── 按会话取事件 / 持锁只读（2026-10 性能）──────────────────────────

    fn user_in(session: &str, content: &str) -> SessionEvent {
        SessionEvent::UserMessage(UserMessage {
            session: Some(session.into()),
            content: content.into(),
            timestamp: 1_700_000_000,
            message_sequence: None,
            source: None,
            images: vec![],
        })
    }

    #[test]
    fn events_for_session_filters_and_preserves_order() {
        let log = EventLog::new();
        log.append(user_in("a", "a1"));
        log.append(user_in("b", "b1"));
        log.append(user_in("a", "a2"));
        log.append(SessionEvent::UserMessage(UserMessage {
            session: None, // 未归因旧事件
            content: "legacy".into(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
        }));
        log.append(user_in("a", "a3"));

        let a = log.events_for_session("a");
        let contents: Vec<&str> = a
            .iter()
            .map(|e| match e {
                SessionEvent::UserMessage(m) => m.content.as_str(),
                _ => "?",
            })
            .collect();
        assert_eq!(contents, vec!["a1", "a2", "a3"], "顺序与全量日志一致");

        // 未归因事件不属于任何会话
        assert_eq!(log.events_for_session("legacy").len(), 0);
        // 未知会话
        assert!(log.events_for_session("nope").is_empty());
        // 与"全量过滤"语义完全等价
        let manual: Vec<SessionEvent> = log
            .log()
            .into_iter()
            .filter(|e| e.session() == Some("a"))
            .collect();
        assert_eq!(a, manual);
    }

    #[test]
    fn with_events_reads_without_cloning() {
        let log = EventLog::new();
        log.append(user_in("a", "x"));
        log.append(user_in("b", "y"));
        // 只读统计：拿到的是 &[SessionEvent]，不产生副本
        let count = log.with_events(|events| events.len());
        assert_eq!(count, 2);
        let sessions: Vec<Option<String>> = log.with_events(|events| {
            events
                .iter()
                .map(|e| e.session().map(str::to_string))
                .collect()
        });
        assert_eq!(sessions.len(), 2);
        // 读操作不改内容
        assert_eq!(log.len(), 2);
    }

    /// 基准：多会话日志下，按会话取事件的克隆量应显著小于全量克隆。
    ///
    /// 断言用**字节量**而非墙钟（CI 上时间抖动太大）：构造 13 个会话、
    /// 每个 100 条带正文的事件，比较两种取法的总字节数。
    #[test]
    fn events_for_session_clones_only_matching_events() {
        let log = EventLog::new();
        // 会话 id 等长（s00..s12）：让各会话的字节量严格相等，断言才能精确
        let sessions: Vec<String> = (0..13).map(|i| format!("s{i:02}")).collect();
        for round in 0..100 {
            for session in &sessions {
                log.append(user_in(
                    session,
                    &format!("{session}-{round}-{}", "x".repeat(200)),
                ));
            }
        }
        let total_bytes = |events: &[SessionEvent]| -> usize {
            events
                .iter()
                .map(|e| match e {
                    SessionEvent::UserMessage(m) => m.content.len(),
                    _ => 0,
                })
                .sum()
        };

        let full = log.with_events(total_bytes);
        let one = log
            .events_for_session("s00")
            .iter()
            .map(|e| match e {
                SessionEvent::UserMessage(m) => m.content.len(),
                _ => 0,
            })
            .sum::<usize>();

        assert_eq!(log.len(), 1300);
        assert_eq!(one, full / 13, "单会话应正好占总量的 1/13");
        // 优化目标：克隆量从 O(全部) 降到 O(本会话)
        assert!(
            one * 10 < full,
            "按会话取事件应显著小于全量克隆：one={one} full={full}"
        );
    }

    #[test]
    fn insert_after_sequence_skips_model_step_events() {
        // 模型往返观测（2026-10-19）也属于本轮：最终回答必须排在它之后，
        // 否则投影会变成 [用户, 回答, assistant(tool_use), tool]——回答跑
        // 到工具调用前面（显示与上下文双双错位）。
        use crate::event::ToolCallEvent;
        let log = EventLog::new();
        log.append(SessionEvent::UserMessage(UserMessage {
            session: None,
            content: "跑一下".into(),
            timestamp: 1_700_000_000,
            message_sequence: Some(1),
            source: None,
            images: vec![],
        }));
        log.append(SessionEvent::ModelStep(crate::event::ModelStepEvent {
            step_index: 0,
            model: "m".into(),
            prompt_tokens: 1,
            completion_tokens: 1,
            tool_call_ids: vec!["call_1".into()],
            turn: None,
            elapsed_ms: Some(1),
            started_at_ms: 1,
            session: None,
        }));
        log.append(SessionEvent::ToolCall(ToolCallEvent {
            session: None,
            id: "call_1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
            started_at_ms: None,
        }));
        log.append(SessionEvent::ToolResult(crate::event::ToolResultEvent {
            session: None,
            tool_call_id: "call_1".into(),
            result: "ok".into(),
            images: vec![],
            elapsed_ms: None,
        }));
        log.insert_after_sequence(
            1,
            SessionEvent::AssistantMessage(AssistantMessage {
                timestamp: 0,
                session: None,
                content: "完成".into(),
                reasoning_content: None,
                tool_calls: vec![],
            }),
        );
        let events = log.log();
        assert_eq!(events.len(), 5);
        assert!(matches!(events[1], SessionEvent::ModelStep(_)));
        assert!(matches!(events[2], SessionEvent::ToolCall(_)));
        assert!(matches!(events[3], SessionEvent::ToolResult(_)));
        assert!(
            matches!(events[4], SessionEvent::AssistantMessage(_)),
            "{events:?}"
        );
    }

    #[test]
    fn insert_after_sequence_skips_reasoning_of_that_turn() {
        // 推理也是本轮事件：最终回答必须排在它之后，不能插到推理前面
        // （否则显示历史里回答会出现在"思考"之前）。
        use crate::event::ToolCallEvent;
        let log = EventLog::new();
        log.append(SessionEvent::UserMessage(UserMessage {
            session: None,
            content: "跑一下".into(),
            timestamp: 1_700_000_000,
            message_sequence: Some(1),
            source: None,
            images: vec![],
        }));
        log.append(SessionEvent::Reasoning(crate::event::ReasoningEvent {
            content: "思考".into(),
            timestamp: 100,
            session: None,
        }));
        log.append(SessionEvent::ToolCall(ToolCallEvent {
            session: None,
            id: "call_1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
            started_at_ms: None,
        }));
        log.insert_after_sequence(
            1,
            SessionEvent::AssistantMessage(AssistantMessage {
                timestamp: 0,
                session: None,
                content: "完成".into(),
                reasoning_content: None,
                tool_calls: vec![],
            }),
        );
        let events = log.log();
        assert_eq!(events.len(), 4);
        assert!(matches!(events[1], SessionEvent::Reasoning(_)));
        assert!(matches!(events[2], SessionEvent::ToolCall(_)));
        assert!(matches!(events[3], SessionEvent::AssistantMessage(_)));
    }

    #[test]
    fn reasoning_event_serde_roundtrip_and_backward_compat() {
        // 新变体落盘后能读回；缺 timestamp/session 的旧写法也能解析。
        let log = EventLog::new();
        log.append(SessionEvent::Reasoning(crate::event::ReasoningEvent {
            content: "思考".into(),
            timestamp: 1_700_000_000,
            session: Some("s1".into()),
        }));
        let data = log.serialize().unwrap();
        // 落盘用 `kind` 标签（pretty JSON，别依赖空格形态）
        let doc: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(
            doc.pointer("/events/0/kind").and_then(|v| v.as_str()),
            Some("reasoning"),
            "{data}"
        );
        assert_eq!(EventLog::deserialize(&data).unwrap(), log.log());

        let minimal = r#"{"version":5,"events":[{"kind":"reasoning","content":"旧数据"}]}"#;
        let parsed = EventLog::deserialize(minimal).unwrap();
        match &parsed[0] {
            SessionEvent::Reasoning(event) => {
                assert_eq!(event.content, "旧数据");
                assert_eq!(event.timestamp, 0);
                assert!(event.session.is_none());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn serialize_deserialize_roundtrip() {
        let log = sample_log();
        let data = log.serialize().unwrap();
        let restored = EventLog::deserialize(&data).unwrap();
        assert_eq!(restored, log.log());
    }

    #[test]
    fn save_and_load_via_file() {
        let log = sample_log();
        let path =
            std::env::temp_dir().join(format!("echo-session-log-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        log.save_to(&path).unwrap();
        let loaded = EventLog::new();
        let restored = loaded.load_from(&path).unwrap();
        assert_eq!(restored.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let err = EventLog::deserialize(r#"{"version": 99, "events": []}"#).unwrap_err();
        assert!(matches!(err, EventLogError::UnsupportedVersion(99)));
    }

    #[test]
    fn malformed_document_is_parse_error() {
        assert!(matches!(
            EventLog::deserialize("not json"),
            Err(EventLogError::Parse(_))
        ));
    }

    #[test]
    fn path_for_sanitizes_colons() {
        let dir = Path::new("/tmp");
        let path = EventLog::path_for(dir, "qq:dm::123");
        assert_eq!(path, PathBuf::from("/tmp/qq_dm__123.json"));
    }
}
