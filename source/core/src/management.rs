//! Management WebSocket server for Panel (TUI) connections.
//!
//! Listens on a configurable address, accepts WS connections from Panel
//! instances, and bridges commands/events between the Panel and the Agent.
//!
//! Sudo password frames (`WsMessage::SudoPassword`) are routed **directly to
//! the sudo broker** — they never enter the agent command queue, the session
//! log, or the LLM context, and their payload is never logged.

use std::sync::Arc;

use anyhow::Context;
use echo_agent::{BackendBridge, BackendEvent};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

/// Broadcasts agent events to every connected Panel.
///
/// The in-process [`BackendBridge`] has a single event receiver; if each WS
/// connection drains that receiver directly, one Panel tab can starve the
/// others. Instead this broker owns the single receiver and fans each event
/// out to per-connection channels.
struct EventBroker {
    bridge: Arc<BackendBridge>,
    subscribers: Mutex<Vec<mpsc::UnboundedSender<BackendEvent>>>,
}

impl EventBroker {
    fn new(bridge: Arc<BackendBridge>) -> Self {
        Self {
            bridge,
            subscribers: Mutex::new(Vec::new()),
        }
    }

    async fn subscribe(&self) -> mpsc::UnboundedReceiver<BackendEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.lock().await.push(tx);
        rx
    }

    async fn run(&self) {
        let mut rx = self.bridge.event_rx.lock().await;
        while let Some(event) = rx.recv().await {
            let mut subscribers = self.subscribers.lock().await;
            subscribers.retain(|tx| tx.send(event.clone()).is_ok());
        }
    }
}

/// Start the management WS server.
pub async fn serve(
    addr: &str,
    bridge: Arc<BackendBridge>,
    agent: Arc<echo_agent::Agent>,
    sudo_broker: Arc<echo_agent::SudoBroker>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind management address {addr}"))?;
    info!("Panel management WS server listening on {addr}");
    serve_with_listener(listener, bridge, agent, sudo_broker).await
}

/// Serve on a pre-bound listener (used by tests to pick a free port).
pub async fn serve_with_listener(
    listener: TcpListener,
    bridge: Arc<BackendBridge>,
    agent: Arc<echo_agent::Agent>,
    sudo_broker: Arc<echo_agent::SudoBroker>,
) -> anyhow::Result<()> {
    let events = Arc::new(EventBroker::new(bridge.clone()));
    let event_runner = events.clone();
    tokio::spawn(async move {
        event_runner.run().await;
    });

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                warn!(error = %e, "accept failed");
                continue;
            }
        };
        info!(%peer, "Panel connected");

        let events = events.clone();
        let bridge = bridge.clone();
        let agent = agent.clone();
        let sudo_broker = sudo_broker.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, events, bridge, agent, sudo_broker).await {
                warn!(error = %e, "Panel connection error");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    events: Arc<EventBroker>,
    bridge: Arc<BackendBridge>,
    _agent: Arc<echo_agent::Agent>,
    sudo_broker: Arc<echo_agent::SudoBroker>,
) -> anyhow::Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (write, mut read) = ws.split();

    // Forward events from Agent → this Panel connection.
    //
    // Each connection has its own subscription channel, so one Panel can no
    // longer drain the shared receiver and starve the others. A dead socket
    // only drops this connection's subscriber.
    let mut event_rx = events.subscribe().await;
    let write_lock = Arc::new(Mutex::new(write));
    let forwarder = tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            let text = echo_agent::bridge::serialize_event(&ev);
            let mut w = write_lock.lock().await;
            if w.send(tokio_tungstenite::tungstenite::Message::Text(text))
                .await
                .is_err()
            {
                return; // client gone — subscriber is dropped on return
            }
        }
    });

    // Forward commands from Panel → Agent, and sudo passwords → broker.
    while let Some(Ok(msg)) = read.next().await {
        match msg {
            tokio_tungstenite::tungstenite::Message::Text(text) => {
                match handle_inbound_text(&text) {
                    Some(InboundFrame::Command(cmd)) => {
                        let _ = bridge.send_command(cmd);
                    }
                    Some(InboundFrame::SudoPassword(submit)) => {
                        // The password resolves the pending run_sudo oneshot
                        // directly; it is never logged or serialized into an
                        // event, and it never enters the agent command queue.
                        let accepted = sudo_broker.submit(submit.request_id, submit.password);
                        if !accepted {
                            warn!(
                                request_id = submit.request_id,
                                "sudo password submitted for unknown/expired request"
                            );
                        }
                    }
                    None => {}
                }
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => {}
        }
    }

    // Connection closed — stop the forwarder so this connection's subscriber
    // is removed and can no longer accumulate undelivered events.
    forwarder.abort();
    Ok(())
}

