//! Communication bridge between agent backend and frontends (TUI / API).
//!
//! Supports two transports:
//! - **In-process** (mpsc channels) — used when Core and Panel run in the same
//!   process (backward-compatible).
//! - **WebSocket** — used when Core and Panel run as separate processes.
//!
//! ## WebSocket wire format
//!
//! ```json
//! // Panel → Core
//! {"type":"command","command":{"SendMessage":{"session_id":"...","content":"..."}}}
//!
//! // Core → Panel
//! {"type":"event","event":{"AgentOutput":{"session_id":"...","content":"...","branch_id":null}}}
//! ```

use tokio::sync::{mpsc, Mutex};

use crate::command::BackendCommand;
use crate::event::BackendEvent;

// ── In-process bridge ──────────────────────────────────────────────────────

/// Frontend side (held by the TUI).
#[derive(Debug)]
pub struct BackendBridge {
    pub command_tx: mpsc::UnboundedSender<BackendCommand>,
    pub event_rx: Mutex<mpsc::UnboundedReceiver<BackendEvent>>,
}

impl BackendBridge {
    pub fn send_command(&self, cmd: BackendCommand) -> bool {
        self.command_tx.send(cmd).is_ok()
    }
}

/// Backend side (held by the agent).
pub struct BackendHandle {
    pub event_tx: mpsc::UnboundedSender<BackendEvent>,
    pub command_rx: Mutex<mpsc::UnboundedReceiver<BackendCommand>>,
}

impl BackendHandle {
    pub fn emit(&self, event: BackendEvent) {
        let _ = self.event_tx.send(event);
    }

    pub fn try_recv_command(&self) -> Option<BackendCommand> {
        self.command_rx.try_lock().ok()?.try_recv().ok()
    }
}

/// Create an in-process bridge pair.
pub fn create_bridge() -> (BackendBridge, BackendHandle) {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    (
        BackendBridge {
            command_tx,
            event_rx: Mutex::new(event_rx),
        },
        BackendHandle {
            event_tx,
            command_rx: Mutex::new(command_rx),
        },
    )
}

// ── Multi-subscriber handle ────────────────────────────────────────────────

/// A handle that fans out events to all connected frontends (Panels).
pub struct FanoutHandle {
    senders: Mutex<Vec<mpsc::UnboundedSender<BackendEvent>>>,
    command_tx: mpsc::UnboundedSender<BackendCommand>,
    command_rx: Mutex<mpsc::UnboundedReceiver<BackendCommand>>,
}

impl Default for FanoutHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl FanoutHandle {
    pub fn new() -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        Self {
            senders: Mutex::new(Vec::new()),
            command_tx,
            command_rx: Mutex::new(command_rx),
        }
    }

    /// Register a new subscriber. Returns an event receiver (for forwarding
    /// to the subscriber) and a command sender (for the subscriber to send
    /// commands back to the agent).
    pub async fn subscribe(
        &self,
    ) -> (
        mpsc::UnboundedReceiver<BackendEvent>,
        mpsc::UnboundedSender<BackendCommand>,
    ) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.senders.lock().await.push(event_tx);
        (event_rx, self.command_tx.clone())
    }

    /// Emit an event to all subscribers.
    ///
    /// Dead subscribers (dropped receivers — e.g. a Panel that disconnected
    /// without unsubscribing) are removed so the channel list cannot grow
    /// unboundedly. The channels themselves stay unbounded: dropping events
    /// would lose conversation data.
    pub async fn emit(&self, event: BackendEvent) {
        let mut senders = self.senders.lock().await;
        senders.retain(|tx| tx.send(event.clone()).is_ok());
    }

    /// Try to receive a command without blocking.
    pub fn try_recv_command(&self) -> Option<BackendCommand> {
        self.command_rx.try_lock().ok()?.try_recv().ok()
    }

    /// Receive commands (for task-based consumption).
    pub async fn recv_command(&self) -> Option<BackendCommand> {
        self.command_rx.lock().await.recv().await
    }

    /// Get a sender for commands (to share with subscribers).
    pub fn command_sender(&self) -> mpsc::UnboundedSender<BackendCommand> {
        self.command_tx.clone()
    }
}

