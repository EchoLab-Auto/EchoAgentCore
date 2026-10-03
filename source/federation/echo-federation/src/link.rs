//! 联邦连接管理（federation Phase 1）：对等链路的建立、认证与保活。
//!
//! 每个 Core 同时是服务器（`listen` 接受连入）与客户端（按 peers 配置
//! 连出）；连接建立后角色对称——双方都可发起 Invoke/SubagentSpawn。
//!
//! 握手协议（参考 management WS 的 Bearer 认证模式，`core/src/management.rs`）：
//!
//! ```text
//! 主动方 (dial)                被动方 (accept)
//!     │  WS + Authorization: Bearer <peer_token>  │
//!     │ ─────────────────────────────────────────►│  token 不符 → 401
//!     │  Hello{node_id, caps, protocol_version}   │
//!     │ ─────────────────────────────────────────►│  版本不符/回环 → Error + close
//!     │  Welcome{node_id, caps, ...}              │
//!     │ ◄─────────────────────────────────────────│
//!     │            此后帧双向对称流动               │
//! ```
//!
//! 回环防护：Hello.node_id == 本机 node_id → 拒绝（连到了自己）；
//! 帧级防护（call_id origin == 本机）在 Phase 2 的路由层生效。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, RwLock};
use tokio_tungstenite::tungstenite::Message;

use crate::frame::{FedError, FedFrame, NodeCaps, NodeHello, PROTOCOL_VERSION};

/// 应用层心跳间隔（WS 自带 Ping 之外的存活证据）。
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// 无入站活动判死阈值（与 management WS 同值）。
const DEAD_AFTER: Duration = Duration::from_secs(90);
/// 握手超时。
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// 出站帧通道容量（慢对端背压上限；满则断链）。
const OUTBOUND_CAP: usize = 256;

/// 一个 peer 的静态配置（`[federation.peers.<name>]`）。
#[derive(Debug, Clone)]
pub struct PeerConfig {
    /// 配置名（日志/路由表用）。
    pub name: String,
    /// `ws://host:port`（v1 明文；wss 待 TLS 需求再议）。
    pub url: String,
    /// per-peer 共享密钥（双向相同）。
    pub token: String,
}

/// 链路对端信息（握手成功后）。
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub node_id: String,
    pub node_name: Option<String>,
    pub version: String,
    pub caps: NodeCaps,
    /// 来自哪个配置（连入侧为对端 node_id 匹配到的 peer 名）。
    pub peer_name: String,
}

/// 链路事件（路由层/装配层消费）。
#[derive(Debug, Clone)]
pub enum LinkEvent {
    /// 链路就绪（握手完成）。
    Up(PeerInfo),
    /// 链路断开（对端关闭/判死/网络错误）。
    Down {
        peer_node_id: String,
        reason: String,
    },
    /// 收到业务帧（Hello/Welcome/Ping/Pong 已由链路层消化）。
    Frame { from: String, frame: FedFrame },
}

/// 链路句柄：向对端发帧。
#[derive(Debug, Clone)]
pub struct LinkHandle {
    tx: mpsc::Sender<FedFrame>,
    /// 本链路使用的共享密钥（状态判定/占位 peer 匹配用）。
    token: String,
    /// 链路代际标识：进程内单调递增，摘除时比对——仅当注册的还是
    /// 本链路才移除（否则旧链路 loop 退出会误删重连后的新链路）。
    generation: u64,
}

impl LinkHandle {
    /// 异步发送（背压：通道满返回错误而非阻塞，调用方决定重试/放弃）。
    /// Err 侧是 `Box<FedFrame>`（帧体积大，Box 保持错误轻量）。
    pub fn send(&self, frame: FedFrame) -> Result<(), Box<FedFrame>> {
        self.tx.try_send(frame).map_err(|e| match e {
            mpsc::error::TrySendError::Full(f) | mpsc::error::TrySendError::Closed(f) => {
                Box::new(f)
            }
        })
    }
}

