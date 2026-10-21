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

use crate::error::FederationError;
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
/// 入站观测主机池上限（只留最近若干个不同的 `Host`，去重后仍封顶）。
const OBSERVED_HOST_CAP: usize = 8;
/// 出站帧通道容量（慢对端背压上限；满则断链）。
const OUTBOUND_CAP: usize = 256;
/// dial 停车轮询间隔（已有活跃链路时等待其消失的检查周期）。
const LINK_WAIT_POLL: Duration = Duration::from_secs(2);
/// 出站写超时：对端不读时 TCP 发送缓冲写满，裸 `write.send().await` 会
/// 永久阻塞——select 循环随之冻结（读/心跳/判死分支全部停摆），链路成
/// 僵尸而 `has_active_link` 仍为真、拨号环也不再重连。超时即断链，交给
/// 重连收敛（与 management WS 的判死/写超时口径一致）。
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// 带超时的出站写（语义见 [`WRITE_TIMEOUT`]）。
async fn send_with_timeout<S>(write: &mut S, msg: Message) -> Result<(), FederationError>
where
    S: futures_util::Sink<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    send_with_timeout_for(write, msg, WRITE_TIMEOUT).await
}

/// 带自定义超时的出站写（测试注入短超时；生产一律走 [`send_with_timeout`]）。
async fn send_with_timeout_for<S>(
    write: &mut S,
    msg: Message,
    timeout: Duration,
) -> Result<(), FederationError>
where
    S: futures_util::Sink<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(timeout, write.send(msg)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(FederationError::Ws(Box::new(e))),
        Err(_) => Err(FederationError::WriteTimeout {
            secs: timeout.as_secs(),
        }),
    }
}

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
    /// 对端宣告的可拨地址（`ws://host:port`；None = 对端未宣告/未监听）。
    /// 装配层（core/main.rs）据此学习对端地址 → 双向重连（配对无方向性）。
    pub advertise: Option<String>,
    /// 本机配置里的 peer 名：连出侧 = dial 配置名；连入侧 = 按链路
    /// token 反查的配置名（2026-10 起；此前为对端 node_id——导致策略
    /// 查找/映射全键错，见 `resolve_accept_peer_name`）。
    pub peer_name: String,
}

/// 单个 peer 的连出健康诊断（2026-10 地址漂移事故后新增）：面板据此
/// 把「持续连不上」显性化——此前拨号失败只有 debug 日志，节点对端映射
/// 变了（NAT 端口变化）后两边都在对死地址空转、界面却看不到任何原因。
#[derive(Debug, Clone, Default)]
pub struct PeerDiagnostics {
    /// 连续拨号失败次数（成功建链即清零）。
    pub consecutive_failures: u32,
    /// 最近一次拨号失败原因（成功建链即清空）。
    pub last_error: Option<String>,
    /// 最近一次成功建链时间（unix 秒；从未成功 = None）。
    pub last_online_at: Option<i64>,
    /// 最近一次从对端 Hello 学到的可拨地址（**跨断链保留**——地址漂移
    /// 提示与人工修复的数据源：配置 url 拨不通而此值不同时，面板可提示
    /// "对端最近宣告了 X，一键改用"）。
    pub last_advertise: Option<String>,
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
    /// 本链路是否属于"优选连接"（由 node_id 较小的一方拨出；见
    /// `run_link` 的双向 dial 去重规则）。优选连接不被非优选连接顶掉。
    preferred: bool,
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
    /// 本机对外宣告的可拨地址（`ws://host:port`；None = 未监听/不宣告）。
    /// 握手 Hello/Welcome 携带；对端学习后即可反向连入本机（2026-10）。
    local_advertise: Option<String>,
    /// peer 表（Phase 4 起运行时可变：Save/DeleteFederationPeer）。
    peers: RwLock<Vec<PeerConfig>>,
    /// **同步**令牌镜像（端口合并用）：管理和联邦共用一个监听端口时，
    /// WS 升级回调（同步闭包）需要同步读取 per-peer 令牌来分类连接——
    /// 异步 `peers` 表在升级回调里不可用。`new`/`add_peer`/`remove_peer`
    /// 同步维护（只含非空令牌；比较走常时）。
    synced_tokens: std::sync::RwLock<Vec<String>>,
    /// 入站连接观测到的「客户端访问本机用的主机名」池（`Host` 头；2026-11）：
    /// 邀请串与 Hello.advertise 的**首选**来源——手工 `[federation] advertise`
    /// 常写成只有本机视角成立的地址（NAT 后的内网 IP），而对端照着拨必然
    /// 超时；真正可达的地址其实就写在对端自己的请求里（见
    /// [`crate::invite`] 模块文档）。
    ///
    /// 为什么是 `Arc<std::sync::RwLock<..>>`：WS 升级回调是**同步**闭包且
    /// 要求 `'static`（与 `synced_tokens` 同理），所以既不能用 tokio 锁，
    /// 也不能借 `&self`——只能把句柄 clone 进闭包。临界区只有几微秒（比较
    /// 一个长度 ≤ 8 的 Vec），不会阻塞运行时。
    observed_hosts: Arc<std::sync::RwLock<Vec<String>>>,
    /// node_id → 链路句柄（活跃链路）。
    links: Arc<RwLock<HashMap<String, LinkHandle>>>,
    /// 链路代际计数（见 LinkHandle::generation）。
    link_generation: std::sync::atomic::AtomicU64,
    /// per-peer 连出循环取消表（peer 配置名 → 取消哨）：
    /// remove_peer 即停重连；add_peer 更新时先停旧循环再起新的
    /// （此前删除的 peer 永远重连、重复 add 起重复 dial_loop）。
    dial_cancels: RwLock<std::collections::HashMap<String, watch::Sender<bool>>>,
    /// 最近链路的 PeerInfo（peer 配置名 → 信息；Up 登记，Down 清除）。
    last_peer_infos: RwLock<std::collections::HashMap<String, PeerInfo>>,
    /// 每 peer 的连出健康（peer 配置名 → 诊断；2026-10 地址漂移事故）：
    /// 失败计数/最后错误/最后在线时刻——`peer_diagnostics` 供装配层填充
    /// FederationStatus；跨断链保留（诊断价值恰在断链时）。
    dial_health: RwLock<std::collections::HashMap<String, PeerDiagnostics>>,
    /// 最近一次从对端 Hello 学到的可拨地址（peer 配置名 → url；跨断链
    /// 保留，peer 删除时清理）——地址漂移的"一键修复"数据源。
    known_advertise: RwLock<std::collections::HashMap<String, String>>,
    /// 负载上报句柄（P2 调度器）：握手 hello 时调用取当前活跃 turn
    /// 数——对端调度器拿到的是**实时**负载而非启动快照。由装配层
    /// （core/main.rs，持有 supervisor）注入；未注入时 hello 报 0。
    load_reporter: RwLock<Option<std::sync::Arc<dyn Fn() -> u32 + Send + Sync>>>,
    /// 远程可调用工具上报句柄（2026-11 S8）：握手 hello 时调用取**当前**
    /// 可被远程调用的工具名——真源是每个工具自己的 `remote_capable()`
    /// 声明（经 `ToolRegistry::remote_capable_names` 汇总）。
    ///
    /// 为什么是回调而不是构造参数：`Federation::new` 在组合根里比插件挂载
    /// 早得多（此刻还没有任何工具注册表）。注入后 hello 每次现取，所以
    /// 运行期启用/停用插件会直接反映到下一次握手，而不用重启节点。
    /// 未注入 = 宣告空集（fail-closed：宁可少宣告，不可宣告了接不了）。
    tool_reporter: RwLock<Option<std::sync::Arc<dyn Fn() -> Vec<String> + Send + Sync>>>,
    /// 事件出口（路由层订阅）。
    event_tx: mpsc::Sender<LinkEvent>,
    shutdown: watch::Sender<bool>,
}