// ── WebSocket message types ────────────────────────────────────────────────

/// 扇出序号信封：每条下发到 Panel 的事件都带一个**进程内单调递增**的序号。
///
/// 用途（2026-10）：Panel 侧据此做**事件流连续性校验**——序号一旦跳号，
/// 说明中间有事件没到（中继半死、连接重建窗口、后台节流……）。前端随即按
/// 游标补拉一次 timeline 修复，而不是让某个 `ToolResult` 静默丢失、工具行
/// 永远停在「运行中」。
///
/// 序号由 Core 的 WS 扇出（`management::EventBroker`）在**持有订阅者表的
/// 锁时**分配并投递，因此所有订阅者看到的顺序与序号完全一致。
#[derive(Debug, Clone)]
pub struct SequencedEvent {
    /// 单调递增序号（从 1 开始；一次进程生命周期内唯一）。
    pub seq: u64,
    /// 条目身份（`EntryId`；见 `core::entry_identity`）。
    ///
    /// 与 `seq` 同为**信封层**字段：`{"type":"event","payload":{…},"seq":N,"entry":"…"}`。
    /// 语义是"这条事件属于哪个逻辑条目"——同一段流式输出的所有增量与它的
    /// 终值共享同一个 id，工具调用与结果同理。Panel 据此把实时行与时间线
    /// 权威条目**精确**对齐，不必再靠「同角色 + 同时间 + 内容前缀」去猜。
    ///
    /// `None` = 该事件没有条目身份（生命周期事件、或 `tool_call_id` 缺失的
    /// 旧版工具事件）。
    pub entry: Option<String>,
    pub event: BackendEvent,
}

/// Top-level WS message envelope.
///
/// Serialises as:
/// ```json
/// {"type":"command","payload":{"SendMessage":{"session_id":"...","content":"..."}}}
/// {"type":"event","payload":{"AgentOutput":{"session_id":"...","content":"..."}}}
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum WsMessage {
    Command(BackendCommand),
    Event(BackendEvent),
}

/// Serialise a command for WS transport.
///
/// On failure, logs the error and returns an empty string (the receiver
/// treats empty frames as protocol errors).
pub fn serialize_command(cmd: &BackendCommand) -> String {
    serde_json::to_string(&WsMessage::Command(cmd.clone())).unwrap_or_else(|e| {
        tracing::error!(error = %e, "failed to serialize command for WS");
        String::new()
    })
}

/// Serialise an event for WS transport.
///
/// On failure, logs the error and returns an empty string.
pub fn serialize_event(ev: &BackendEvent) -> String {
    serialize_event_with_seq(None, ev)
}

/// Serialise an event **with its fanout sequence number** for WS transport.
///
/// 序号作为顶层字段附加在事件帧上（`{"type":"event","payload":{…},"seq":N}`）：
/// 放在顶层而不是塞进 `BackendEvent` 各变体，是为了不动事件契约——新增
/// 变体不需要记得带序号，也不会让旧版 Panel 的严格反序列化失败（serde 对
/// internally-tagged enum 的多余字段是忽略的，见下方测试）。
///
/// 前端据此校验事件流连续性；缺帧即按游标补拉 timeline（见
/// `web/src/event-seq.ts`）。
pub fn serialize_event_sequenced(seq: u64, ev: &BackendEvent) -> String {
    serialize_event_with_meta(Some(seq), None, ev)
}

/// Serialise an event with its fanout sequence **and** entry identity.
///
/// 条目身份同样作为顶层字段附加（`entry`），理由与 `seq` 相同：不动
/// `BackendEvent` 各变体的契约（那会让 70+ 处构造点都要改），旧版 Panel 对
/// 多余字段是忽略的。
pub fn serialize_event_identified(seq: u64, entry: Option<&str>, ev: &BackendEvent) -> String {
    serialize_event_with_meta(Some(seq), entry, ev)
}

fn serialize_event_with_seq(seq: Option<u64>, ev: &BackendEvent) -> String {
    serialize_event_with_meta(seq, None, ev)
}

