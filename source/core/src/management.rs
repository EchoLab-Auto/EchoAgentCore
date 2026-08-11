//! Management WebSocket server for Panel (TUI) connections.
//!
//! Listens on a configurable address, accepts WS connections from Panel
//! instances, and bridges commands/events between the Panel and the Agent.

use std::sync::Arc;

use anyhow::Context;
use echo_agent::BackendBridge;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Start the management WS server.
pub async fn serve(
    addr: &str,
    bridge: Arc<BackendBridge>,
    agent: Arc<echo_agent::Agent>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind management address {addr}"))?;
    info!("Panel management WS server listening on {addr}");
    serve_with_listener(listener, bridge, agent).await
}

/// Serve on a pre-bound listener (used by tests to pick a free port).
pub async fn serve_with_listener(
    listener: TcpListener,
    bridge: Arc<BackendBridge>,
    agent: Arc<echo_agent::Agent>,
) -> anyhow::Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                warn!(error = %e, "accept failed");
                continue;
            }
        };
        info!(%peer, "Panel connected");

        let bridge = bridge.clone();
        let agent = agent.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, bridge, agent).await {
                warn!(error = %e, "Panel connection error");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    bridge: Arc<BackendBridge>,
    _agent: Arc<echo_agent::Agent>,
) -> anyhow::Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (write, mut read) = ws.split();

    // Forward events from Agent → Panel.
    //
    // The event channel is shared by all connections, so the forwarder MUST
    // die with its connection: a leaked forwarder keeps draining events and
    // discarding them into the dead socket, starving every future Panel
    // connection. The shared rx lock is released before sending, so a blocked
    // write to a dead peer only stalls this connection, not the others.
    let write_lock = Arc::new(Mutex::new(write));
    let wl = write_lock.clone();
    let b = bridge.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            let events = {
                let mut rx = b.event_rx.lock().await;
                let mut events = Vec::new();
                while let Ok(ev) = rx.try_recv() {
                    events.push(ev);
                }
                events
            };
            if events.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
            let mut w = wl.lock().await;
            for ev in events {
                let text = echo_agent::bridge::serialize_event(&ev);
                if w.send(tokio_tungstenite::tungstenite::Message::Text(text))
                    .await
                    .is_err()
                {
                    return; // client gone — stop before stealing more events
                }
            }
        }
    });

    // Forward commands from Panel → Agent.
    while let Some(Ok(msg)) = read.next().await {
        match msg {
            tokio_tungstenite::tungstenite::Message::Text(text) => {
                if let Some(cmd) = handle_inbound_text(&text, &bridge) {
                    let _ = bridge.send_command(cmd);
                }
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => {}
        }
    }

    // Connection closed — stop the event forwarder so it stops draining the
    // shared event channel (see note above).
    forwarder.abort();
    Ok(())
}

/// Parse one inbound WS text frame into a command.
///
/// Returns `None` for non-command frames (events, malformed payloads) —
/// those are ignored by the management channel.
pub(crate) fn handle_inbound_text(
    text: &str,
    _bridge: &BackendBridge,
) -> Option<echo_agent::BackendCommand> {
    match echo_agent::bridge::deserialize_message(text) {
        Some(echo_agent::bridge::WsMessage::Command(cmd)) => Some(cmd),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_agent::BackendCommand;

    #[test]
    fn parses_command_frame() {
        let (bridge, _handle) = echo_agent::create_bridge();
        let text =
            r#"{"type":"command","payload":{"SendMessage":{"session_id":"s1","content":"hi"}}}"#;
        let cmd = handle_inbound_text(text, &bridge).expect("command parsed");
        assert_eq!(
            cmd,
            BackendCommand::SendMessage {
                session_id: "s1".into(),
                content: "hi".into()
            }
        );
    }

    #[test]
    fn ignores_event_frames() {
        let (bridge, _handle) = echo_agent::create_bridge();
        let text =
            r#"{"type":"event","payload":{"AgentOutput":{"session_id":"s1","content":"hi"}}}"#;
        assert!(handle_inbound_text(text, &bridge).is_none());
    }

    #[test]
    fn ignores_malformed_frames() {
        let (bridge, _handle) = echo_agent::create_bridge();
        assert!(handle_inbound_text("not json", &bridge).is_none());
        assert!(handle_inbound_text("", &bridge).is_none());
    }

    #[test]
    fn gate_mode_command_roundtrips_with_enum() {
        // Wire format stays snake_case strings for GateMode.
        let (bridge, _handle) = echo_agent::create_bridge();
        let text = r#"{"type":"command","payload":{"SetQqGateMode":{"mode":"allowlist"}}}"#;
        let cmd = handle_inbound_text(text, &bridge).expect("command parsed");
        assert_eq!(
            cmd,
            BackendCommand::SetQqGateMode {
                mode: echo_adapter::GateMode::Allowlist
            }
        );
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

    async fn start_test_server() -> (Arc<BackendBridge>, echo_agent::BackendHandle, String) {
        let (bridge, handle) = echo_agent::create_bridge();
        let bridge = Arc::new(bridge);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let agent = dummy_agent();
        let b = bridge.clone();
        tokio::spawn(async move {
            let _ = serve_with_listener(listener, b, agent).await;
        });
        (bridge, handle, addr)
    }

    #[tokio::test]
    async fn panel_command_reaches_agent_backend() {
        let (bridge, handle, addr) = start_test_server().await;
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
        let (bridge, handle, addr) = start_test_server().await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("connect");

        // Emit an event from the agent side; the server forwards it.
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
}
