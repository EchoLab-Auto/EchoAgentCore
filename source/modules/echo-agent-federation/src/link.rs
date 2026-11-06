//! EchoAgent 的联邦协议适配：通用连接层负责 socket 与生命周期，本层只负责
//! Core 的 `FedFrame`、能力元数据和 Panel 身份。

use std::sync::Arc;

use echo_fabric_federation as fabric;
use fabric::WireFrame;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};

use crate::frame::{FedFrame, NodeCaps, NodeHello};
use crate::panel::{PanelAccess, PanelGate, PanelIdentity};
use crate::FederationError;

pub use fabric::{PeerConfig, PeerDiagnostics};
pub type LinkHandle = fabric::LinkHandle<FedFrame>;

/// 保留既有 Core 线协议：控制帧走通用外壳，业务帧仍是原来的 `FedFrame`。
impl fabric::Frame for FedFrame {
    fn encode_wire(frame: &WireFrame<Self>) -> Result<String, serde_json::Error> {
        match frame {
            WireFrame::Application(frame) => serde_json::to_string(frame),
            control => serde_json::to_string(control),
        }
    }

    fn decode_wire(text: &str) -> Result<WireFrame<Self>, serde_json::Error> {
        let legacy: FedFrame = serde_json::from_str(text)?;
        Ok(match legacy {
            FedFrame::Hello(hello) => WireFrame::Hello(to_fabric_hello(hello)),
            FedFrame::Welcome(hello) => WireFrame::Welcome(to_fabric_hello(hello)),
            FedFrame::Ping { sent_at_ms } => WireFrame::Ping { sent_at_ms },
            FedFrame::Pong {
                sent_at_ms,
                echo_ms,
            } => WireFrame::Pong {
                sent_at_ms,
                echo_ms,
            },
            FedFrame::Error { message, .. } => WireFrame::Error { message },
            application => WireFrame::Application(application),
        })
    }
}

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub node_id: String,
    pub node_name: Option<String>,
    pub version: String,
    pub caps: NodeCaps,
    pub advertise: Option<String>,
    pub peer_name: String,
}

#[derive(Debug, Clone)]
pub enum LinkEvent {
    Up(PeerInfo),
    Down {
        peer_node_id: String,
        reason: String,
    },
    Frame {
        from: String,
        frame: FedFrame,
    },
}

fn to_fabric_hello(hello: NodeHello) -> fabric::NodeHello {
    fabric::NodeHello {
        node_id: hello.node_id,
        node_name: hello.node_name,
        protocol_version: hello.protocol_version,
        version: hello.version,
        caps: serde_json::to_value(hello.caps).unwrap_or_default(),
        advertise: hello.advertise,
    }
}

fn from_fabric_info(info: fabric::PeerInfo) -> PeerInfo {
    PeerInfo {
        node_id: info.node_id,
        node_name: info.node_name,
        version: info.version,
        caps: serde_json::from_value(info.caps).unwrap_or_default(),
        advertise: info.advertise,
        peer_name: info.peer_name,
    }
}

fn from_fabric_event(event: fabric::LinkEvent<FedFrame>) -> LinkEvent {
    match event {
        fabric::LinkEvent::Up(info) => LinkEvent::Up(from_fabric_info(info)),
        fabric::LinkEvent::Down {
            peer_node_id,
            reason,
        } => LinkEvent::Down {
            peer_node_id,
            reason,
        },
        fabric::LinkEvent::Frame { from, frame } => LinkEvent::Frame { from, frame },
    }
}

type LoadReporter = Arc<dyn Fn() -> u32 + Send + Sync>;
type ToolReporter = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

pub struct Federation {
    inner: Arc<fabric::Federation<FedFrame>>,
    initial_caps: NodeCaps,
    load_reporter: Arc<std::sync::RwLock<Option<LoadReporter>>>,
    tool_reporter: Arc<std::sync::RwLock<Option<ToolReporter>>>,
    panel_gate: PanelGate,
}