impl Federation {
    /// 创建管理器；`event_tx` 由调用方提供（缓冲即背压边界）。
    /// `advertise` = 本机对外可拨地址（`ws://host:port`；None = 不宣告）。
    pub fn new(
        node_id: String,
        node_name: Option<String>,
        caps: NodeCaps,
        peers: Vec<PeerConfig>,
        advertise: Option<String>,
        event_tx: mpsc::Sender<LinkEvent>,
        shutdown: watch::Sender<bool>,
    ) -> Arc<Self> {
        let synced_tokens = std::sync::RwLock::new(
            peers
                .iter()
                .map(|p| p.token.clone())
                .filter(|t| !t.is_empty())
                .collect(),
        );
        Arc::new(Self {
            node_id,
            node_name,
            version: env!("CARGO_PKG_VERSION").to_string(),
            caps,
            local_advertise: advertise,
            peers: RwLock::new(peers),
            synced_tokens,
            observed_hosts: Arc::new(std::sync::RwLock::new(Vec::new())),
            links: Arc::new(RwLock::new(HashMap::new())),
            link_generation: std::sync::atomic::AtomicU64::new(0),
            dial_cancels: RwLock::new(std::collections::HashMap::new()),
            last_peer_infos: RwLock::new(std::collections::HashMap::new()),
            dial_health: RwLock::new(std::collections::HashMap::new()),
            known_advertise: RwLock::new(std::collections::HashMap::new()),
            load_reporter: RwLock::new(None),
            tool_reporter: RwLock::new(None),
            event_tx,
            shutdown,
        })
    }

