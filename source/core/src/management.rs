//! Management WebSocket server for Panel connections.
//!
//! Listens on a configurable address, accepts WS connections from Panel
//! instances, and bridges commands/events between the Panel and the Agent.

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

/// Start the management server with an optional bearer token.
pub async fn serve_with_token(
    addr: &str,
    bridge: Arc<BackendBridge>,
    agent: Arc<echo_agent::Agent>,
    access_token: String,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind management address {addr}"))?;
    // 裸奔提醒：无 token 且绑定在非回环地址时，任何能到达该端口的人
    // 都能以管理权限控制 Agent——启动时明确 warn。
    if access_token.is_empty() && !is_loopback_addr(addr) {
        warn!(
            addr,
            "management_access_token 为空且管理 WS 绑定在非回环地址—— \
             任何可访问该端口的客户端都可全权控制 Agent（建议配置 token 或改绑 127.0.0.1）"
        );
    }
    info!("Panel management WS server listening on {addr}");
    serve_with_listener_and_token(listener, bridge, agent, access_token).await
}

/// 绑定地址是否为回环（`127.0.0.1`/`localhost`/`::1`）。
fn is_loopback_addr(addr: &str) -> bool {
    let host = addr
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(addr)
        .trim_matches(['[', ']']);
    host == "127.0.0.1" || host == "localhost" || host == "::1"
}

/// 常量时间字符串相等：逐字节 XOR 累积，长度不等也走完全程再判，
/// 避免时序侧信道逐字节探测 token。
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

/// 未授权（令牌缺失/不匹配）的握手拒绝响应。
///
/// 状态**必须是 4xx**（这里用 401）：`ErrorResponse::new` 的默认状态是
/// 200 OK，而 tungstenite 对「成功状态」的握手错误会直接放弃写出任何字节
/// （`CustomResponseSuccessful`），连接被静默关闭——客户端（Panel/TUI）
/// 只能看到「Handshake not finished」，把「令牌不对」误诊为协议/网络故障
/// （2026-10 云机接入实况：端口映射 + 令牌缺失时极难排查）。显式 401 后，
/// 客户端收到标准 HTTP 响应并归类为认证失败、给出明确原因。
fn unauthorized_response() -> tokio_tungstenite::tungstenite::handshake::server::ErrorResponse {
    let mut response = tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(Some(
        "unauthorized".into(),
    ));
    *response.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
    response
}

/// Serve on a pre-bound listener (tests use it to pick a free port).
#[cfg(test)]
pub(crate) async fn serve_with_listener(
    listener: TcpListener,
    bridge: Arc<BackendBridge>,
    agent: Arc<echo_agent::Agent>,
) -> anyhow::Result<()> {
    serve_with_listener_and_token(listener, bridge, agent, String::new()).await
}

async fn serve_with_listener_and_token(
    listener: TcpListener,
    bridge: Arc<BackendBridge>,
    agent: Arc<echo_agent::Agent>,
    access_token: String,
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
        let access_token = access_token.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, events, bridge, agent, &access_token).await {
                warn!(error = %e, "Panel connection error");
            }
        });
    }
}