/// 联邦管理器：持有全部链路与事件出口。
pub struct Federation {
    node_id: String,
    node_name: Option<String>,
    version: String,
    caps: NodeCaps,
    /// peer 表（Phase 4 起运行时可变：Save/DeleteFederationPeer）。
    peers: RwLock<Vec<PeerConfig>>,
    /// node_id → 链路句柄（活跃链路）。
    links: Arc<RwLock<HashMap<String, LinkHandle>>>,
    /// 链路代际计数（见 LinkHandle::generation）。
    link_generation: std::sync::atomic::AtomicU64,
    /// per-peer 连出循环取消表（peer 配置名 → 取消哨）：
    /// remove_peer 即停重连；add_peer 更新时先停旧循环再起新的
    /// （此前删除的 peer 永远重连、重复 add 起重复 dial_loop）。
    dial_cancels: RwLock<std::collections::HashMap<String, watch::Sender<bool>>>,
    /// 事件出口（路由层订阅）。
    event_tx: mpsc::Sender<LinkEvent>,
    shutdown: watch::Sender<bool>,
}

impl Federation {
    /// 创建管理器；`event_tx` 由调用方提供（缓冲即背压边界）。
    pub fn new(
        node_id: String,
        node_name: Option<String>,
        caps: NodeCaps,
        peers: Vec<PeerConfig>,
        event_tx: mpsc::Sender<LinkEvent>,
        shutdown: watch::Sender<bool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            node_id,
            node_name,
            version: env!("CARGO_PKG_VERSION").to_string(),
            caps,
            peers: RwLock::new(peers),
            links: Arc::new(RwLock::new(HashMap::new())),
            link_generation: std::sync::atomic::AtomicU64::new(0),
            dial_cancels: RwLock::new(std::collections::HashMap::new()),
            event_tx,
            shutdown,
        })
    }

    fn hello(&self) -> NodeHello {
        NodeHello {
            node_id: self.node_id.clone(),
            node_name: self.node_name.clone(),
            protocol_version: PROTOCOL_VERSION,
            version: self.version.clone(),
            caps: self.caps.clone(),
        }
    }

    /// 运行时新增/更新 peer（Phase 4）：配置写表；有 url 即补起连出循环。
    pub async fn add_peer(self: &Arc<Self>, peer: PeerConfig) {
        {
            let mut peers = self.peers.write().await;
            match peers.iter_mut().find(|p| p.name == peer.name) {
                Some(existing) => *existing = peer.clone(),
                None => peers.push(peer.clone()),
            }
        }
        // 取消该 peer 的旧连出循环（更新场景），再视 url 起新循环。
        if let Some(cancel) = self.dial_cancels.write().await.remove(&peer.name) {
            let _ = cancel.send(true);
        }
        if !peer.url.is_empty() {
            let fed = self.clone();
            let (cancel_tx, cancel_rx) = watch::channel(false);
            self.dial_cancels
                .write()
                .await
                .insert(peer.name.clone(), cancel_tx);
            tokio::spawn(async move { fed.dial_loop(peer, cancel_rx).await });
        }
    }

    /// 运行时删除 peer：停连出循环 + 断其链路（若有）并移出配置表。
    pub async fn remove_peer(&self, name: &str) -> bool {
        // 先停重连循环——否则删除后 peer 仍会无限重连（🔴 修复）。
        if let Some(cancel) = self.dial_cancels.write().await.remove(name) {
            let _ = cancel.send(true);
        }
        let removed = {
            let mut peers = self.peers.write().await;
            let before = peers.len();
            peers.retain(|p| p.name != name);
            peers.len() != before
        };
        // 注：链路表按 node_id 索引、不存 peer 配置名；断链由调用方
        // （装配层 FederationRuntime.peer_names 维护 name↔node 映射）
        // 经 [`Federation::drop_link`] 显式完成。
        removed
    }

    /// 断开指定 node_id 的链路（Phase 4：remove_peer 的配套）。
    pub async fn drop_link(&self, node_id: &str) {
        self.links.write().await.remove(node_id);
    }

    /// peer 配置快照（状态查询用；token 明文不回传——调用方负责脱敏）。
    pub async fn peer_configs(&self) -> Vec<PeerConfig> {
        self.peers.read().await.clone()
    }

    /// 某 token 是否有活跃链路（占位 peer 在线判定用）。
    pub async fn is_link_token_online(&self, token: &str) -> bool {
        !token.is_empty() && self.links.read().await.values().any(|h| h.token == token)
    }

    /// 本机 node_id（状态查询用）。
    pub fn local_node_id(&self) -> &str {
        &self.node_id
    }

    /// 活跃链路快照（node_id 列表）。
    pub async fn active_peers(&self) -> Vec<String> {
        self.links.read().await.keys().cloned().collect()
    }

    /// 向指定节点发帧。
    pub async fn send_to(&self, node_id: &str, frame: FedFrame) -> Result<(), Box<FedFrame>> {
        match self.links.read().await.get(node_id) {
            Some(handle) => handle.send(frame),
            None => Err(Box::new(frame)),
        }
    }

    /// 启动监听（`addr` 如 `0.0.0.0:3133`）+ 全部 peers 连出，常驻运行。
    pub async fn run(self: &Arc<Self>, listen: Option<String>) -> Result<()> {
        let mut shutdown_rx = self.shutdown.subscribe();
        // 连出任务：启动时的 peers 各起独立重连循环；运行时新增的 peer
        // 由 [`Federation::add_peer`] 现场补起。
        for peer in self.peers.read().await.iter() {
            if peer.url.is_empty() {
                continue; // 仅接受连入的 peer（纯被动配置）
            }
            let fed = self.clone();
            let peer = peer.clone();
            let (cancel_tx, cancel_rx) = watch::channel(false);
            self.dial_cancels
                .write()
                .await
                .insert(peer.name.clone(), cancel_tx);
            tokio::spawn(async move { fed.dial_loop(peer, cancel_rx).await });
        }
        if let Some(addr) = listen {
            let listener = TcpListener::bind(&addr)
                .await
                .with_context(|| format!("bind federation address {addr}"))?;
            tracing::info!(%addr, "federation listener up");
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, peer_addr)) => {
                                tracing::info!(%peer_addr, "federation inbound");
                                let fed = self.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = fed.accept_one(stream).await {
                                        tracing::warn!(%peer_addr, error = %e, "federation inbound closed");
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "federation accept failed");
                                tokio::time::sleep(Duration::from_millis(200)).await;
                            }
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        tracing::info!("federation listener shutting down");
                        return Ok(());
                    }
                }
            }
        } else {
            // 无监听（纯连出节点）：等待关闭信号。
            let _ = shutdown_rx.changed().await;
            Ok(())
        }
    }

    /// 连出 + 断线重连（指数退避，上限 30s）。`cancel` 为该 peer 专属
    /// 取消哨（remove_peer/更新时触发），与全局 shutdown 并列。
    async fn dial_loop(&self, peer: PeerConfig, mut cancel: watch::Receiver<bool>) {
        let mut backoff = Duration::from_secs(1);
        let mut shutdown_rx = self.shutdown.subscribe();
        loop {
            if *cancel.borrow() {
                tracing::info!(peer = %peer.name, "federation dial loop cancelled (peer removed/updated)");
                return;
            }
            let request = match bearer_request(&peer.url, &peer.token) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(peer = %peer.name, error = %e, "federation request build failed");
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = shutdown_rx.changed() => return,
                        _ = cancel.changed() => return,
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            };
            match tokio::time::timeout(
                HELLO_TIMEOUT + Duration::from_secs(5),
                tokio_tungstenite::connect_async(request),
            )
            .await
            {
                Ok(Ok((ws, _))) => {
                    let link_token = peer.token.clone();
                    let connected_at = Instant::now();
                    if let Err(e) = self.run_link(ws, Some(peer.clone()), link_token).await {
                        tracing::warn!(peer = %peer.name, error = %e, "federation link down");
                        // 仅当链路曾正常存活（>5s = 握手成功且跑过一段）
                        // 才重置退避；握手失败（版本/token 不符秒断）保持
                        // 退避递增——否则以 ~1s 间隔无限重连刷日志（🟡 修复）。
                        if connected_at.elapsed() > Duration::from_secs(5) {
                            backoff = Duration::from_secs(1);
                        }
                    } else {
                        backoff = Duration::from_secs(1);
                    }
                }
                Ok(Err(e)) => {
                    tracing::debug!(peer = %peer.name, error = %e, "federation dial failed");
                }
                Err(_) => {
                    tracing::debug!(peer = %peer.name, "federation dial timeout");
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown_rx.changed() => return,
                _ = cancel.changed() => return,
            }
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    /// 接受一条连入：认证 → 握手 → 跑链路。
    #[doc(hidden)]
    pub async fn accept_one_pub(&self, stream: TcpStream) -> Result<()> {
        self.accept_one(stream).await
    }

    /// 握手校验（测试可见）。
    #[doc(hidden)]
    pub fn check_peer_pub(&self, hello: &NodeHello) -> Result<()> {
        self.check_peer(hello)
    }

    async fn accept_one(&self, stream: TcpStream) -> Result<()> {
        // 认证：Bearer token 必须匹配某个配置的 peer（与 management WS 同模式）。
        let tokens: Vec<String> = self
            .peers
            .read()
            .await
            .iter()
            .map(|p| p.token.clone())
            .collect();
        let (ws, presented_token) = accept_with_auth(stream, &tokens).await?;
        self.run_link(ws, None, presented_token).await
    }

    /// 握手 + 链路主循环（连入/连出共用；`dial_peer` = Some 时为主动方）。
    /// `link_token`：连入侧 = 认证呈现的 Bearer；连出侧 = peer 配置 token。
    async fn run_link<S>(
        &self,
        ws: tokio_tungstenite::WebSocketStream<S>,
        dial_peer: Option<PeerConfig>,
        link_token: String,
    ) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut write, mut read) = ws.split();
        let hello = FedFrame::Hello(self.hello());

        // 主动方（Bearer 已在 connect 请求头完成）先发 Hello；被动方先收 Hello。
        if dial_peer.is_some() {
            let text = serde_json::to_string(&hello)?;
            write.send(Message::Text(text)).await?;
        }

        // 等对方 Hello（被动方）或作为主动方已发 Hello 后等 Welcome。
        let first = tokio::time::timeout(HELLO_TIMEOUT, read.next())
            .await
            .context("hello timeout")?
            .context("link closed before hello")??;
        let peer_hello: NodeHello = match serde_json::from_str::<FedFrame>(first.to_text()?) {
            Ok(FedFrame::Hello(h)) | Ok(FedFrame::Welcome(h)) => h,
            _ => anyhow::bail!("first frame is not Hello/Welcome"),
        };
        self.check_peer(&peer_hello)?;

        if dial_peer.is_none() {
            // 被动方回握。
            let text = serde_json::to_string(&FedFrame::Welcome(self.hello()))?;
            write.send(Message::Text(text)).await?;
        } else {
            // 主动方应收到 Welcome；若收到的是 Hello（双方同时 dial），按
            // node_id 字典序小的一方断开重连（天然防撞）。
            if matches!(
                serde_json::from_str::<FedFrame>(first.to_text()?),
                Ok(FedFrame::Hello(_))
            ) && self.node_id < peer_hello.node_id
            {
                anyhow::bail!("simultaneous dial, backing off");
            }
        }

        let peer_name = dial_peer
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| peer_hello.node_id.clone());
        let info = PeerInfo {
            node_id: peer_hello.node_id.clone(),
            node_name: peer_hello.node_name,
            version: peer_hello.version,
            caps: peer_hello.caps,
            peer_name,
        };
        tracing::info!(peer = %info.node_id, name = %info.peer_name, "federation link up");

        // 注册链路（同 node_id 的旧链路被顶掉——重连/双向同时 dial 场景）。
        // 顶掉前先显式 Down：否则路由层会保留旧链路注册的代理工具（新
        // Up 与之并存导致重复注册/旧裁决残留）。
        let (out_tx, mut out_rx) = mpsc::channel::<FedFrame>(OUTBOUND_CAP);
        let generation = self
            .link_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let handle = LinkHandle {
            tx: out_tx,
            token: link_token.clone(),
            generation,
        };
        let replaced = self
            .links
            .write()
            .await
            .insert(info.node_id.clone(), handle)
            .is_some();
        if replaced {
            tracing::info!(peer = %info.node_id, "federation link replaced (simultaneous dial or reconnect)");
            let _ = self
                .event_tx
                .send(LinkEvent::Down {
                    peer_node_id: info.node_id.clone(),
                    reason: "replaced by new link".into(),
                })
                .await;
        }
        let _ = self.event_tx.send(LinkEvent::Up(info.clone())).await;

        let result = self
            .link_loop(&mut read, &mut write, &mut out_rx, &info)
            .await;

        // 仅当注册的还是本链路时摘除（避免误删重连后的新链路）——
        // 代际不匹配说明本链路已被顶掉，新链路仍活跃：不摘注册、
        // 不发 Down（顶掉时已为新链路补过旧链路的 Down）。
        let still_mine = {
            let mut links = self.links.write().await;
            match links.get(&info.node_id) {
                Some(h) if h.generation == generation => {
                    links.remove(&info.node_id);
                    true
                }
                _ => false,
            }
        };
        let reason = format!("{result:?}");
        if still_mine {
            let _ = self
                .event_tx
                .send(LinkEvent::Down {
                    peer_node_id: info.node_id.clone(),
                    reason: reason.clone(),
                })
                .await;
            tracing::info!(peer = %info.node_id, %reason, "federation link down");
        }
        result
    }

    /// 握手校验：版本兼容 + 非回环。
    fn check_peer(&self, hello: &NodeHello) -> Result<()> {
        if hello.node_id == self.node_id {
            anyhow::bail!("loop detected: peer is self ({})", hello.node_id);
        }
        if hello.protocol_version != PROTOCOL_VERSION {
            anyhow::bail!(
                "protocol version mismatch: local {} vs peer {}",
                PROTOCOL_VERSION,
                hello.protocol_version
            );
        }
        Ok(())
    }

    /// 链路主循环：出站转发 + 入站分发 + 心跳 + 判死。
    async fn link_loop<R, W>(
        &self,
        read: &mut R,
        write: &mut W,
        out_rx: &mut mpsc::Receiver<FedFrame>,
        info: &PeerInfo,
    ) -> Result<()>
    where
        R: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
        W: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat.tick().await; // 跳过立即触发
        let mut last_seen = Instant::now();
        let mut shutdown_rx = self.shutdown.subscribe();
        loop {
            tokio::select! {
                // 出站：业务帧 → 对端
                Some(frame) = out_rx.recv() => {
                    let text = serde_json::to_string(&frame)?;
                    write.send(Message::Text(text)).await?;
                }
                // 入站
                inbound = read.next() => {
                    match inbound {
                        Some(Ok(Message::Text(text))) => {
                            last_seen = Instant::now();
                            match serde_json::from_str::<FedFrame>(&text) {
                                Ok(FedFrame::Ping { sent_at_ms }) => {
                                    let pong = FedFrame::Pong { sent_at_ms: now_ms(), echo_ms: sent_at_ms };
                                    write.send(Message::Text(serde_json::to_string(&pong)?)).await?;
                                }
                                Ok(FedFrame::Pong { .. }) => { /* 存活证据已刷新 */ }
                                Ok(frame) => {
                                    let event = LinkEvent::Frame { from: info.node_id.clone(), frame };
                                    if self.event_tx.send(event).await.is_err() {
                                        anyhow::bail!("event sink closed");
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(peer = %info.node_id, error = %e, "bad federation frame");
                                }
                            }
                        }
                        Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {
                            last_seen = Instant::now();
                        }
                        Some(Ok(Message::Close(_))) | None => anyhow::bail!("peer closed"),
                        Some(Err(e)) => anyhow::bail!("ws error: {e}"),
                        _ => {}
                    }
                }
                // 心跳 + 判死
                _ = heartbeat.tick() => {
                    if last_seen.elapsed() > DEAD_AFTER {
                        anyhow::bail!("peer silent for >{}s", DEAD_AFTER.as_secs());
                    }
                    let ping = FedFrame::Ping { sent_at_ms: now_ms() };
                    write.send(Message::Text(serde_json::to_string(&ping)?)).await?;
                }
                _ = shutdown_rx.changed() => anyhow::bail!("shutdown"),
            }
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 连入侧认证 + WS upgrade。回调签名由 tungstenite 固定（Err 侧
/// `ErrorResponse` 体积大，与 `core/src/management.rs` 同款容忍）。
#[allow(clippy::result_large_err)]
async fn accept_with_auth(
    stream: TcpStream,
    tokens: &[String],
) -> Result<(tokio_tungstenite::WebSocketStream<TcpStream>, String)> {
    let tokens = tokens.to_vec();
    let presented = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let presented2 = presented.clone();
    let ws = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
            let token = request
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_string);
            let ok = token
                .as_ref()
                .map(|t| tokens.iter().any(|known| !known.is_empty() && known == t))
                .unwrap_or(false);
            if ok {
                *presented2.lock().unwrap() = token.unwrap_or_default();
                Ok(response)
            } else {
                Err(
                    tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(Some(
                        "unauthorized".into(),
                    )),
                )
            }
        },
    )
    .await?;
    let token = presented.lock().unwrap().clone();
    Ok((ws, token))
}

/// 构造带 Bearer 的连出请求（dial 侧认证）。
pub fn bearer_request(
    url: &str,
    token: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request()?;
    if !token.is_empty() {
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .context("invalid token header")?,
        );
    }
    Ok(req)
}

/// FedError → 帧（辅助）。
pub fn error_frame(
    call_id: Option<String>,
    code: FedError,
    message: impl Into<String>,
) -> FedFrame {
    FedFrame::Error {
        call_id,
        code,
        message: message.into(),
    }
}