    fn hello(&self) -> NodeHello {
        let mut caps = self.caps.clone();
        // 实时负载注入（P2 调度器）：装配层已注入 reporter 时，握手
        // 宣告当前活跃 turn 数（try_read 失败=注入中，用启动快照 0）。
        if let Ok(guard) = self.load_reporter.try_read() {
            if let Some(reporter) = guard.as_ref() {
                caps.active_turns = reporter();
            }
        }
        // 远程可调用工具（2026-11 S8）：工具自己的 `remote_capable()` 声明，
        // 汇总自注册表。**覆盖**构造时的快照——那个快照只在未注入 reporter
        // 的场合有意义（单测 / 早启动），生产装配一律走 reporter。
        match self.tool_reporter.try_read() {
            Ok(guard) => match guard.as_ref() {
                Some(reporter) => caps.tools = reporter(),
                None => {
                    // 装配顺序错了（监听早于工具注册表就绪）：对端会以为本机
                    // 什么活都接不了，且**不会重问**——所以这条必须显性报警，
                    // 而不是安静地宣告空集。组合根已在注入 reporter 之后才
                    // 起监听/连出（见 core/main.rs），所以正常运行时不会出现。
                    tracing::warn!(
                        target: "federation",
                        "hello: tool reporter not injected yet — advertising no remote tools; \
                         peer will register no proxy tools until reconnect"
                    );
                }
            },
            Err(_) => {
                // 注入中（写锁未释放）：本次握手保守地不宣告，下次握手会好。
                tracing::debug!(target: "federation", "hello: tool reporter busy — empty tools");
            }
        }
        NodeHello {
            node_id: self.node_id.clone(),
            node_name: self.node_name.clone(),
            protocol_version: PROTOCOL_VERSION,
            version: self.version.clone(),
            caps,
            // 观测地址优先（2026-11）：手工 advertise 可能只在本机视角成立
            // （云机 NAT 后的内网 IP），对端据它回拨必然超时。
            advertise: self.effective_advertise_sync(),
        }
    }

    /// 重键最近链路信息（邀请占位提升：`invite-*` → 配置名）——dial
    /// 停车判定（`has_active_link`）与远程调用反查按新名继续工作。
    pub async fn rename_peer_info(&self, from: &str, to: &str) {
        let mut infos = self.last_peer_infos.write().await;
        if let Some(mut info) = infos.remove(from) {
            info.peer_name = to.to_string();
            infos.insert(to.to_string(), info);
        }
    }

    /// 记录一次拨号失败（返回累计连续失败次数）。装配层/日志据此把
    /// "持续连不上"从静默 debug 变为可观测（2026-10 地址漂移事故）。
    pub async fn note_dial_failure(&self, peer: &str, reason: &str) -> u32 {
        let mut map = self.dial_health.write().await;
        let health = map.entry(peer.to_string()).or_default();
        health.consecutive_failures = health.consecutive_failures.saturating_add(1);
        health.last_error = Some(reason.to_string());
        health.consecutive_failures
    }

    /// 链路建立（握手成功、登记入表）= 在线：清零失败计数、记录时刻。
    pub async fn mark_peer_online(&self, peer: &str) {
        let mut map = self.dial_health.write().await;
        let health = map.entry(peer.to_string()).or_default();
        health.consecutive_failures = 0;
        health.last_error = None;
        health.last_online_at = Some(now_ms() / 1000);
    }

    /// 记录对端 Hello 宣告的可拨地址（跨断链保留）——地址漂移提示 /
    /// 人工一键修复的数据源。
    pub async fn note_peer_advertise(&self, peer: &str, advertise: Option<&str>) {
        if let Some(adv) = advertise.map(str::trim).filter(|a| !a.is_empty()) {
            self.known_advertise
                .write()
                .await
                .insert(peer.to_string(), adv.to_string());
        }
    }

    /// 某 peer 的诊断快照（连出健康 + 最近宣告地址；状态面板用）。
    pub async fn peer_diagnostics(&self, peer: &str) -> PeerDiagnostics {
        let mut diag = self
            .dial_health
            .read()
            .await
            .get(peer)
            .cloned()
            .unwrap_or_default();
        diag.last_advertise = self.known_advertise.read().await.get(peer).cloned();
        diag
    }

    /// 到某 peer（配置名）是否已有活跃链路：以最近链路信息的 node_id
    /// 在 links 表核对（Down 时 last_peer_infos 已清除，双条件防陈旧）。
    /// 先取 node_id 再释放读锁、后锁 links，避免与注册路径（links→infos
    /// 加锁序）死锁。
    async fn has_active_link(&self, peer_name: &str) -> bool {
        let node_id = self
            .last_peer_infos
            .read()
            .await
            .get(peer_name)
            .map(|info| info.node_id.clone());
        let Some(node_id) = node_id else {
            return false;
        };
        self.links.read().await.contains_key(&node_id)
    }

    /// 注入负载上报句柄（装配层：core/main.rs 持 supervisor 后调用）。
    pub async fn set_load_reporter(&self, reporter: std::sync::Arc<dyn Fn() -> u32 + Send + Sync>) {
        *self.load_reporter.write().await = Some(reporter);
    }

    /// 注入"远程可调用工具"的真源（组合根：全部 persona 的工具注册表）。
    ///
    /// 组装顺序：`Federation` 先建（此时还没有工具注册表）→ persona/supervisor
    /// 就绪 → 调用本方法。此后每次握手都现取，插件启停即时生效。
    pub async fn set_tool_reporter(
        &self,
        reporter: std::sync::Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    ) {
        *self.tool_reporter.write().await = Some(reporter);
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
        // 同步令牌镜像（端口合并分类器读；见字段注释）。
        self.refresh_synced_tokens().await;
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

    /// 从异步 `peers` 表刷新同步令牌镜像（仅 `add_peer`/`remove_peer`
    /// 调用，且调用时必然持有过 peers 写锁之外的时机——短临界区、无 await）。
    async fn refresh_synced_tokens(&self) {
        let tokens: Vec<String> = self
            .peers
            .read()
            .await
            .iter()
            .map(|p| p.token.clone())
            .filter(|t| !t.is_empty())
            .collect();
        *self
            .synced_tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = tokens;
    }

    /// 同步版 per-peer 令牌判定（端口合并的升级回调专用；空令牌恒不
    /// 命中）。命中即该连接应归为联邦链路。
    pub fn has_peer_token_sync(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        self.synced_tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|known| constant_time_eq(known, token))
    }

