//! 事件桥接的**运行时**部分：进程内通道与多订阅者扇出。
//!
//! 帧形状与序列化（`WsMessage` / `SequencedEvent` / `serialize_*`）已上移到
//! `echo-protocol::envelope`；这里保留通道本体，并再导出线函数，使
//! `protocol::bridge::serialize_event` 这类旧路径继续可用。
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

pub use echo_protocol::envelope::{
    deserialize_message, serialize_command, serialize_event, serialize_event_identified,
    serialize_event_sequenced, SequencedEvent, WsMessage,
};

use tokio::sync::{mpsc, Mutex};

use echo_protocol::command::BackendCommand;
use echo_protocol::event::BackendEvent;

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