fn serialize_event_with_meta(seq: Option<u64>, entry: Option<&str>, ev: &BackendEvent) -> String {
    if seq.is_none() && entry.is_none() {
        return serde_json::to_string(&WsMessage::Event(ev.clone())).unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to serialize event for WS");
            String::new()
        });
    }
    let mut value = match serde_json::to_value(WsMessage::Event(ev.clone())) {
        Ok(value) => value,
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize event for WS");
            return String::new();
        }
    };
    if let Some(obj) = value.as_object_mut() {
        if let Some(seq) = seq {
            obj.insert("seq".to_string(), serde_json::Value::from(seq));
        }
        if let Some(entry) = entry {
            obj.insert(
                "entry".to_string(),
                serde_json::Value::from(entry.to_string()),
            );
        }
    }
    value.to_string()
}

/// Deserialise a WS text message. Logs malformed input for diagnosis.
pub fn deserialize_message(text: &str) -> Option<WsMessage> {
    match serde_json::from_str(text) {
        Ok(msg) => Some(msg),
        Err(e) => {
            let snippet: String = text.chars().take(200).collect();
            tracing::warn!(error = %e, "failed to deserialize WS message: {snippet}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::SessionInfo;

    #[test]
    fn command_roundtrip_via_ws() {
        let cmd = BackendCommand::SendMessage {
            session_id: "qq:group:123:456".into(),
            content: "你好".into(),
            images: vec![],
            team_id: None,
        };
        let text = serialize_command(&cmd);
        assert!(text.contains("\"type\":\"command\""));
        let msg = deserialize_message(&text).unwrap();
        match msg {
            WsMessage::Command(c) => match c {
                BackendCommand::SendMessage {
                    session_id,
                    content,
                    images,
                    team_id,
                } => {
                    assert!(images.is_empty());
                    assert!(team_id.is_none());
                    assert_eq!(session_id, "qq:group:123:456");
                    assert_eq!(content, "你好");
                }
                other => panic!("wrong command variant: {other:?}"),
            },
            other => panic!("wrong message type: {other:?}"),
        }
    }

    #[test]
    fn event_roundtrip_via_ws() {
        let ev = BackendEvent::AgentOutput {
            session_id: "user_0".into(),
            team_id: None,
            content: "回复内容".into(),
            branch_id: Some("branch-1".into()),
        };
        let text = serialize_event(&ev);
        assert!(text.contains("\"type\":\"event\""));
        assert!(text.contains("branch_id"));
        let msg = deserialize_message(&text).unwrap();
        match msg {
            WsMessage::Event(e) => match e {
                BackendEvent::AgentOutput {
                    session_id,
                    team_id,
                    content,
                    branch_id,
                } => {
                    assert!(team_id.is_none());
                    assert_eq!(session_id, "user_0");
                    assert_eq!(content, "回复内容");
                    assert_eq!(branch_id.as_deref(), Some("branch-1"));
                }
                other => panic!("wrong event variant: {other:?}"),
            },
            other => panic!("wrong message type: {other:?}"),
        }
    }

    #[test]
    fn execution_lifecycle_events_roundtrip_via_ws() {
        let events = [
            BackendEvent::SubagentStarted {
                session_id: "s1".into(),
                task: "compare designs".into(),
            },
            BackendEvent::SubagentCompleted {
                session_id: "s1".into(),
                success: true,
            },
            BackendEvent::AgentReasoning {
                session_id: "s1".into(),
                team_id: None,
                branch_id: "b1".into(),
                content: "considering options".into(),
            },
            BackendEvent::ReplyBranchStarted {
                session_id: "s1".into(),
                branch_id: "b1".into(),
                message_sequence: 2,
                task: "reply".into(),
                target: "QQ 私聊 1".into(),
                started_at_ms: 10,
            },
            BackendEvent::ReplyBranchContent {
                session_id: "s1".into(),
                branch_id: "b1".into(),
                content: "still working".into(),
            },
            BackendEvent::ReplyBranchCompleted {
                session_id: "s1".into(),
                branch_id: "b1".into(),
                message_sequence: 2,
                success: true,
                cancelled: false,
                completed_at_ms: 20,
            },
            BackendEvent::AgentCompleted {
                session_id: "s1".into(),
            },
        ];
        for event in events {
            let text = serialize_event(&event);
            let decoded = deserialize_message(&text).expect("lifecycle event should decode");
            match decoded {
                WsMessage::Event(BackendEvent::SubagentStarted { session_id, .. })
                | WsMessage::Event(BackendEvent::SubagentCompleted { session_id, .. })
                | WsMessage::Event(BackendEvent::AgentReasoning { session_id, .. })
                | WsMessage::Event(BackendEvent::ReplyBranchStarted { session_id, .. })
                | WsMessage::Event(BackendEvent::ReplyBranchContent { session_id, .. })
                | WsMessage::Event(BackendEvent::ReplyBranchCompleted { session_id, .. })
                | WsMessage::Event(BackendEvent::AgentCompleted { session_id }) => {
                    assert_eq!(session_id, "s1");
                }
                other => panic!("wrong lifecycle event: {other:?}"),
            }
        }
    }

    #[test]
    fn session_updated_event_roundtrip() {
        let session = SessionInfo {
            id: "qq:group:123:456".into(),
            node_id: Some("node-test".into()),
            region_name: Some("workstation".into()),
            team_id: None,
            platform: "qq".into(),
            scope: "group".into(),
            user_id: "456".into(),
            nickname: "alice".into(),
            group_name: Some("测试群".into()),
            last_active: 1700000000,
            last_message: "hello".into(),
        };
        let ev = BackendEvent::SessionUpdated { session };
        let text = serialize_event(&ev);
        let msg = deserialize_message(&text).unwrap();
        match msg {
            WsMessage::Event(BackendEvent::SessionUpdated { session }) => {
                assert_eq!(session.id, "qq:group:123:456");
                assert_eq!(session.node_id.as_deref(), Some("node-test"));
                assert_eq!(session.nickname, "alice");
                assert_eq!(session.group_name.as_deref(), Some("测试群"));
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn malformed_message_returns_none() {
        assert!(deserialize_message("not json").is_none());
        assert!(deserialize_message("{\"type\":\"unknown\"}").is_none());
    }

    #[tokio::test]
    async fn fanout_handle_default_and_subscribe() {
        let handle = FanoutHandle::default();
        let (ev_rx, cmd_tx) = handle.subscribe().await;
        // Emit an event and verify it reaches the subscriber.
        handle
            .emit(BackendEvent::AgentOutput {
                session_id: "s1".into(),
                team_id: None,
                content: "hi".into(),
                branch_id: None,
            })
            .await;
        let mut rx = ev_rx;
        let ev = rx.try_recv().expect("event should be delivered");
        match ev {
            BackendEvent::AgentOutput { content, .. } => assert_eq!(content, "hi"),
            other => panic!("wrong event: {other:?}"),
        }
        let _ = cmd_tx;
    }

    #[tokio::test]
    async fn fanout_emits_to_all_subscribers() {
        let handle = FanoutHandle::default();
        let (mut rx1, _) = handle.subscribe().await;
        let (mut rx2, _) = handle.subscribe().await;
        handle
            .emit(BackendEvent::AgentOutput {
                session_id: "s".into(),
                team_id: None,
                content: "x".into(),
                branch_id: None,
            })
            .await;
        assert!(rx1.try_recv().is_ok(), "subscriber 1 gets the event");
        assert!(rx2.try_recv().is_ok(), "subscriber 2 gets the event");
    }

    #[test]
    fn sequenced_serialization_keeps_event_contract() {
        let ev = BackendEvent::AgentThinking {
            session_id: "s1".into(),
            team_id: None,
        };
        let text = serialize_event_sequenced(42, &ev);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value.get("type").and_then(|v| v.as_str()), Some("event"));
        assert_eq!(value.get("seq").and_then(|v| v.as_u64()), Some(42));
        // 顶层加 seq 后仍能被既有反序列化路径解析（多余字段忽略）。
        match deserialize_message(&text) {
            Some(WsMessage::Event(BackendEvent::AgentThinking { session_id, .. })) => {
                assert_eq!(session_id, "s1")
            }
            other => panic!("sequenced frame should still decode: {other:?}"),
        }
        // 不带序号的旧格式照旧可用（序列化/反序列化对称）。
        let plain = serialize_event(&ev);
        assert!(!plain.contains("\"seq\""), "旧格式不应带 seq");
        assert!(deserialize_message(&plain).is_some());
    }

    /// 条目身份作为**信封层**字段下发（2026-10 条目身份化，P1 第一步）。
    ///
    /// 为什么在信封而不是事件变体里：`BackendEvent` 有 70+ 处构造点，加字段
    /// 会让每一处都要改；而顶层字段对旧版 Panel 是无害的多余键（与 `seq`
    /// 同一取舍，见 `entry_identity` 模块注释）。
    #[test]
    fn identified_serialization_carries_entry() {
        let ev = BackendEvent::AgentContentDelta {
            session_id: "s1".into(),
            team_id: None,
            branch_id: "b1".into(),
            delta: "hi".into(),
        };
        let text = serialize_event_identified(7, Some("backend:s1:b1:3"), &ev);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value.get("type").and_then(|v| v.as_str()), Some("event"));
        assert_eq!(value.get("seq").and_then(|v| v.as_u64()), Some(7));
        assert_eq!(
            value.get("entry").and_then(|v| v.as_str()),
            Some("backend:s1:b1:3")
        );
        // 事件体本身不含 entry（契约未变）
        let payload = value.get("payload").unwrap();
        assert!(payload.get("AgentContentDelta").is_some());
        assert!(!text.contains("\"entry\":\"backend") || payload.get("entry").is_none());
        // 仍能被既有反序列化路径解析（多余顶层字段忽略）
        match deserialize_message(&text) {
            Some(WsMessage::Event(BackendEvent::AgentContentDelta { delta, .. })) => {
                assert_eq!(delta, "hi")
            }
            other => panic!("identified frame should still decode: {other:?}"),
        }
    }

    #[test]
    fn identified_serialization_omits_entry_when_absent() {
        let ev = BackendEvent::AgentThinking {
            session_id: "s1".into(),
            team_id: None,
        };
        let text = serialize_event_identified(9, None, &ev);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value.get("seq").and_then(|v| v.as_u64()), Some(9));
        assert!(
            value.get("entry").is_none(),
            "无身份时不写该键（不产生空串）"
        );
        // 旧入口行为不变
        let legacy = serialize_event_sequenced(9, &ev);
        let legacy_value: serde_json::Value = serde_json::from_str(&legacy).unwrap();
        assert!(legacy_value.get("entry").is_none());
        assert!(!serialize_event(&ev).contains("\"seq\""));
    }

    #[tokio::test]
    async fn fanout_prunes_dead_subscribers() {
        let handle = FanoutHandle::default();
        let (dead_rx, _) = handle.subscribe().await;
        let (mut live_rx, _) = handle.subscribe().await;
        // Kill the first subscriber.
        drop(dead_rx);

        // A few emits: the dead sender must be pruned, the live one served.
        for i in 0..3 {
            handle
                .emit(BackendEvent::AgentOutput {
                    session_id: "s".into(),
                    team_id: None,
                    content: format!("m{i}"),
                    branch_id: None,
                })
                .await;
        }
        match live_rx.try_recv().unwrap() {
            BackendEvent::AgentOutput { session_id, .. } => assert_eq!(session_id, "s"),
            other => panic!("wrong event: {other:?}"),
        }
        assert!(
            handle.senders.lock().await.len() <= 1,
            "dead subscriber must be pruned"
        );
    }

    #[tokio::test]
    async fn fanout_command_channel_reaches_agent() {
        let handle = FanoutHandle::default();
        let (_, cmd_tx) = handle.subscribe().await;
        cmd_tx
            .send(BackendCommand::SwitchModel {
                model: "gpt-x".into(),
            })
            .unwrap();
        assert_eq!(
            handle.try_recv_command(),
            Some(BackendCommand::SwitchModel {
                model: "gpt-x".into()
            })
        );
    }
}