    /// 记录一次入站连接观测到的 `Host`（同步：WS 升级回调专用）。
    ///
    /// 只记「可用于对外邀请」的主机名（回环/通配直接丢弃，见
    /// [`crate::invite::note_observed`]）；池内去重且最近优先。
    pub fn note_observed_host_sync(&self, host: &str) {
        let cap = OBSERVED_HOST_CAP;
        let mut hosts = self
            .observed_hosts
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::invite::note_observed(&mut hosts, host, cap);
    }

    /// 当前最优的观测主机名（公网优先，同级取最近）；`None` = 还没观测到
    /// 可用地址（此时调用方回退 `[federation] advertise`）。
    pub fn observed_host_sync(&self) -> Option<String> {
        let hosts = self
            .observed_hosts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::invite::pick_observed_host(&hosts)
    }

    /// Hello 里宣告的本机可拨地址：观测地址优先（替换主机部分、端口不变），
    /// 未观测到则用构造期推导值（2026-11：对端据此回拨，地址必须真可达）。
    fn effective_advertise_sync(&self) -> Option<String> {
        let advertise = self.local_advertise.as_deref()?;
        let Some(observed) = self.observed_host_sync() else {
            return self.local_advertise.clone();
        };
        crate::invite::replace_host(advertise.trim_start_matches("ws://"), &observed)
            .map(|host_port| format!("ws://{host_port}"))
            .or_else(|| self.local_advertise.clone())
    }

    /// 在**已完成**认证与 WS 升级的连接上跑联邦链路（端口合并模式：
    /// 认证与分类由合并监听端完成，`presented_token` = 对端呈现的
    /// Bearer，用于按 token 反查 peer 配置名）。
    #[doc(hidden)]
    pub async fn accept_ws_pub<S>(
        &self,
        ws: tokio_tungstenite::WebSocketStream<S>,
        presented_token: String,
    ) -> Result<(), FederationError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        self.run_link(ws, None, presented_token).await
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
        if removed {
            self.refresh_synced_tokens().await;
        }
        // 诊断表清理（peer 已从配置消失——历史失败/宣告地址一并移除）。
        self.dial_health.write().await.remove(name);
        self.known_advertise.write().await.remove(name);
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

    /// 按 **peer 配置名**查最近链路的对端能力（Up 时登记于
    /// PeerInfo；2026-10 P2 调度器负载输入）。链路断开/未知时 None。
    pub async fn peer_caps(&self, peer_name: &str) -> Option<NodeCaps> {
        let infos = self.last_peer_infos.read().await;
        infos.get(peer_name).map(|i| i.caps.clone())
    }

    /// 按 **peer 配置名**查最近链路的完整对端信息（Up 登记/Down 清除）。
    pub async fn peer_info(&self, peer_name: &str) -> Option<PeerInfo> {
        self.last_peer_infos.read().await.get(peer_name).cloned()
    }

    /// 接受侧 peer 名解析（2026-10）：按链路呈现的 token 反查运行时
    /// peer 表（配置 + 邀请占位）的**配置名**。认证已要求 token 匹配，
    /// 因而总能命中；仅 token 为空（理论不可达）退回 node_id。
    pub async fn resolve_accept_peer_name(&self, token: &str, node_id: &str) -> String {
        if !token.is_empty() {
            let peers = self.peers.read().await;
            if let Some(p) = peers.iter().find(|p| p.token == token) {
                return p.name.clone();
            }
        }
        node_id.to_string()
    }

    /// 向指定节点发帧。
    pub async fn send_to(&self, node_id: &str, frame: FedFrame) -> Result<(), Box<FedFrame>> {
        match self.links.read().await.get(node_id) {
            Some(handle) => handle.send(frame),
            None => Err(Box::new(frame)),
        }
    }