impl Federation {
    pub fn new(
        node_id: String,
        node_name: Option<String>,
        caps: NodeCaps,
        peers: Vec<PeerConfig>,
        advertise: Option<String>,
        event_tx: mpsc::Sender<LinkEvent>,
        shutdown: watch::Sender<bool>,
    ) -> Arc<Self> {
        let (inner_tx, mut inner_rx) = mpsc::channel(256);
        let inner = fabric::Federation::new(
            node_id,
            node_name,
            env!("CARGO_PKG_VERSION").to_string(),
            serde_json::to_value(&caps).unwrap_or_default(),
            peers,
            advertise,
            inner_tx,
            shutdown,
        );
        let fed = Arc::new(Self {
            inner,
            initial_caps: caps,
            load_reporter: Arc::new(std::sync::RwLock::new(None)),
            tool_reporter: Arc::new(std::sync::RwLock::new(None)),
            panel_gate: PanelGate::default(),
        });
        let forward = async move {
            while let Some(event) = inner_rx.recv().await {
                if event_tx.send(from_fabric_event(event)).await.is_err() {
                    break;
                }
            }
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(forward);
        } else {
            // 配置解析等同步单测也会构造 Federation；此时不能无条件 tokio::spawn。
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("federation event forwarder runtime")
                    .block_on(forward);
            });
        }
        fed
    }

    async fn refresh_capability_reporter(&self) {
        let initial = self.initial_caps.clone();
        let load = self.load_reporter.clone();
        let tools = self.tool_reporter.clone();
        let panel = self.panel_gate.clone();
        self.inner
            .set_capability_reporter(Arc::new(move || {
                let mut caps = initial.clone();
                if let Some(reporter) = load
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                {
                    caps.active_turns = reporter();
                }
                if let Some(reporter) = tools
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                {
                    caps.tools = reporter();
                }
                caps.panel_ready = panel.is_open();
                serde_json::to_value(caps).unwrap_or_default()
            }))
            .await;
    }

    pub async fn set_load_reporter(&self, reporter: LoadReporter) {
        *self
            .load_reporter
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reporter);
        self.refresh_capability_reporter().await;
    }

    pub async fn set_tool_reporter(&self, reporter: ToolReporter) {
        *self
            .tool_reporter
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reporter);
        self.refresh_capability_reporter().await;
    }

    pub fn set_panel_identity(&self, identity: PanelIdentity) {
        self.panel_gate.set(identity);
    }

    pub fn panel_ready_sync(&self) -> bool {
        self.panel_gate.is_open()
    }

    pub fn panel_access_sync(&self) -> PanelAccess {
        self.panel_gate.identity().access()
    }

    pub fn panel_gate_sync(&self, token: &str) -> Option<PanelAccess> {
        self.panel_gate.lookup(token)
    }

    pub async fn rename_peer_info(&self, from: &str, to: &str) {
        self.inner.rename_peer_info(from, to).await;
    }

    pub async fn note_dial_failure(&self, peer: &str, reason: &str) -> u32 {
        self.inner.note_dial_failure(peer, reason).await
    }

    pub async fn mark_peer_online(&self, peer: &str) {
        self.inner.mark_peer_online(peer).await;
    }

    pub async fn note_peer_advertise(&self, peer: &str, advertise: Option<&str>) {
        self.inner.note_peer_advertise(peer, advertise).await;
    }

    pub async fn peer_diagnostics(&self, peer: &str) -> PeerDiagnostics {
        self.inner.peer_diagnostics(peer).await
    }

    pub async fn add_peer(self: &Arc<Self>, peer: PeerConfig) {
        self.inner.add_peer(peer).await;
    }

    pub fn has_peer_token_sync(&self, token: &str) -> bool {
        self.inner.has_peer_token_sync(token)
    }

    pub fn note_observed_host_sync(&self, host: &str) {
        self.inner.note_observed_host_sync(host);
    }

    pub fn observed_host_sync(&self) -> Option<String> {
        self.inner.observed_host_sync()
    }

    pub async fn accept_ws_pub<S>(
        &self,
        ws: tokio_tungstenite::WebSocketStream<S>,
        presented_token: String,
    ) -> Result<(), FederationError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        self.inner.accept_ws_pub(ws, presented_token).await
    }

    pub async fn remove_peer(&self, name: &str) -> bool {
        self.inner.remove_peer(name).await
    }

    pub async fn drop_link(&self, node_id: &str) {
        self.inner.drop_link(node_id).await;
    }

    pub async fn peer_configs(&self) -> Vec<PeerConfig> {
        self.inner.peer_configs().await
    }

    pub async fn is_link_token_online(&self, token: &str) -> bool {
        self.inner.is_link_token_online(token).await
    }

    pub fn local_node_id(&self) -> &str {
        self.inner.local_node_id()
    }

    pub async fn active_peers(&self) -> Vec<String> {
        self.inner.active_peers().await
    }

    pub async fn peer_caps(&self, peer_name: &str) -> Option<NodeCaps> {
        self.inner
            .peer_caps(peer_name)
            .await
            .and_then(|caps| serde_json::from_value(caps).ok())
    }

    pub async fn peer_info(&self, peer_name: &str) -> Option<PeerInfo> {
        self.inner.peer_info(peer_name).await.map(from_fabric_info)
    }

    pub async fn resolve_accept_peer_name(&self, token: &str, node_id: &str) -> String {
        self.inner.resolve_accept_peer_name(token, node_id).await
    }

    pub async fn send_to(&self, node_id: &str, frame: FedFrame) -> Result<(), Box<FedFrame>> {
        self.inner.send_to(node_id, frame).await
    }

    pub async fn run(self: &Arc<Self>, listen: Option<String>) -> Result<(), FederationError> {
        self.inner.run(listen).await
    }

    pub async fn accept_one_pub(&self, stream: TcpStream) -> Result<(), FederationError> {
        self.inner.accept_one_pub(stream).await
    }

    pub fn check_peer_pub(&self, hello: &NodeHello) -> Result<(), FederationError> {
        self.inner.check_peer_pub(&to_fabric_hello(hello.clone()))
    }
}

pub fn bearer_request(
    url: &str,
    token: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, FederationError> {
    fabric::bearer_request(url, token)
}