/// One parsed inbound frame from the Panel.
#[derive(Debug)]
pub(crate) enum InboundFrame {
    Command(echo_agent::BackendCommand),
    SudoPassword(echo_agent::bridge::SudoPasswordSubmit),
}

/// Parse one inbound WS text frame.
///
/// Returns `None` for non-command frames (events, malformed payloads) —
/// those are ignored by the management channel. Sudo password frames are
/// parsed with the redacted deserializer so a malformed frame cannot leak
/// the password into logs.
pub(crate) fn handle_inbound_text(text: &str) -> Option<InboundFrame> {
    if let Some(submit) = echo_agent::bridge::deserialize_sudo_password(text) {
        return Some(InboundFrame::SudoPassword(submit));
    }
    match echo_agent::bridge::deserialize_message(text) {
        Some(echo_agent::bridge::WsMessage::Command(cmd)) => Some(InboundFrame::Command(cmd)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_agent::BackendCommand;

    #[test]
    fn parses_command_frame() {
        let text =
            r#"{"type":"command","payload":{"SendMessage":{"session_id":"s1","content":"hi"}}}"#;
        match handle_inbound_text(text) {
            Some(InboundFrame::Command(cmd)) => {
                assert_eq!(
                    cmd,
                    BackendCommand::SendMessage {
                        session_id: "s1".into(),
                        content: "hi".into(),
                        images: vec![],
                        agent_id: None
                    }
                );
            }
            other => panic!("expected command frame, got {other:?}"),
        }
    }

    #[test]
    fn ignores_event_frames() {
        let text =
            r#"{"type":"event","payload":{"AgentOutput":{"session_id":"s1","content":"hi"}}}"#;
        assert!(handle_inbound_text(text).is_none());
    }

    #[test]
    fn ignores_malformed_frames() {
        assert!(handle_inbound_text("not json").is_none());
        assert!(handle_inbound_text("").is_none());
    }

    #[test]
    fn parses_sudo_password_frame() {
        let text = r#"{"type":"sudo_password","payload":{"request_id":42,"password":"hunter2"}}"#;
        match handle_inbound_text(text) {
            Some(InboundFrame::SudoPassword(submit)) => {
                assert_eq!(submit.request_id, 42);
                assert_eq!(submit.password.as_deref(), Some("hunter2"));
            }
            other => panic!("expected sudo frame, got {other:?}"),
        }
    }

    #[test]
    fn sudo_password_debug_redacts_payload() {
        let text = r#"{"type":"sudo_password","payload":{"request_id":7,"password":"hunter2"}}"#;
        let Some(InboundFrame::SudoPassword(submit)) = handle_inbound_text(text) else {
            panic!("expected sudo frame");
        };
        let debug = format!("{submit:?}");
        assert!(!debug.contains("hunter2"), "Debug must redact: {debug}");
    }

    #[test]
    fn gate_mode_command_roundtrips_with_enum() {
        // Wire format stays snake_case strings for GateMode.
        let text = r#"{"type":"command","payload":{"SetQqGateMode":{"mode":"allowlist"}}}"#;
        match handle_inbound_text(text) {
            Some(InboundFrame::Command(cmd)) => {
                assert_eq!(
                    cmd,
                    BackendCommand::SetQqGateMode {
                        mode: echo_adapter::GateMode::Allowlist
                    }
                );
            }
            other => panic!("expected command frame, got {other:?}"),
        }
    }

    // ── End-to-end WS server ──────────────────────────────────────────────

    /// A provider that is never actually called (management needs an Agent).
    struct DummyProvider;

    #[async_trait::async_trait]
    impl echo_agent::LlmProvider for DummyProvider {
        fn name(&self) -> &str {
            "dummy"
        }
        fn default_model(&self) -> &str {
            "dummy"
        }
        async fn chat(
            &self,
            _request: &echo_agent::llm::ChatRequest,
        ) -> Result<echo_agent::llm::ChatResponse, echo_agent::llm::LlmError> {
            unimplemented!()
        }
        async fn chat_stream(
            &self,
            _request: &echo_agent::llm::ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<echo_agent::llm::ChatChunk>,
        ) -> Result<(), echo_agent::llm::LlmError> {
            unimplemented!()
        }
    }

    fn dummy_agent() -> Arc<echo_agent::Agent> {
        Arc::new(echo_agent::Agent::new(
            Arc::new(DummyProvider),
            echo_agent::AgentConfig::default(),
            echo_agent::SkillRegistry::new(),
            echo_agent::ToolRegistry::new(),
            Arc::new(echo_adapter::AdapterRegistry::new()),
        ))
    }

    async fn start_test_server() -> (
        Arc<BackendBridge>,
        echo_agent::BackendHandle,
        String,
        Arc<echo_agent::SudoBroker>,
    ) {
        let (bridge, handle) = echo_agent::create_bridge();
        let bridge = Arc::new(bridge);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let agent = dummy_agent();
        let sudo_broker = Arc::new(echo_agent::SudoBroker::new());
        let b = bridge.clone();
        let sb = sudo_broker.clone();
        tokio::spawn(async move {
            let _ = serve_with_listener(listener, b, agent, sb).await;
        });
        (bridge, handle, addr, sudo_broker)
    }

    #[tokio::test]
    async fn panel_command_reaches_agent_backend() {
        let (bridge, handle, addr, _sb) = start_test_server().await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("connect");

        let text = echo_agent::bridge::serialize_command(&BackendCommand::SwitchModel {
            model: "gpt-x".into(),
        });
        ws.send(tokio_tungstenite::tungstenite::Message::Text(text))
            .await
            .expect("send");

        let received = handle
            .command_rx
            .lock()
            .await
            .recv()
            .await
            .expect("command delivered");
        assert_eq!(
            received,
            BackendCommand::SwitchModel {
                model: "gpt-x".into()
            }
        );
        let _ = bridge;
        ws.close(None).await.ok();
    }

    #[tokio::test]
    async fn agent_events_reach_panel() {
        let (bridge, handle, addr, _sb) = start_test_server().await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("connect");

        // Emit an event from the agent side; the server forwards it.
        handle.emit(echo_agent::BackendEvent::AgentOutput {
            session_id: "s1".into(),
            content: "回复".into(),
            branch_id: None,
        });

        // The forwarder polls every 100ms.
        let mut received = None;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = ws.next().await {
                if let Some(echo_agent::bridge::WsMessage::Event(ev)) =
                    echo_agent::bridge::deserialize_message(&text)
                {
                    received = Some(ev);
                    break;
                }
            }
        }
        let ev = received.expect("event forwarded to panel");
        match ev {
            echo_agent::BackendEvent::AgentOutput { content, .. } => assert_eq!(content, "回复"),
            other => panic!("wrong event: {other:?}"),
        }
        let _ = bridge;
        ws.close(None).await.ok();
    }

    #[tokio::test]
    async fn sudo_password_frame_resolves_broker_request() {
        let (_bridge, _handle, addr, sudo_broker) = start_test_server().await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("connect");

        // Register a pending request and submit the password over the wire.
        let pending = sudo_broker.request();
        let text =
            echo_agent::bridge::serialize_sudo_password(&echo_agent::bridge::SudoPasswordSubmit {
                request_id: pending.request_id,
                password: Some("s3cret".into()),
            });
        ws.send(tokio_tungstenite::tungstenite::Message::Text(text))
            .await
            .expect("send");

        let password = pending
            .into_receiver()
            .await
            .expect("broker request resolved via WS");
        assert_eq!(password.as_deref(), Some("s3cret"));
        ws.close(None).await.ok();
    }
}