    /// 启动监听（`addr` 如 `0.0.0.0:3133`）+ 全部 peers 连出，常驻运行。
    pub async fn run(self: &Arc<Self>, listen: Option<String>) -> Result<(), FederationError> {
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
                .map_err(|e| FederationError::Bind {
                    addr: addr.clone(),
                    source: e,
                })?;
            tracing::info!(%addr, "federation listener up");
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, peer_addr)) => {
                                // 连接接受/关闭日志（2026-10 审计 P2-②）：
                                // 未认证拒绝（标准 401——探测/扫描/错 token）
                                // 属常规，降为 debug；其余错误保持 warn。
                                // 此前两者都 info+warn，被扫描时日志刷屏。
                                tracing::debug!(%peer_addr, "federation inbound");
                                let fed = self.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = fed.accept_one(stream).await {
                                        let text = e.to_string();
                                        if text.contains("401") || text.contains("Unauthorized") {
                                            tracing::debug!(%peer_addr, error = %text, "federation inbound rejected (unauthorized)");
                                        } else {
                                            tracing::warn!(%peer_addr, error = %text, "federation inbound closed");
                                        }
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
            // 已有到该 peer 的活跃链路时停车等待（配对无方向性的关键）：
            // 双方互知地址后都持有连出循环，若不停车，后拨到的连接会顶掉
            // 先建的链路 → 对方触发重连 → 两边无限对拆。链路消失或取消
            // 哨/关闭信号触发即醒。
            while self.has_active_link(&peer.name).await {
                tokio::select! {
                    _ = tokio::time::sleep(LINK_WAIT_POLL) => {}
                    _ = shutdown_rx.changed() => return,
                    _ = cancel.changed() => return,
                }
                if *cancel.borrow() {
                    return;
                }
            }
            let request = match bearer_request(&peer.url, &peer.token) {
                Ok(r) => r,
                Err(e) => {
                    let reason = format!("request build failed: {e}");
                    let failures = self.note_dial_failure(&peer.name, &reason).await;
                    tracing::warn!(peer = %peer.name, failures, error = %e, "federation request build failed");
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
                        } else {
                            // 秒断 = 从未真正可用（认证/版本/对端立即关闭）：
                            // 计入拨号失败，让诊断面板可见（2026-10）。
                            let reason = format!("connection failed after connect: {e}");
                            self.note_dial_failure(&peer.name, &reason).await;
                        }
                    } else {
                        backoff = Duration::from_secs(1);
                    }
                }
                Ok(Err(e)) => {
                    let reason = e.to_string();
                    let failures = self.note_dial_failure(&peer.name, &reason).await;
                    // 失败显性化（2026-10 地址漂移事故）：首次与每 60 次
                    // （退避上限 30s ≈ 30 分钟）warn 一次——此前恒 debug，
                    // 对端地址变了之后"两边空转"在日志里完全不可见。
                    //
                    // 2026-10 日志治理：间隔由每 10 次（≈5 分钟）放宽到每 60
                    // 次（≈30 分钟）。长期失联的对端（对端下线是常态）每分钟
                    // 一条 WARN 会把 7 天窗口内的日志顶到 1600+ 条，淹没真正
                    // 的变化；恢复时另有一条独立的 INFO（见 run_link）。
                    if failures == 1 || failures % 60 == 0 {
                        tracing::warn!(
                            peer = %peer.name,
                            url = %peer.url,
                            failures,
                            error = %reason,
                            "federation dial failing (will keep retrying)"
                        );
                    } else {
                        tracing::debug!(peer = %peer.name, error = %reason, "federation dial failed");
                    }
                }
                Err(_) => {
                    let reason = "dial timeout".to_string();
                    let failures = self.note_dial_failure(&peer.name, &reason).await;
                    if failures == 1 || failures % 60 == 0 {
                        tracing::warn!(
                            peer = %peer.name,
                            url = %peer.url,
                            failures,
                            "federation dial timing out (will keep retrying)"
                        );
                    } else {
                        tracing::debug!(peer = %peer.name, "federation dial timeout");
                    }
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
    pub async fn accept_one_pub(&self, stream: TcpStream) -> Result<(), FederationError> {
        self.accept_one(stream).await
    }

    /// 握手校验（测试可见）。
    #[doc(hidden)]
    pub fn check_peer_pub(&self, hello: &NodeHello) -> Result<(), FederationError> {
        self.check_peer(hello)
    }

    async fn accept_one(&self, stream: TcpStream) -> Result<(), FederationError> {
        // 认证：Bearer token 必须匹配某个配置的 peer（与 management WS 同模式）。
        let tokens: Vec<String> = self
            .peers
            .read()
            .await
            .iter()
            .map(|p| p.token.clone())
            .collect();
        let (ws, presented_token) =
            accept_with_auth(stream, &tokens, self.observed_hosts.clone()).await?;
        self.run_link(ws, None, presented_token).await
    }

    /// 握手 + 链路主循环（连入/连出共用；`dial_peer` = Some 时为主动方）。
    /// `link_token`：连入侧 = 认证呈现的 Bearer；连出侧 = peer 配置 token。
    async fn run_link<S>(
        &self,
        ws: tokio_tungstenite::WebSocketStream<S>,
        dial_peer: Option<PeerConfig>,
        link_token: String,
    ) -> Result<(), FederationError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut write, mut read) = ws.split();
        let hello = FedFrame::Hello(self.hello());

        // 主动方（Bearer 已在 connect 请求头完成）先发 Hello；被动方先收 Hello。
        if dial_peer.is_some() {
            let text = serde_json::to_string(&hello)?;
            send_with_timeout(&mut write, Message::Text(text)).await?;
        }

        // 等对方 Hello（被动方）或作为主动方已发 Hello 后等 Welcome。
        let first = tokio::time::timeout(HELLO_TIMEOUT, read.next())
            .await
            .map_err(|_| FederationError::HelloTimeout)?
            .ok_or(FederationError::LinkClosedBeforeHello)??;
        let peer_hello: NodeHello = match serde_json::from_str::<FedFrame>(first.to_text()?) {
            Ok(FedFrame::Hello(h)) | Ok(FedFrame::Welcome(h)) => h,
            _ => {
                return Err(FederationError::Handshake(
                    "first frame is not Hello/Welcome".into(),
                ))
            }
        };
        self.check_peer(&peer_hello)?;

        if dial_peer.is_none() {
            // 被动方回握。
            let text = serde_json::to_string(&FedFrame::Welcome(self.hello()))?;
            send_with_timeout(&mut write, Message::Text(text)).await?;
        } else {
            // 主动方应收到 Welcome；若收到的是 Hello（双方同时 dial），按
            // node_id 字典序小的一方断开重连（天然防撞）。
            if matches!(
                serde_json::from_str::<FedFrame>(first.to_text()?),
                Ok(FedFrame::Hello(_))
            ) && self.node_id < peer_hello.node_id
            {
                return Err(FederationError::Handshake(
                    "simultaneous dial, backing off".into(),
                ));
            }
        }

        let peer_name = match &dial_peer {
            Some(p) => p.name.clone(),
            // 接受侧（2026-10 修复）：peer_name 必须解析为**本机配置名**——
            // 此前直接用对端 node_id，导致三条链路全断：
            // 1. 策略查找（peer_policies 按配置名存）静默落空 → 连入方向
            //    全部按默认（全拒）裁决，Panel 配的权限对连入链路无效；
            // 2. peer_names / node_to_peer 映射键错（远程调用/迁移反查不到）；
            // 3. 占位提升（见 main.rs）拿不到配置名。
            // 认证已校验 token 匹配，故按 token 反查运行时 peer 表即可。
            None => {
                self.resolve_accept_peer_name(&link_token, &peer_hello.node_id)
                    .await
            }
        };
        let info = PeerInfo {
            node_id: peer_hello.node_id.clone(),
            node_name: peer_hello.node_name,
            version: peer_hello.version,
            caps: peer_hello.caps,
            advertise: peer_hello.advertise.clone(),
            peer_name,
        };
        tracing::info!(peer = %info.node_id, name = %info.peer_name, "federation link up");

        // 双向 dial 去重（2026-10，配对无方向性）：双方互知地址后会同时
        // 从两端拨号，可能并存两条连接。确定性规则——**由 node_id 较小
        // 的一方拨出的连接为"优选"**（两端各自独立计算即得同一结论）：
        // - 优选连接：顶掉存量（存量可能是非优选，或已死的旧链路——
        //   重连速度不受影响）；被顶链路主动退出（见 link_loop 分支）
        // - 非优选连接：若存量是优选 → 放弃本连接（bail，保留优选）；
        //   否则照常顶掉（存量陈旧/同为救援链路）
        // - 无存量：无论优劣先建（对端可能根本拨不进来——非优选方先
        //   行成链，等优选方拨入后自然接管）。
        let preferred = if dial_peer.is_some() {
            self.node_id < info.node_id
        } else {
            info.node_id < self.node_id
        };
        let (out_tx, mut out_rx) = mpsc::channel::<FedFrame>(OUTBOUND_CAP);
        let generation = self
            .link_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let handle = LinkHandle {
            tx: out_tx,
            token: link_token.clone(),
            generation,
            preferred,
        };
        let replaced = {
            let mut links = self.links.write().await;
            if let Some(existing) = links.get(&info.node_id) {
                if existing.preferred && !preferred {
                    drop(links);
                    return Err(FederationError::NonPreferredLink {
                        node_id: info.node_id.clone(),
                    });
                }
            }
            links.insert(info.node_id.clone(), handle).is_some()
        };
        if replaced {
            tracing::info!(peer = %info.node_id, preferred, "federation link replaced (bidirectional dial or reconnect)");
            let _ = self
                .event_tx
                .send(LinkEvent::Down {
                    peer_node_id: info.node_id.clone(),
                    reason: "replaced by new link".into(),
                })
                .await;
        }
        self.last_peer_infos
            .write()
            .await
            .insert(info.peer_name.clone(), info.clone());
        let _ = self.event_tx.send(LinkEvent::Up(info.clone())).await;
        // 诊断登记（2026-10）：在线清零失败计数；记住对端宣告的可拨地址
        // （跨断链保留——地址漂移提示/一键修复的数据源）。
        self.mark_peer_online(&info.peer_name).await;
        self.note_peer_advertise(&info.peer_name, info.advertise.as_deref())
            .await;

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
            self.last_peer_infos.write().await.remove(&info.peer_name);
            tracing::info!(peer = %info.node_id, %reason, "federation link down");
        }
        result
    }

    /// 握手校验：版本兼容 + 非回环。
    fn check_peer(&self, hello: &NodeHello) -> Result<(), FederationError> {
        if hello.node_id == self.node_id {
            return Err(FederationError::LoopDetected {
                node_id: hello.node_id.clone(),
            });
        }
        if hello.protocol_version != PROTOCOL_VERSION {
            return Err(FederationError::VersionMismatch {
                local: PROTOCOL_VERSION,
                peer: hello.protocol_version,
            });
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
    ) -> Result<(), FederationError>
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
                frame = out_rx.recv() => {
                    match frame {
                        Some(frame) => {
                            let text = serde_json::to_string(&frame)?;
                            send_with_timeout(&mut *write, Message::Text(text)).await?;
                        }
                        // 通道关闭 = 本链路已被顶掉（replace）/被移除：主动
                        // 退出并关闭连接——否则旧连接会成为"僵尸"一直心跳
                        // 存活（双向 dial 场景下持续堆积任务与 socket）。
                        None => return Err(FederationError::OutboundClosed),
                    }
                }
                // 入站
                inbound = read.next() => {
                    match inbound {
                        Some(Ok(Message::Text(text))) => {
                            last_seen = Instant::now();
                            match serde_json::from_str::<FedFrame>(&text) {
                                Ok(FedFrame::Ping { sent_at_ms }) => {
                                    let pong = FedFrame::Pong { sent_at_ms: now_ms(), echo_ms: sent_at_ms };
                                    send_with_timeout(&mut *write, Message::Text(serde_json::to_string(&pong)?)).await?;
                                }
                                Ok(FedFrame::Pong { .. }) => { /* 存活证据已刷新 */ }
                                Ok(frame) => {
                                    let event = LinkEvent::Frame { from: info.node_id.clone(), frame };
                                    if self.event_tx.send(event).await.is_err() {
                                        return Err(FederationError::EventSinkClosed);
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
                        Some(Ok(Message::Close(_))) | None => return Err(FederationError::PeerClosed),
                        Some(Err(e)) => return Err(FederationError::Ws(Box::new(e))),
                        _ => {}
                    }
                }
                // 心跳 + 判死
                _ = heartbeat.tick() => {
                    if last_seen.elapsed() > DEAD_AFTER {
                        return Err(FederationError::PeerSilent {
                            secs: DEAD_AFTER.as_secs(),
                        });
                    }
                    let ping = FedFrame::Ping { sent_at_ms: now_ms() };
                    send_with_timeout(&mut *write, Message::Text(serde_json::to_string(&ping)?)).await?;
                }
                _ = shutdown_rx.changed() => return Err(FederationError::Shutdown),
            }
        }
    }
}

/// 常量时间字符串相等：逐字节 XOR 累积，长度不等也走完全程再判，
/// 避免时序侧信道逐字节探测 token（与 `core/src/management.rs` 同款；
/// 联邦侧此前用 `==`，对齐统一——端口合并后两命名空间在同一监听端口
/// 上分类，比较必须同为常时）。
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 未授权（令牌缺失/不匹配）的握手拒绝响应。
///
/// 状态**必须是 4xx**（这里用 401）：`ErrorResponse::new` 的默认状态是
/// 200 OK，而 tungstenite 对「成功状态」的握手错误会直接放弃写出任何
/// 字节、静默断开——客户端只见「Handshake not finished」，把「令牌
/// 不对」误诊为网络/协议故障（与 `core/src/management.rs` 的同类修复
/// 对齐，2026-10）。
#[allow(clippy::result_large_err)]
fn unauthorized_response() -> tokio_tungstenite::tungstenite::handshake::server::ErrorResponse {
    let mut response = tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(Some(
        "unauthorized".into(),
    ));
    *response.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
    response
}

/// 连入侧认证 + WS upgrade。回调签名由 tungstenite 固定（Err 侧
/// `ErrorResponse` 体积大，与 `core/src/management.rs` 同款容忍）。
async fn accept_with_auth(
    stream: TcpStream,
    tokens: &[String],
    observed_hosts: Arc<std::sync::RwLock<Vec<String>>>,
) -> Result<(tokio_tungstenite::WebSocketStream<TcpStream>, String), FederationError> {
    let tokens = tokens.to_vec();
    let presented = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let presented2 = presented.clone();
    let ws = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
            // 观测地址（2026-11）：对端拨号用的 `Host` 就是「它眼中的我」，
            // 比本机推导的 advertise 更接近真相（NAT/端口映射下尤其如此）。
            if let Some(host) = request.headers().get("host").and_then(|v| v.to_str().ok()) {
                let cap = OBSERVED_HOST_CAP;
                crate::invite::note_observed(
                    &mut observed_hosts
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    host,
                    cap,
                );
            }
            let token = request
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_string);
            let ok = token
                .as_ref()
                .map(|t| {
                    tokens
                        .iter()
                        .any(|known| !known.is_empty() && constant_time_eq(known, t))
                })
                .unwrap_or(false);
            if ok {
                *presented2
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = token.unwrap_or_default();
                Ok(response)
            } else {
                Err(unauthorized_response())
            }
        },
    )
    .await?;
    let token = presented
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    Ok((ws, token))
}

