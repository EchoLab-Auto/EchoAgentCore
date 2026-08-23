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

/// Sudo password submission, Panel → Core, carried on a **dedicated channel**.
///
/// The password never transits the agent command queue, the session log, or
/// the LLM context: the management server routes `WsMessage::SudoPassword`
/// straight to the sudo broker, and `Debug` redacts the password so a stray
/// `{:?}` cannot leak it into logs.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SudoPasswordSubmit {
    pub request_id: u64,
    /// `Some(password)` authorizes; `None` denies the request.
    pub password: Option<String>,
}

impl std::fmt::Debug for SudoPasswordSubmit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SudoPasswordSubmit")
            .field("request_id", &self.request_id)
            .field(
                "password",
                &if self.password.is_some() {
                    "***"
                } else {
                    "None"
                },
            )
            .finish()
    }
}

/// Top-level WS message envelope.
///
/// Serialises as:
/// ```json
/// {"type":"command","payload":{"SendMessage":{"session_id":"...","content":"..."}}}
/// {"type":"event","payload":{"AgentOutput":{"session_id":"...","content":"..."}}}
/// {"type":"sudo_password","payload":{"request_id":1,"password":"***"}}
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum WsMessage {
    Command(BackendCommand),
    Event(BackendEvent),
    /// Panel → Core only. Routed directly to the sudo broker by the
    /// management server; never enters the agent command queue.
    SudoPassword(SudoPasswordSubmit),
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
    serde_json::to_string(&WsMessage::Event(ev.clone())).unwrap_or_else(|e| {
        tracing::error!(error = %e, "failed to serialize event for WS");
        String::new()
    })
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

/// Serialise a sudo password submission for WS transport.
///
/// This is a **secret-carrying frame**: on failure only the error is logged,
/// never the payload (the password must not appear in logs).
pub fn serialize_sudo_password(submit: &SudoPasswordSubmit) -> String {
    match serde_json::to_string(&WsMessage::SudoPassword(submit.clone())) {
        Ok(text) => text,
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize sudo password frame");
            String::new()
        }
    }
}

/// Deserialise a sudo password submission from a WS text message.
///
/// On failure only the error is logged — never the raw text, which may
/// contain the password.
pub fn deserialize_sudo_password(text: &str) -> Option<SudoPasswordSubmit> {
    match serde_json::from_str::<WsMessage>(text) {
        Ok(WsMessage::SudoPassword(submit)) => Some(submit),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "failed to deserialize sudo password frame");
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
                } => {
                    assert!(images.is_empty());
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
                    content,
                    branch_id,
                } => {
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
    fn legacy_agent_output_without_branch_id_still_decodes() {
        // Old Core versions serialised AgentOutput without the branch_id field.
        // The field is #[serde(default)], so old payloads must decode to None.
        let text =
            r#"{"type":"event","payload":{"AgentOutput":{"session_id":"s1","content":"hi"}}}"#;
        let msg = deserialize_message(text).expect("legacy payload must decode");
        match msg {
            WsMessage::Event(BackendEvent::AgentOutput {
                session_id,
                content,
                branch_id,
            }) => {
                assert_eq!(session_id, "s1");
                assert_eq!(content, "hi");
                assert_eq!(branch_id, None);
            }
            other => panic!("wrong event: {other:?}"),
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
    fn sudo_password_frame_roundtrips() {
        let submit = SudoPasswordSubmit {
            request_id: 7,
            password: Some("s3cret".into()),
        };
        let text = serialize_sudo_password(&submit);
        assert!(text.contains("\"type\":\"sudo_password\""));
        let decoded = deserialize_sudo_password(&text).expect("sudo frame decodes");
        assert_eq!(decoded.request_id, 7);
        assert_eq!(decoded.password.as_deref(), Some("s3cret"));
        // The password never appears in Debug output.
        let debug = format!("{decoded:?}");
        assert!(
            !debug.contains("s3cret"),
            "Debug must redact the password: {debug}"
        );
    }

    #[test]
    fn sudo_password_deny_roundtrips() {
        let submit = SudoPasswordSubmit {
            request_id: 9,
            password: None,
        };
        let text = serialize_sudo_password(&submit);
        let decoded = deserialize_sudo_password(&text).expect("deny frame decodes");
        assert_eq!(decoded.password, None);
    }

    #[test]
    fn sudo_password_frame_parses_as_sudo_variant_only() {
        let submit = SudoPasswordSubmit {
            request_id: 1,
            password: Some("x".into()),
        };
        let text = serialize_sudo_password(&submit);
        // The generic parser yields the sudo variant; the dedicated parser
        // extracts the submission.
        match deserialize_message(&text) {
            Some(WsMessage::SudoPassword(parsed)) => assert_eq!(parsed.request_id, 1),
            other => panic!("expected SudoPassword variant, got {other:?}"),
        }
        assert_eq!(
            deserialize_sudo_password(&text).map(|s| s.request_id),
            Some(1)
        );
        assert!(deserialize_sudo_password("not json").is_none());
        assert!(deserialize_sudo_password("{\"type\":\"command\"}").is_none());
    }

    #[tokio::test]
    async fn fanout_handle_default_and_subscribe() {
        let handle = FanoutHandle::default();
        let (ev_rx, cmd_tx) = handle.subscribe().await;
        // Emit an event and verify it reaches the subscriber.
        handle
            .emit(BackendEvent::AgentOutput {
                session_id: "s1".into(),
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
                content: "x".into(),
                branch_id: None,
            })
            .await;
        assert!(rx1.try_recv().is_ok(), "subscriber 1 gets the event");
        assert!(rx2.try_recv().is_ok(), "subscriber 2 gets the event");
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
