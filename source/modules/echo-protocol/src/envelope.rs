//! 管理面 WS 信封：命令/事件帧的形状、序号与条目身份的附加位置。
//!
//! 纯 serde，不含任何通道或任务——运行时桥接（`BackendBridge` /
//! `FanoutHandle`）留在 `agent-module-management`。
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

use crate::command::BackendCommand;
use crate::event::BackendEvent;

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
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
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
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
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
            files: vec![],
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
                    files,
                    team_id,
                } => {
                    assert!(images.is_empty());
                    assert!(files.is_empty());
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
                profile: None,
                model: None,
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
}