/// 构造带 Bearer 的连出请求（dial 侧认证）。
pub fn bearer_request(
    url: &str,
    token: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, FederationError> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request()?;
    if !token.is_empty() {
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .map_err(|e| FederationError::InvalidTokenHeader(Box::new(e)))?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// 永不就绪的 Sink：模拟对端不读（TCP 发送缓冲写满后的挂起写）。
    struct StallingSink;

    impl futures_util::Sink<Message> for StallingSink {
        type Error = tokio_tungstenite::tungstenite::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }
        fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), Self::Error> {
            unreachable!("poll_ready 永不就绪，不应走到 start_send")
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }
    }

    /// 写超时必须生效：对端不读时裸 send 永久挂起会冻结整个 select 循环。
    #[tokio::test]
    async fn write_timeout_fires_when_peer_stalls() {
        let mut sink = StallingSink;
        let err = send_with_timeout_for(
            &mut sink,
            Message::Text("x".to_string()),
            Duration::from_millis(30),
        )
        .await
        .expect_err("写超时未触发");
        assert!(err.to_string().contains("timed out"), "错误信息不符: {err}");
    }

    /// 正常路径：可立即写出的 Sink 不受超时影响。
    #[tokio::test]
    async fn write_passes_through_when_ready() {
        let mut sink = futures_util::sink::drain::<Message>();
        send_with_timeout_for(
            &mut sink,
            Message::Text("ok".to_string()),
            Duration::from_millis(30),
        )
        .await
        .expect("就绪 Sink 不应超时");
    }

    fn test_fed() -> Arc<Federation> {
        let (tx, _rx) = mpsc::channel(8);
        let (shutdown, _) = watch::channel(false);
        Federation::new(
            "node-test".into(),
            Some("test".into()),
            NodeCaps::default(),
            Vec::new(),
            None,
            tx,
            shutdown,
        )
    }

    /// 观测地址（2026-11 云机配对事故）：入站连接的 `Host` 决定 Hello 宣告
    /// 的地址——手工 advertise 常停在 NAT 后的内网 IP，对端照着拨必然超时。
    #[tokio::test]
    async fn observed_host_overrides_advertised_address() {
        let (tx, _rx) = mpsc::channel(8);
        let (shutdown, _) = watch::channel(false);
        let fed = Federation::new(
            "node-test".into(),
            Some("test".into()),
            NodeCaps::default(),
            Vec::new(),
            Some("ws://10.1.0.8:3132".into()), // 云机自行推导出的内网地址
            tx,
            shutdown,
        );
        assert_eq!(fed.observed_host_sync(), None);
        assert_eq!(
            fed.hello().advertise.as_deref(),
            Some("ws://10.1.0.8:3132"),
            "未观测时回退构造期推导值"
        );

        fed.note_observed_host_sync("114.132.241.46:3132");
        assert_eq!(fed.observed_host_sync().as_deref(), Some("114.132.241.46"));
        assert_eq!(
            fed.hello().advertise.as_deref(),
            Some("ws://114.132.241.46:3132"),
            "观测到的公网地址替换主机部分（端口不变）"
        );

        // 回环/通配观测不覆盖有效值（否则一次本机访问就会毁掉邀请）。
        fed.note_observed_host_sync("127.0.0.1:3132");
        fed.note_observed_host_sync("localhost:3132");
        assert_eq!(fed.observed_host_sync().as_deref(), Some("114.132.241.46"));

        // 公网优先于私网：内网观测（如另一台 Panel 走 10.10.10.100）不顶掉
        // 已学到的公网地址，即使它更近。
        fed.note_observed_host_sync("10.10.10.100:3132");
        assert_eq!(fed.observed_host_sync().as_deref(), Some("114.132.241.46"));
    }

    /// 诊断计数（2026-10 地址漂移事故）：失败累计 → 在线清零；宣告地址
    /// 跨断链保留（诊断价值恰在断链时）；remove_peer 清理两张表。
    #[tokio::test]
    async fn diagnostics_track_failures_recovery_and_advertise() {
        let fed = test_fed();
        // 初始：全零/空。
        let d = fed.peer_diagnostics("p1").await;
        assert_eq!(d.consecutive_failures, 0);
        assert!(d.last_error.is_none() && d.last_online_at.is_none() && d.last_advertise.is_none());

        // 连续失败累计，最后错误刷新。
        assert_eq!(fed.note_dial_failure("p1", "conn refused").await, 1);
        assert_eq!(fed.note_dial_failure("p1", "timeout").await, 2);
        let d = fed.peer_diagnostics("p1").await;
        assert_eq!(d.consecutive_failures, 2);
        assert_eq!(d.last_error.as_deref(), Some("timeout"));

        // 学到对端宣告地址（跨断链保留）。
        fed.note_peer_advertise("p1", Some(" ws://10.0.0.9:3133 "))
            .await;
        fed.note_peer_advertise("p1", Some("")).await; // 空宣告不覆盖
        assert_eq!(
            fed.peer_diagnostics("p1").await.last_advertise.as_deref(),
            Some("ws://10.0.0.9:3133")
        );

        // 在线：清零失败、记录在线时刻；宣告地址保留。
        fed.mark_peer_online("p1").await;
        let d = fed.peer_diagnostics("p1").await;
        assert_eq!(d.consecutive_failures, 0);
        assert!(d.last_error.is_none());
        assert!(d.last_online_at.is_some());
        assert_eq!(d.last_advertise.as_deref(), Some("ws://10.0.0.9:3133"));

        // remove_peer 清理诊断（即便该 peer 从未在配置表里也要幂等）。
        fed.note_dial_failure("p1", "x").await;
        fed.remove_peer("p1").await;
        let d = fed.peer_diagnostics("p1").await;
        assert_eq!(d.consecutive_failures, 0);
        assert!(d.last_advertise.is_none());
    }
}