// 回调签名由 tokio-tungstenite 的 `accept_hdr_async` 固定：Err 侧是库的
// `ErrorResponse`（`http::Response<Option<String>>`），无法装箱缩小。
#[allow(clippy::result_large_err)]
async fn handle_connection(
    stream: tokio::net::TcpStream,
    events: Arc<EventBroker>,
    bridge: Arc<BackendBridge>,
    _agent: Arc<echo_agent::Agent>,
    access_token: &str,
) -> anyhow::Result<()> {
    let ws = if access_token.is_empty() {
        tokio_tungstenite::accept_async(stream).await?
    } else {
        tokio_tungstenite::accept_hdr_async(
            stream,
            |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                let authorized = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(|value| constant_time_eq(value, &format!("Bearer {access_token}")))
                    .unwrap_or(false);
                if authorized {
                    Ok(response)
                } else {
                    Err(unauthorized_response())
                }
            },
        )
        .await?
    };
    let (mut write, mut read) = ws.split();

    // Each connection has its own subscription channel, so one Panel can no
    // longer drain the shared receiver and starve the others. A dead socket
    // only drops this connection's subscriber.
    let mut event_rx = events.subscribe().await;

    // 心跳保活：30s 一次 Ping（tungstenite 在读侧自动回 Pong；任何入站帧
    // 都刷新 last_seen）。超过 90s 无任何入站活动即判死并断开，避免空闲
    // 连接被中间层悄悄断开后服务端永远悬挂。
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(30));
    heartbeat.tick().await; // 首次 tick 立即返回，跳过
    let mut last_seen = std::time::Instant::now();

    loop {
        tokio::select! {
            // Agent → Panel 事件转发。
            ev = event_rx.recv() => {
                match ev {
                    Some(ev) => {
                        let text = echo_agent::bridge::serialize_event(&ev);
                        if write
                            .send(tokio_tungstenite::tungstenite::Message::Text(text))
                            .await
                            .is_err()
                        {
                            break; // client gone — subscriber is dropped on return
                        }
                    }
                    None => break, // bridge closed
                }
            }
            // Panel → Agent 命令。
            msg = read.next() => {
                match msg {
                    Some(Ok(msg)) => {
                        last_seen = std::time::Instant::now();
                        match msg {
                            tokio_tungstenite::tungstenite::Message::Text(text) => {
                                match handle_inbound_text(&text) {
                                    Some(InboundFrame::Command(cmd)) => {
                                        let _ = bridge.send_command(cmd);
                                    }
                                    None => {}
                                }
                            }
                            tokio_tungstenite::tungstenite::Message::Close(_) => break,
                            _ => {}
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > std::time::Duration::from_secs(90) {
                    break; // 对端已死（无 Pong/任何入站帧）
                }
                if write
                    .send(tokio_tungstenite::tungstenite::Message::Ping(Vec::new()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }

    Ok(())
}

/// One parsed inbound frame from the Panel.
#[derive(Debug)]
pub(crate) enum InboundFrame {
    Command(echo_agent::BackendCommand),
}

/// Parse one inbound WS text frame.
///
/// Returns `None` for non-command frames (events, malformed payloads) —
/// those are ignored by the management channel.
pub(crate) fn handle_inbound_text(text: &str) -> Option<InboundFrame> {
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
                        team_id: None
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
    fn gate_mode_command_roundtrips_with_enum() {
        // Wire format stays snake_case strings for GateMode.
        let text = r#"{"type":"command","payload":{"SetQqGateMode":{"mode":"allowlist"}}}"#;
        match handle_inbound_text(text) {
            Some(InboundFrame::Command(cmd)) => {
                assert_eq!(
                    cmd,
                    BackendCommand::SetQqGateMode {
                        adapter: None,
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
        ) -> Result<echo_agent::llm::ChatResponse, echo_agent::llm::LlmError> {
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
        handle.emit(echo_agent::BackendEvent::AgentOutput {
            session_id: "s1".into(),
            team_id: None,
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

    /// 未授权拒绝必须是标准 HTTP 401 响应（回归，2026-10）：
    /// 此前 `ErrorResponse::new(Some("unauthorized"))` 的默认状态是 200，
    /// tungstenite 对成功状态直接放弃写出（CustomResponseSuccessful）——
    /// 客户端只看到连接被静默关闭（"Handshake not finished"），把「令牌
    /// 缺失/不匹配」误诊为协议或网络故障（云机端口映射场景实况）。
    #[tokio::test]
    async fn unauthorized_panel_gets_http_401_not_silent_close() {
        let (bridge, _handle) = echo_agent::create_bridge();
        let bridge = Arc::new(bridge);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let agent = dummy_agent();
        tokio::spawn(async move {
            let _ =
                serve_with_listener_and_token(listener, bridge, agent, "secret-token".into()).await;
        });

        // 1) 不带令牌：必须收到 HTTP 401（而非 0 字节静默断开）。
        let error = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect_err("unauthorized handshake must fail");
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert_eq!(
                    response.status(),
                    tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED,
                    "unauthorized rejection must be an explicit HTTP 401"
                );
            }
            other => panic!("expected HTTP 401 error, got {other:?}"),
        }

        // 2) 令牌错误：同样是 401（可归类为认证失败）。
        let mut request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                format!("ws://{addr}").as_str(),
            )
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer wrong-token".parse().unwrap());
        let error = tokio_tungstenite::connect_async(request)
            .await
            .expect_err("wrong token must fail");
        assert!(
            matches!(
                error,
                tokio_tungstenite::tungstenite::Error::Http(ref resp)
                    if resp.status() == tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED
            ),
            "wrong-token rejection must be HTTP 401 too"
        );

        // 3) 正确令牌：正常升级（101）。
        let mut request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                format!("ws://{addr}").as_str(),
            )
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer secret-token".parse().unwrap());
        let ws = tokio_tungstenite::connect_async(request)
            .await
            .expect("authorized handshake must succeed");
        drop(ws);
    }
}
