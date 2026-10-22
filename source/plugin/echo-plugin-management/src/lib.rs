//! # echo-plugin-management — 管理面 WebSocket 插件（解耦计划 Phase 3 ⑧）
//!
//! Panel 的连接面：监听一个地址、接受 WS 连接，把命令/事件在 Panel 与 Agent
//! 之间中继。原 `source/core/management.rs` 的独立 crate 化。
//!
//! ## 为什么它本来就是「协议驱动」
//!
//! 中继两侧都只碰 `echo-protocol` 的类型：入站帧 → `BackendCommand` 塞进
//! bridge；bridge 的事件 → 带 `seq` + `entry id` 序列化成帧发回。**没有一处
//! 需要 `Agent` 句柄**——服务化后这一点被显式化：函数签名里不再有
//! `Arc<Agent>` 参数（此前它已标注 `_agent` 未使用）。
//!
//! ## 内容
//!
//! - [`EventBroker`]：单一事件接收者 → 每个连接一份订阅通道（防单个 Panel
//!   标签页饿死其它连接），并按投递顺序分配扇出序号；
//! - [`serve_with_token`] / [`serve_with_federation`]：独立监听 / 与联邦入站
//!   共用端口的两种启动形态；
//! - [`plan_federation_listen`] / [`validate_merged`]：端口合并的规划与
//!   fail-closed 校验（合并 = 管理通道与联邦入站同址，必须令牌可分）；
//! - [`timeline`]：**显示时间线的投影策略**——哪些事件进面板、字段怎么映射、
//!   工具完成态如何在原地合并、推理条目发往前端时怎么瘦身（全插件化 S3）；
//! - token 鉴权（`classify_token` / 常量时间比较）与心跳保活；
//! - **Panel 身份**（P1，2026-11）：`[federation] panel_keys` + `panel_access`
//!   让一个 Panel 用同一把密钥接入全网任一节点，服务端按级别（只读/操作/
//!   管理）强制边界——见 [`ConnRole::Panel`] 与 [`serve_management_ws`]。
//!
//! 依赖纪律：只依赖 `echo-protocol`（线契约）、`echo-federation`（合并端口
//! 时的对端连接接收）与 tokio/tungstenite——不依赖 `echo-agent`。

pub mod host_keys;
pub mod history;
pub mod session_view;
pub mod timeline;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use echo_protocol::{
    serialize_event_identified, BackendBridge, BackendEvent, DeltaBatcher, EntryTracker,
    SequencedEvent,
};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tracing::{
    debug,info, warn};

/// 联邦入站监听的缺省地址（组合根「端口合并」规划与 `port` 命令共用）。
///
/// 与组合根 `port_cmd::DEFAULT_FEDERATION_LISTEN` 同值；放在本 crate 是为
/// 让合并规划（[`plan_federation_listen`]）自带缺省值，不反向依赖组合根。
pub const DEFAULT_FEDERATION_LISTEN: &str = "0.0.0.0:3133";

/// 插件 id（`plugins.toml` / Panel 插件清单同一口径）。
pub const PLUGIN_ID: &str = "echo-agent.management.panel";

/// Broadcasts agent events to every connected Panel.
///
/// The in-process [`BackendBridge`] has a single event receiver; if each WS
/// connection drains that receiver directly, one Panel tab can starve the
/// others. Instead this broker owns the single receiver and fans each event
/// out to per-connection channels.
struct EventBroker {
    bridge: Arc<BackendBridge>,
    subscribers: Mutex<Vec<mpsc::UnboundedSender<SequencedEvent>>>,
    /// 下一条事件的序号（1 起；0 留给"从未收到过事件"）。
    ///
    /// 序号在**持锁投递之前**分配：与订阅者收到事件的顺序严格一致，
    /// 多订阅者（多窗口/多 Panel）看到的是同一套编号。Panel 据此做事件流
    /// 连续性校验——跳号即说明中间有帧没到（中继半死、重连窗口、后台
    /// 节流），前端随按游标补拉 timeline 修复，而不是让某条 ToolResult
    /// 静默丢失、工具行永远停在「运行中」（2026-10）。
    next_seq: AtomicU64,
    /// 条目身份追踪（见 `entry_identity`）：把"同一逻辑条目的多个事件"绑到
    /// 同一个 id 上，随帧的 `entry` 顶层字段下发。与 `next_seq` 一样在**持锁
    /// 投递之前**推进，保证所有订阅者看到同一套身份。
    entries: Mutex<EntryTracker>,
}

impl EventBroker {
    fn new(bridge: Arc<BackendBridge>) -> Self {
        Self {
            bridge,
            subscribers: Mutex::new(Vec::new()),
            next_seq: AtomicU64::new(1),
            entries: Mutex::new(EntryTracker::new()),
        }
    }

    async fn subscribe(&self) -> mpsc::UnboundedReceiver<SequencedEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.lock().await.push(tx);
        rx
    }

    /// 事件循环：接收 → 条目身份 → **增量合批** → 扇出。
    ///
    /// 合批（2026-10）：provider 的流式节奏很碎（一段中等回答 20–60 个增量），
    /// 每个增量在 Panel 侧都是一次完整链路、也是一帧 WS 消息。把同一段连续
    /// 增量在 60 ms 窗口内合并成一帧，语义不变（拼接后的 `delta` 就是原增量
    /// 顺序拼接），但帧数与 Panel 侧的重算次数同比例下降。
    ///
    /// **条目身份必须在合批之前推进**（`observe` 会因工具开始而封口条目）：
    /// 若等到冲批时才 `observe`，被延后的增量会拿到封口后的新 id，Panel 就
    /// 配不上它那一行。
    ///
    /// 保序由 [`DeltaBatcher`] 保证：任何非增量事件到达时先把挂起的增量全部
    /// 冲出，再发它自己（工具开始 = 分步边界，不能让它越过前面的叙述）。
    async fn run(&self) {
        let mut rx = self.bridge.event_rx.lock().await;
        let mut batcher = DeltaBatcher::default();
        // 20 ms 的到期检查（窗口 60 ms）：代价可忽略，延迟上界 = 窗口 + 20 ms。
        let mut ticker = tokio::time::interval(Duration::from_millis(20));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                maybe = rx.recv() => {
                    let Some(event) = maybe else {
                        // 桥关闭：把挂起的增量冲出后退出（不丢最后一段文本）。
                        for emit in batcher.drain() {
                            self.deliver(emit.event, emit.entry).await;
                        }
                        return;
                    };
                    let entry = self.entries.lock().await.observe(&event);
                    for emit in batcher.push(event, entry, Instant::now()) {
                        self.deliver(emit.event, emit.entry).await;
                    }
                }
                _ = ticker.tick() => {
                    for emit in batcher.take_due(Instant::now()) {
                        self.deliver(emit.event, emit.entry).await;
                    }
                }
            }
        }
    }

    /// 分配扇出序号并投递给所有订阅者（死订阅者就地摘除）。
    async fn deliver(&self, event: BackendEvent, entry: Option<String>) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut subscribers = self.subscribers.lock().await;
        subscribers.retain(|tx| {
            tx.send(SequencedEvent {
                seq,
                entry: entry.clone(),
                event: event.clone(),
            })
            .is_ok()
        });
    }
}

/// Start the management server with an optional bearer token.
pub async fn serve_with_token(
    addr: &str,
    bridge: Arc<BackendBridge>,
    access_token: String,
) -> anyhow::Result<()> {
    serve_internal(addr, bridge, access_token, None, None).await
}

/// 独立监听 + 联邦观测出处（2026-11）：监听与管理面行为同
/// [`serve_with_token`]，但升级回调会把入站连接的 `Host` 记进
/// [`echo_federation::Federation::note_observed_host_sync`]——邀请串据此
/// 选「别人连我时实际用的地址」（独立监听时联邦不经本端口，所以这里只
/// 借用它的观测表，不分流）。
pub async fn serve_with_token_observing(
    addr: &str,
    bridge: Arc<BackendBridge>,
    access_token: String,
    observer: Option<Arc<echo_federation::Federation>>,
) -> anyhow::Result<()> {
    serve_internal(addr, bridge, access_token, None, observer).await
}

/// 端口合并监听端（`[core].management_address` == `[federation].listen`）：单监听按令牌命名空间分类——
/// 管理令牌 → 管理 WS；peer 令牌 → 联邦链路；其余 → 标准 HTTP 401。
pub async fn serve_with_federation(
    addr: &str,
    bridge: Arc<BackendBridge>,
    access_token: String,
    federation: Arc<echo_federation::Federation>,
) -> anyhow::Result<()> {
    serve_internal(addr, bridge, access_token, Some(federation.clone()), Some(federation)).await
}

async fn serve_internal(
    addr: &str,
    bridge: Arc<BackendBridge>,
    access_token: String,
    federation: Option<Arc<echo_federation::Federation>>,
    observer: Option<Arc<echo_federation::Federation>>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind management address {addr}"))?;
    // 裸奔提醒：无 token 且绑定在非回环地址时，任何能到达该端口的人
    // 都能以管理权限控制 Agent——启动时明确 warn。（合并模式在装配层
    // 已 fail-closed 拒绝该组合；此处保留 warn 作为防御兜底。）
    if access_token.is_empty() && !is_loopback_addr(addr) {
        warn!(
            addr,
            "management_access_token 为空且管理 WS 绑定在非回环地址—— \
             任何可访问该端口的客户端都可全权控制 Agent（建议配置 token 或改绑 127.0.0.1）"
        );
    }
    if federation.is_some() {
        info!(
            "Panel management WS server listening on {addr}（端口合并：管理 + 联邦，按令牌分流）"
        );
    } else {
        info!("Panel management WS server listening on {addr}");
    }
    serve_with_listener_and_token(listener, bridge, access_token, federation, observer).await
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

/// 联邦监听的最终布局规划（2026-10「缺省合并」语义）：
///
/// | `[federation].listen` | 结果 |
/// |---|---|
/// | **未配置**（常见） | **缺省合并**：联邦监听 = 管理地址（单监听按令牌分流）；下列保守场景**回退**独立监听：① 管理地址为回环（合并会把联邦拉进回环、外部 peer 不可达）；② 管理令牌为空且非回环（不在无认证端口上同时开管理+联邦）；③ 管理令牌与某 peer 令牌相同（升级回调无法区分两类连接） |
/// | 显式地址 | 按显式值（与管理同址 = 合并；其余 = 独立监听） |
/// | 显式空串 | 纯连出（不监听） |
///
/// 返回 `(merged, effective_listen, fallback_reason)`：
/// - `merged == true`：联邦入站合并在管理监听端（`effective` = 管理地址）；
/// - `merged == false`：`effective` = 联邦独立监听地址；`None` = 不监听；
/// - `fallback_reason`：缺省合并被回退时的可读原因（启动期 warning /
///   `port` 清单注脚用）。
///
/// `has_peer_token`：管理令牌是否与某 peer 令牌相同（保守回退条件 ③；
/// 显式合并的对应场景由 [`validate_merged`] fail-closed 拒绝启动）。
pub fn plan_federation_listen(
    management_address: &str,
    management_token: &str,
    federation_listen: &str,
    listen_explicit: bool,
    has_peer_token: impl Fn(&str) -> bool,
) -> (bool, Option<String>, Option<&'static str>) {
    let management = management_address.trim();
    if listen_explicit {
        let listen = federation_listen.trim();
        if listen.is_empty() {
            return (false, None, None); // 显式空串 = 纯连出
        }
        if !management.is_empty() && listen == management {
            return (true, Some(management.to_string()), None); // 显式同址 = 合并
        }
        return (false, Some(listen.to_string()), None); // 显式独立
    }
    // 未配置：缺省合并（联邦跟随管理端口）；以下保守场景回退独立监听。
    let independent = || (false, Some(DEFAULT_FEDERATION_LISTEN.to_string()));
    // ① 管理地址为空/回环：合并会把联邦监听拉进回环（或地址无效），外部
    //    peer 再也连不进来（联邦需要对外可达）。
    if management.is_empty() || is_loopback_addr(management) {
        let (m, l) = independent();
        return (m, l, None);
    }
    // ② 空令牌 + 非回环：合并会把管理通道与联邦入站一起放在无认证端口上
    //    （显式合并会被 fail-closed 校验拒绝）——缺省场景不拒绝启动。
    if management_token.is_empty() {
        let (m, l) = independent();
        return (
            m,
            l,
            Some("management_access_token 为空且管理地址非回环——缺省合并回退独立监听（配置令牌后自动合并）"),
        );
    }
    // ③ 管理令牌与某 peer 令牌相同：合并监听无法区分两类连接（显式合并
    //    同样会被 fail-closed 校验拒绝）。
    if has_peer_token(management_token) {
        let (m, l) = independent();
        return (
            m,
            l,
            Some("management_access_token 与某 peer 令牌相同——缺省合并回退独立监听（合并监听无法区分管理/联邦连接；换用不同令牌后自动合并）"),
        );
    }
    (true, Some(management.to_string()), None)
}

/// 端口合并的启动校验（安全条件 fail-closed）：
/// - 合并 + 管理令牌为空 + 绑定非回环 → 拒绝启动——合并把管理通道暴露
///   在联邦端口上，空令牌 = 任何可达者全权控制 Agent；
/// - 管理令牌与某 peer 令牌相同 → 拒绝启动——升级回调无法区分两类
///   连接（路由歧义）。
pub fn validate_merged(
    management_address: &str,
    management_token: &str,
    has_peer_token: impl Fn(&str) -> bool,
) -> anyhow::Result<()> {
    if management_token.is_empty() && !is_loopback_addr(management_address) {
        anyhow::bail!(
            "端口合并配置非法：management_address = {management_address} 与 federation.listen 同址（合并模式），\
             但 management_access_token 为空且绑定非回环地址——任何可达该端口的客户端都可全权控制 Agent。\
             补救：运行 `echo-agent-core token --rotate` 生成令牌后重启；或改绑 127.0.0.1；或让两地址不同（回退独立模式）。"
        );
    }
    if !management_token.is_empty() && has_peer_token(management_token) {
        anyhow::bail!(
            "端口合并配置非法：management_access_token 与某个 [federation.peers.*] token 相同——\
             升级回调无法区分管理/联邦连接。请为两者使用不同令牌（改 peer token 或 `echo-agent-core token --rotate`）。"
        );
    }
    Ok(())
}

/// Panel 身份配置的启动期校验（P1，2026-11）——**fail-closed**。
///
/// 三类令牌共用"Bearer 命名空间"（合并模式下同一个监听端口），所以必须
/// 互不相同，否则连接意图不可判定（同一把密钥既是管理又是 Panel，升级回调
/// 只能二选一，另一种用法静默降级）。这条与 [`validate_merged`] 的令牌冲突
/// 校验同源，但**在独立模式下也要跑**：Panel 密钥与管理令牌相同时，运维改
/// 一次 `[federation].listen` 就会让行为变脸。
///
/// `level` 来自 `[federation].panel_access`；`keys` 来自 `panel_keys`。
pub fn validate_panel_identity(
    keys: &[String],
    level: echo_federation::PanelAccess,
    management_token: &str,
    has_peer_token: impl Fn(&str) -> bool,
) -> anyhow::Result<()> {
    let keys: Vec<&String> = keys.iter().filter(|k| !k.trim().is_empty()).collect();
    // ① 声明了开放级别却没配密钥：配置写错了（多半是漏了一行）。
    //    不静默按 off 处理——那会让运维以为已开放，实际全部 401。
    if level.is_open() && keys.is_empty() {
        anyhow::bail!(
            "[federation] panel_access = {} 但没有配置任何 panel_keys——\
             请补上 panel_keys，或把 panel_access 改回 \"off\"",
            level.as_str()
        );
    }
    // ② 配了密钥却级别是 off：同样是"差不多能用"的错觉，明确报错。
    if !level.is_open() && !keys.is_empty() {
        anyhow::bail!(
            "配置了 [federation] panel_keys 但 panel_access = \"off\"（缺省值）——\
             请显式设为 readonly / operate / admin 之一，或删掉 panel_keys"
        );
    }
    // ③ 与其它令牌撞车：不可判定。
    for key in &keys {
        let key = key.trim();
        if !management_token.is_empty() && constant_time_eq(key, management_token) {
            anyhow::bail!(
                "[federation] panel_keys 与 management_access_token 相同——\
                 两者必须使用不同密钥（连接意图不可判定）。建议：\
                 `echo-agent-core token --rotate` 换管理令牌，或换一把 panel 密钥。"
            );
        }
        if has_peer_token(key) {
            anyhow::bail!(
                "[federation] panel_keys 与某个 [federation.peers.*] token 相同——\
                 两者必须使用不同密钥（Panel 身份会被误判为联邦链路）。"
            );
        }
    }
    Ok(())
}

/// Serve on a pre-bound listener (tests use it to pick a free port).
#[cfg(test)]
pub(crate) async fn serve_with_listener(
    listener: TcpListener,
    bridge: Arc<BackendBridge>,
) -> anyhow::Result<()> {
    serve_with_listener_and_token(listener, bridge, String::new(), None, None).await
}

async fn serve_with_listener_and_token(
    listener: TcpListener,
    bridge: Arc<BackendBridge>,
    access_token: String,
    federation: Option<Arc<echo_federation::Federation>>,
    observer: Option<Arc<echo_federation::Federation>>,
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
        // TCP 层：此时既没握手也没分类（可能是 401、联邦链路、探活）——
        // 记 debug，真正的「Panel 链路已建立」在角色判定之后打（见
        // `handle_connection`），否则统计出来的连接数会虚高且不可解释。
        debug!(%peer, "management listener accepted a connection");

        let events = events.clone();
        let bridge = bridge.clone();
        let access_token = access_token.clone();
        let federation = federation.clone();
        let observer = observer.clone();
        // Panel 身份出处：合并模式 = 同一联邦句柄；独立模式 = 观测句柄
        // （两者都指向本节点的 `Arc<Federation>`，装配层保证）。
        let panel_source = federation.clone().or_else(|| observer.clone());
        tokio::spawn(async move {
            if let Err(e) = handle_connection(
                stream,
                peer,
                events,
                bridge,
                &access_token,
                federation,
                observer,
                panel_source,
            )
            .await
            {
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
    peer: std::net::SocketAddr,
    events: Arc<EventBroker>,
    bridge: Arc<BackendBridge>,
    access_token: &str,
    // 合并模式的联邦句柄（Some = 联邦入站与本端口共用监听）。
    federation: Option<Arc<echo_federation::Federation>>,
    // 入站 Host 观测出处（独立模式也用它学"别人怎么连我"）。
    observer: Option<Arc<echo_federation::Federation>>,
    // **Panel 身份密钥表**出处（P1）：合并模式下与 `federation` 同一个句柄，
    // 独立模式下就是 `observer`。单独一个参数是为了让"谁提供 panel 判定"
    // 在调用点显式可读（两者在两种模式下恰好同源，但不是同一个概念）。
    panel_source: Option<Arc<echo_federation::Federation>>,
) -> anyhow::Result<()> {
    match federation {
        // 独立模式：管理通道 + Panel 身份（`panel_source` 提供 Panel 密钥表；
        // `observer` 仅用于记录入站 Host）。
        None => {
            let (ws, access) = accept_standalone_ws(stream, access_token, panel_source, observer)
                .await
                .inspect_err(|e| {
                    // 401 / 非 WS / 探测扫描都从这里返回：此前完全没有日志，
                    // "Panel 连不上"在 Core 侧看不到任何痕迹。
                    warn!(%peer, error = %e, "management handshake rejected");
                })?;
            info!(
                %peer,
                merged = false,
                access = panel_access_str(access),
                "panel link up"
            );
            serve_management_ws(ws, events, bridge, false, access, peer).await
        }
        // 合并模式：升级回调按令牌分类，升级后按角色分派。
        Some(fed) => {
            let (ws, role, presented) = accept_merged_ws(stream, access_token, fed.clone())
                .await
                .inspect_err(|e| {
                    warn!(%peer, error = %e, "management handshake rejected");
                })?;
            match role {
                ConnRole::Management => {
                    info!(%peer, merged = true, access = "admin", "panel link up");
                    serve_management_ws(
                        ws,
                        events,
                        bridge,
                        true,
                        echo_federation::PanelAccess::Admin,
                        peer,
                    )
                    .await
                }
                // Panel 身份：同一条管理 WS，但权限受级别约束（只读时拒绝
                // 一切写命令，见 `serve_management_ws` 的门）。
                ConnRole::Panel(access) => {
                    // 联邦级 Panel 身份（跨机接入通常走这条）：记下权限级别，
                    // 只读被拒的命令才有据可查。
                    info!(
                        %peer,
                        merged = true,
                        access = panel_access_str(access),
                        "panel link up"
                    );
                    serve_management_ws(ws, events, bridge, true, access, peer).await
                }
                ConnRole::Federation => {
                    let result = fed.accept_ws_pub(ws, presented).await;
                    if let Err(ref e) = result {
                        // anyhow 的 `%e`（Display）只打印最外层 context——合并
                        // 端口上链路失败排查需要完整链（含根因），用 `?e`。
                        warn!(error = ?e, "federation link on merged port failed");
                    }
                    result.context("federation link on merged port")
                }
            }
        }
    }
}

/// 合并监听端的连接角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnRole {
    Management,
    Federation,
    /// **联邦级 Panel 身份**（P1，2026-11）：持 `[federation] panel_keys` 之一
    /// 的客户端。走管理 WS，但权限由 `panel_access` 决定（`Readonly` 时
    /// 只放行 `PANEL_READONLY_COMMANDS`）。
    Panel(echo_federation::PanelAccess),
}

/// 合并监听端的令牌分类（决策表如下）：
/// 1. == 管理令牌（非空）→ 管理；2. ∈ peer 令牌表 → 联邦；
/// 3. ∈ Panel 密钥表 → **Panel 身份**（级别由节点配置决定）；
/// 4. 管理令牌为空 → 管理（兼容旧监听行为；非回环合并已在启动期拒绝）；
/// 5. 其余（未知令牌 / 缺令牌且管理令牌非空）→ `None`（401）。
///
/// **顺序即优先级**：管理令牌排在最前，所以即使运维把 Panel 密钥误配成
/// 管理令牌，本机管理面也不会因为"看起来像 panel"而被降级（启动期还有
/// `validate_panel_identity` fail-closed 拦住这种配置）。
///
/// `has_peer_token("")` / `panel_of("")` 必须为 None（生产实现由
/// `Federation::has_peer_token_sync` / `panel_gate_sync` 保证：空凭据恒不
/// 命中——空串不是"万能令牌"）。
fn classify_token(
    presented: &str,
    management_token: &str,
    has_peer_token: impl Fn(&str) -> bool,
    panel_of: impl Fn(&str) -> Option<echo_federation::PanelAccess>,
) -> Option<ConnRole> {
    if !management_token.is_empty() && constant_time_eq(presented, management_token) {
        return Some(ConnRole::Management);
    }
    if has_peer_token(presented) {
        return Some(ConnRole::Federation);
    }
    // 空凭据**永不**归为 Panel（纵深防御：即便注入的判定闭包把空串当成
    // 命中——例如将来有人写出"未配置密钥 = 不校验"的那种实现——这里也拦住。
    // 无 Authorization 头的连接最多落到下面的"管理令牌为空"兼容分支。）
    if !presented.is_empty() {
        if let Some(access) = panel_of(presented) {
            return Some(ConnRole::Panel(access));
        }
    }
    if management_token.is_empty() {
        return Some(ConnRole::Management);
    }
    None
}

/// 独立模式升级：管理令牌 → 管理；Panel 密钥 → Panel 身份；无令牌 = 不校验
/// （兼容旧行为，一律按管理）。
///
/// 返回（已升级连接, 该连接的权限级别）。`Management` 恒等于
/// [`echo_federation::PanelAccess::Admin`]——本机管理面本来就是全权。
///
/// 为什么独立模式也要认 Panel 密钥：`[federation] listen` 显式独立（或
/// 缺省合并被保守回退）时，Panel 身份的入口就在管理监听上。若只在合并
/// 模式认，运维一改 `listen` 就会得到"同一把密钥有时能连、有时 401"的
/// 不一致行为。
#[allow(clippy::result_large_err)]
async fn accept_standalone_ws(
    stream: tokio::net::TcpStream,
    access_token: &str,
    panel_source: Option<Arc<echo_federation::Federation>>,
    observer: Option<Arc<echo_federation::Federation>>,
) -> anyhow::Result<(
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    echo_federation::PanelAccess,
)> {
    use echo_federation::PanelAccess as Level;
    // 独立监听：没有令牌时也要能观测 `Host`（升级回调是唯一的时机——升级
    // 完成后握手头就没了），所以空/非空令牌两条路径都走带回调的升级。
    let access_token = access_token.to_string();
    let observe = move |request: &tokio_tungstenite::tungstenite::handshake::server::Request| {
        if let Some(federation) = observer.as_ref() {
            if let Some(host) = request.headers().get("host").and_then(|v| v.to_str().ok()) {
                federation.note_observed_host_sync(host);
            }
        }
    };
    // 命中的权限级别由升级回调写入（与合并模式同款：回调是同步且 'static，
    // 结果只能经共享槽位带回）。
    let level_out = Arc::new(std::sync::Mutex::new(Level::Admin));
    let level_out2 = level_out.clone();
    let panel_source2 = panel_source.clone();
    let ws = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
            observe(request);
            let presented = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .unwrap_or("");
            let level = if !access_token.is_empty() && constant_time_eq(presented, &access_token) {
                Level::Admin
            } else if let Some(level) = panel_source2
                .as_ref()
                .and_then(|fed| fed.panel_gate_sync(presented))
            {
                level
            } else if access_token.is_empty() {
                // 兼容旧监听行为（无令牌 = 不校验）。
                Level::Admin
            } else {
                return Err(unauthorized_response());
            };
            *level_out2
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = level;
            Ok(response)
        },
    )
    .await?;
    let level = *level_out
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok((ws, level))
}

/// 合并模式升级：按令牌命名空间分类；未命中即标准 401（非静默断开）。
/// 返回（已升级连接, 角色, 呈现的 Bearer）。
#[allow(clippy::result_large_err)]
async fn accept_merged_ws(
    stream: tokio::net::TcpStream,
    management_token: &str,
    federation: Arc<echo_federation::Federation>,
) -> anyhow::Result<(
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ConnRole,
    String,
)> {
    let management_token = management_token.to_string();
    let role_out = Arc::new(std::sync::Mutex::new(None));
    let token_out = Arc::new(std::sync::Mutex::new(String::new()));
    let role_out2 = role_out.clone();
    let token_out2 = token_out.clone();
    let ws = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
            // 观测地址（2026-11）：客户端连本机用的 `Host` 就是「它眼中的
            // 我」——邀请串优先用它（手工 advertise 常停在过期内网 IP）。
            if let Some(host) = request.headers().get("host").and_then(|v| v.to_str().ok()) {
                federation.note_observed_host_sync(host);
            }
            let presented = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .unwrap_or("");
            match classify_token(
                presented,
                &management_token,
                |t| federation.has_peer_token_sync(t),
                |t| federation.panel_gate_sync(t),
            ) {
                Some(role) => {
                    *role_out2
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(role);
                    *token_out2
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = presented.to_string();
                    Ok(response)
                }
                None => Err(unauthorized_response()),
            }
        },
    )
    .await?;
    let role = role_out
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .expect("merged classifier sets role before upgrade succeeds");
    let presented = token_out
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    Ok((ws, role, presented))
}

/// 管理通道连接主体（升级后；独立/合并模式共用）。
///
/// `merged_hint` = 该连接来自合并监听端（用于联邦帧误连的诊断增强：
/// 管理令牌为空的核心上，未知 peer 令牌的连接会经兼容分支被归为管理，
/// 若其首帧是联邦帧则明确记录并断开，避免对端黑盒等超时）。
/// `access` = 本连接的权限级别（P1，2026-11）：
///
/// - 本机管理令牌 / Panel 身份的 `operate` / `admin` → 不设门（与原行为一致）；
/// - Panel 身份的 `readonly` → **只放行** [`echo_protocol::PANEL_READONLY_COMMANDS`]，
///   其余命令就地拒绝并回一条 `Error` 事件（而不是静默丢弃——客户端必须知道
///   "我不是没权限，是被降级了"）；
/// - `off` 理论上不会走到这里（握手期已 401），出现即按只读处理（fail-closed）。
/// Panel 权限级别的日志短名（`PanelAccess` 的 Debug 输出会随枚举增长变，
/// 日志字段要稳定可聚合）。
fn panel_access_str(access: echo_federation::PanelAccess) -> &'static str {
    use echo_federation::PanelAccess;
    match access {
        PanelAccess::Off => "off",
        PanelAccess::Readonly => "readonly",
        PanelAccess::Operate => "operate",
        PanelAccess::Admin => "admin",
    }
}

async fn serve_management_ws(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    events: Arc<EventBroker>,
    bridge: Arc<BackendBridge>,
    merged_hint: bool,
    access: echo_federation::PanelAccess,
    peer: std::net::SocketAddr,
) -> anyhow::Result<()> {
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

    // 链路收尾诊断（此前这条连接**没有任何退出日志**：Panel 侧看到的断线在
    // Core 里查不到痕迹，无从判断是"对端关的"还是"本端判死的"）。
    let started = std::time::Instant::now();
    let mut events_out: u64 = 0;
    let mut commands_in: u64 = 0;
    // 初值只兜底「select 因其它原因直接落出循环」，正常路径都会被下面覆盖。
    #[allow(unused_assignments)]
    let mut close_reason = "loop exited without a recorded reason";

    loop {
        tokio::select! {
            // Agent → Panel 事件转发。
            ev = event_rx.recv() => {
                match ev {
                    Some(ev) => {
                        // 带扇出序号下发：Panel 据此校验事件流连续性，跳号即
                        // 按游标补拉 timeline（2026-10，见 web/src/event-seq.ts）。
                        let text =
                            serialize_event_identified(ev.seq, ev.entry.as_deref(), &ev.event);
                        if write
                            .send(tokio_tungstenite::tungstenite::Message::Text(text))
                            .await
                            .is_err()
                        {
                            // client gone — subscriber is dropped on return
                            close_reason = "event write failed (peer gone)";
                            break;
                        }
                        events_out += 1;
                    }
                    None => {
                        close_reason = "event broker closed";
                        break;
                    }
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
                                        // 只读 Panel 身份的门（P1）：边界在服务端
                                        // 强制，客户端自律不算边界。
                                        if let Some(reason) = readonly_rejection(access, &cmd) {
                                            let frame = echo_protocol::serialize_event(
                                                &echo_protocol::BackendEvent::Error {
                                                    session_id: None,
                                                    message: reason,
                                                },
                                            );
                                            if write
                                                .send(tokio_tungstenite::tungstenite::Message::Text(
                                                    frame,
                                                ))
                                                .await
                                                .is_err()
                                            {
                                                close_reason = "event write failed (peer gone)";
                                                break;
                                            }
                                            continue;
                                        }
                                        commands_in += 1;
                                        let _ = bridge.send_command(cmd);
                                    }
                                    None => {
                                        // 合并端诊断增强：疑似联邦帧到达管理
                                        // 连接（未知 peer 令牌 + 空管理令牌的
                                        // 兼容分支）——明确记录并断开。
                                        if merged_hint
                                            && serde_json::from_str::<echo_federation::FedFrame>(
                                                &text,
                                            )
                                            .is_ok()
                                        {
                                            warn!(
                                                "merged port: federation frame on a management \
                                                 connection — likely a peer with an unknown token; \
                                                 closing (check [federation.peers.*] token)"
                                            );
                                            close_reason = "federation frame on management link";
                                            break;
                                        }
                                    }
                                }
                            }
                            tokio_tungstenite::tungstenite::Message::Close(frame) => {
                                // 关帧码是归因的决定性证据：1001=页面卸载（Panel
                                // 进程被重启也会这样）/ 1000=对端主动关 / 无码=异常。
                                match &frame {
                                    Some(frame) => info!(
                                        %peer,
                                        code = u16::from(frame.code),
                                        reason = %frame.reason,
                                        "panel sent close"
                                    ),
                                    None => info!(%peer, "panel sent close (no code)"),
                                }
                                close_reason = "peer sent close";
                                break;
                            }
                            _ => {}
                        }
                    }
                    // read 侧结束 = 对端 TCP 断开 / 协议错误：前者是浏览器或
                    // Panel 重启，后者多半是版本不匹配，值得区分。
                    Some(Err(e)) => {
                        // 错误文本是"网络掉了 vs 协议不匹配"的唯一直接证据。
                        warn!(%peer, error = %e, "panel read error");
                        close_reason = "read error (protocol or transport)";
                        break;
                    }
                    None => {
                        close_reason = "peer closed the socket";
                        break;
                    }
                }
            }
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > std::time::Duration::from_secs(90) {
                    // 判死：90s 内没有任何入站帧（含 Pong）。这条以前是静默
                    // break——空闲链路的"无故断开"根本查不出来。
                    warn!(
                        %peer,
                        idle_s = last_seen.elapsed().as_secs(),
                        "panel link idle timeout; closing (no inbound frame for 90s)"
                    );
                    close_reason = "idle timeout (no inbound frame for 90s)";
                    break;
                }
                if write
                    .send(tokio_tungstenite::tungstenite::Message::Ping(Vec::new()))
                    .await
                    .is_err()
                {
                    close_reason = "heartbeat write failed (peer gone)";
                    break;
                }
            }
        }
    }

    info!(
        %peer,
        merged = merged_hint,
        access = panel_access_str(access),
        elapsed_s = started.elapsed().as_secs(),
        events_out,
        commands_in,
        idle_ms = last_seen.elapsed().as_millis() as u64,
        reason = close_reason,
        "panel link down"
    );
    Ok(())
}

/// One parsed inbound frame from the Panel.
#[derive(Debug)]
pub(crate) enum InboundFrame {
    Command(echo_protocol::BackendCommand),
}

/// Parse one inbound WS text frame.
///
/// Returns `None` for non-command frames (events, malformed payloads) —
/// those are ignored by the management channel.
pub(crate) fn handle_inbound_text(text: &str) -> Option<InboundFrame> {
    match echo_protocol::deserialize_message(text) {
        Some(echo_protocol::WsMessage::Command(cmd)) => Some(InboundFrame::Command(cmd)),
        _ => None,
    }
}

/// 只读 Panel 身份的**服务端门**（P1，2026-11）：返回 `Some(拒绝原因)` 即拦下。
///
/// 为什么是纯函数：门是安全边界，必须能脱离 WS/网络单测（`mod tests` 里
/// 有三条：放行观察命令、拒绝写命令、拒绝取密钥）。三个级别的语义：
///
/// | 级别 | 行为 |
/// |---|---|
/// | `Off` | 拒绝一切（理论上握手期已 401；这里是兜底） |
/// | `Readonly` | 只放行 `PANEL_READONLY_COMMANDS`，其余拒绝并说明 |
/// | `Operate` / `Admin` | 不设门（等同管理面） |
fn readonly_rejection(
    access: echo_federation::PanelAccess,
    cmd: &echo_protocol::BackendCommand,
) -> Option<String> {
    use echo_federation::PanelAccess;
    match access {
        PanelAccess::Off => Some("该连接没有操作权限（Panel 身份未授予）".to_string()),
        PanelAccess::Readonly => {
            let name = cmd.kind_name();
            if echo_protocol::is_panel_readonly_allowed(name) {
                None
            } else {
                Some(format!(
                    "只读 Panel 身份不能执行 `{name}`（本节点 [federation] panel_access = readonly）"
                ))
            }
        }
        PanelAccess::Operate | PanelAccess::Admin => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_protocol::BackendCommand;

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

    /// unit 变体的标准线格式：裸字符串。
    ///
    /// 与 Panel 的 protocol-sync 契约测试成对（`serializeCommand(
    /// 'RequestFederationStatus')` → `{"payload":"RequestFederationStatus"}`）
    /// ——2026-10 Panel 补齐 4 条联邦命令类型时，unit 变体统一改字符串形态。
    #[test]
    fn unit_command_parses_from_bare_string() {
        let text = r#"{"type":"command","payload":"RequestFederationStatus"}"#;
        match handle_inbound_text(text) {
            Some(InboundFrame::Command(cmd)) => {
                assert_eq!(cmd, BackendCommand::RequestFederationStatus);
            }
            other => panic!("expected command frame, got {other:?}"),
        }
    }

    /// 历史兼容：旧 Panel 曾以 `{"X": null}` 对象形态发 unit 命令（绕类型
    /// 系统的产物）；serde 宽容继续接受，不因 Panel 升级而破坏混布。
    #[test]
    fn unit_command_object_null_form_stays_compatible() {
        let text = r#"{"type":"command","payload":{"RequestFederationStatus":null}}"#;
        match handle_inbound_text(text) {
            Some(InboundFrame::Command(cmd)) => {
                assert_eq!(cmd, BackendCommand::RequestFederationStatus);
            }
            other => panic!("expected command frame, got {other:?}"),
        }
    }

    #[test]
    fn invoke_instance_op_command_roundtrips() {
        // 插件实例管理命令线格式（W2.1）：管理方法经统一通道直通。
        let text = r#"{"type":"command","payload":{"InvokeInstanceOp":{"instance":"qq-default","method":"set_gate_mode","payload":{"mode":"allowlist"}}}}"#;
        match handle_inbound_text(text) {
            Some(InboundFrame::Command(cmd)) => {
                assert_eq!(
                    cmd,
                    BackendCommand::InvokeInstanceOp {
                        instance: "qq-default".into(),
                        method: "set_gate_mode".into(),
                        payload: serde_json::json!({"mode": "allowlist"}),
                    }
                );
            }
            other => panic!("expected command frame, got {other:?}"),
        }
    }

    // ── End-to-end WS server ──────────────────────────────────────────────

    /// A provider that is never actually called (management needs an Agent).
    async fn start_test_server() -> (Arc<BackendBridge>, echo_protocol::BackendHandle, String) {
        let (bridge, handle) = echo_protocol::create_bridge();
        let bridge = Arc::new(bridge);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let b = bridge.clone();
        tokio::spawn(async move {
            let _ = serve_with_listener(listener, b).await;
        });
        (bridge, handle, addr)
    }

    #[tokio::test]
    async fn panel_command_reaches_agent_backend() {
        let (bridge, handle, addr) = start_test_server().await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("connect");

        let text = echo_protocol::serialize_command(&BackendCommand::SwitchModel {
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
        handle.emit(echo_protocol::BackendEvent::AgentOutput {
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
                if let Some(echo_protocol::WsMessage::Event(ev)) =
                    echo_protocol::deserialize_message(&text)
                {
                    received = Some(ev);
                    break;
                }
            }
        }
        let ev = received.expect("event forwarded to panel");
        match ev {
            echo_protocol::BackendEvent::AgentOutput { content, .. } => assert_eq!(content, "回复"),
            other => panic!("wrong event: {other:?}"),
        }
        let _ = bridge;
        ws.close(None).await.ok();
    }

    /// 流式增量合批（2026-10）在**扇出层**生效：连续同类增量合并为一帧下发，
    /// 且非增量事件（工具开始）先把挂起的增量冲出——保序是硬约束。
    ///
    /// 这条测试锁的是**接线**（`DeltaBatcher` 自己的单测在 echo-protocol），
    /// 因为合批一旦顺序错乱，Panel 就会把工具后的正文并进工具前那一行
    /// （历史上"思考 chip 堆到正文后面"的同型问题）。
    #[tokio::test]
    async fn stream_deltas_are_batched_but_ordered_before_tool_calls() {
        let (bridge, handle, addr) = start_test_server().await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("connect");
        // 订阅完成后再发事件（broker 只投给已订阅连接）。
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let delta = |text: &str| echo_protocol::BackendEvent::AgentContentDelta {
            session_id: "s1".into(),
            team_id: None,
            branch_id: "b1".into(),
            delta: text.into(),
        };
        // 三个连续增量 + 一个工具调用（分步边界）。
        handle.emit(delta("he"));
        handle.emit(delta("llo"));
        handle.emit(delta(" world"));
        handle.emit(echo_protocol::BackendEvent::ToolCall {
            session_id: "s1".into(),
            team_id: None,
            tool_name: "bash".into(),
            arguments: "{}".into(),
            tool_call_id: "c1".into(),
            branch_id: String::new(),
            started_at_ms: None,
        });

        // 收集帧，直到收到 ToolCall（或超时）。
        let mut frames: Vec<(String, Option<String>)> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(300), ws.next()).await {
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
                    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
                    let entry = value
                        .get("entry")
                        .and_then(|v| v.as_str())
                        .map(std::string::ToString::to_string);
                    let Some(echo_protocol::WsMessage::Event(ev)) =
                        echo_protocol::deserialize_message(&text)
                    else {
                        continue;
                    };
                    match ev {
                        echo_protocol::BackendEvent::AgentContentDelta { delta, .. } => {
                            frames.push((format!("delta:{delta}"), entry))
                        }
                        echo_protocol::BackendEvent::ToolCall { .. } => {
                            frames.push(("tool".into(), entry));
                            break;
                        }
                        _ => {}
                    }
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(e))) => panic!("ws error: {e}"),
                Ok(None) => break,
                Err(_) => {} // 等下一轮（合批窗口 60ms + 检查 20ms）
            }
        }

        assert!(!frames.is_empty(), "应至少收到一帧");
        // 最后一帧必须是工具调用（保序：它不能越过前面的增量）。
        assert_eq!(frames.last().unwrap().0, "tool", "frames={frames:?}");
        // 前面所有增量合并后的文本必须与原始顺序一致（不得丢字、不得乱序）。
        let merged: String = frames
            .iter()
            .filter_map(|(tag, _)| tag.strip_prefix("delta:"))
            .collect();
        assert_eq!(merged, "hello world", "frames={frames:?}");
        // 合批生效：三次增量不应各自成帧（窗口内应被合并）。
        let delta_frames = frames
            .iter()
            .filter(|(t, _)| t.starts_with("delta:"))
            .count();
        assert!(
            delta_frames < 3,
            "增量应被合批（实际 {delta_frames} 帧）：frames={frames:?}"
        );
        // 条目身份随帧下发（同一段流式 + 工具各自身份）。
        let tool_entry = frames.last().unwrap().1.clone().expect("tool entry");
        assert_eq!(tool_entry, "tool:s1:c1");
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
        let (bridge, _handle) = echo_protocol::create_bridge();
        let bridge = Arc::new(bridge);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let _ =
                serve_with_listener_and_token(listener, bridge, "secret-token".into(), None, None)
                    .await;
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

    // ── 端口合并（合并监听端：令牌分类 / 启动校验 / 端到端路由）──────

    /// 分类决策表六行 + 联邦优先于“空管理令牌”兼容分支。
    #[test]
    fn merged_classification_decision_table() {
        let peers = ["peer-token-1".to_string()];
        let has = |t: &str| peers.iter().any(|p| p == t);

        let no_panel = |_: &str| None;

        // #1 管理令牌命中 → 管理。
        assert_eq!(
            classify_token("mgmt-tok", "mgmt-tok", has, no_panel),
            Some(ConnRole::Management)
        );
        // #2 peer 令牌命中 → 联邦（管理令牌空/非空皆然）。
        assert_eq!(
            classify_token("peer-token-1", "mgmt-tok", has, no_panel),
            Some(ConnRole::Federation)
        );
        assert_eq!(
            classify_token("peer-token-1", "", has, no_panel),
            Some(ConnRole::Federation)
        );
        // #3 未知令牌 + 管理令牌非空 → 401。
        assert_eq!(classify_token("unknown", "mgmt-tok", has, no_panel), None);
        // #4 未知令牌 + 管理令牌为空 → 管理（兼容旧监听行为）。
        assert_eq!(
            classify_token("unknown", "", has, no_panel),
            Some(ConnRole::Management)
        );
        // #5 无令牌 + 管理令牌为空 → 管理。
        assert_eq!(
            classify_token("", "", has, no_panel),
            Some(ConnRole::Management)
        );
        // #6 无令牌 + 管理令牌非空 → 401。
        assert_eq!(classify_token("", "mgmt-tok", has, no_panel), None);
    }

    /// Panel 身份配置校验（P1）：四种错配都必须**拒绝启动**，正确配置放行。
    ///
    /// 为什么全是 fail-closed：这些组合的运行时表现都是"看起来能用"——
    /// 只配级别不配密钥 → 全 401（以为开了）；只配密钥不配级别 → 全 401；
    /// 与其它令牌撞车 → 行为取决于分类顺序（同一把密钥两种角色）。
    /// 启动时直接拒绝，运维当场知道改哪里。
    #[test]
    fn panel_identity_validation_is_fail_closed() {
        use echo_federation::PanelAccess;
        let no_peers = |_: &str| false;
        let key = |k: &str| vec![k.to_string()];

        // ① 级别开了但没密钥 → 拒绝。
        let err = validate_panel_identity(&[], PanelAccess::Readonly, "mgmt", no_peers)
            .expect_err("open level without keys must fail");
        assert!(err.to_string().contains("panel_keys"), "{err}");

        // ② 有密钥但级别 off → 拒绝。
        let err = validate_panel_identity(&key("k1"), PanelAccess::Off, "mgmt", no_peers)
            .expect_err("keys without open level must fail");
        assert!(err.to_string().contains("panel_access"), "{err}");

        // ③ 与管理令牌相同 → 拒绝。
        let err = validate_panel_identity(&key("mgmt"), PanelAccess::Admin, "mgmt", no_peers)
            .expect_err("panel key must differ from management token");
        assert!(err.to_string().contains("management_access_token"), "{err}");

        // ④ 与某 peer 令牌相同 → 拒绝。
        let err = validate_panel_identity(&key("peer-tok"), PanelAccess::Admin, "mgmt", |t| {
            t == "peer-tok"
        })
        .expect_err("panel key must differ from peer tokens");
        assert!(err.to_string().contains("[federation.peers.*]"), "{err}");

        // ⑤ 正确配置（含多条轮换密钥）放行；空白项被忽略。
        validate_panel_identity(
            &["k1".to_string(), " ".to_string(), "k2".to_string()],
            PanelAccess::Operate,
            "mgmt",
            |t| t == "peer-tok",
        )
        .expect("valid panel identity must pass");

        // ⑥ 完全未配置（默认状态）放行。
        validate_panel_identity(&[], PanelAccess::Off, "mgmt", no_peers)
            .expect("default (off + no keys) must pass");
    }

    /// Panel 身份分类（P1）：命中即带级别，且**空凭据永不命中**。
    ///
    /// 安全契约：空串若被当成"万能 Panel 密钥"，任何不带 Authorization 头
    /// 的连接都会拿到 Panel 权限——这一条测试把它钉死。
    #[test]
    fn panel_identity_classification_carries_level() {
        use echo_federation::PanelAccess;
        let peers = ["peer-token-1".to_string()];
        let has = |t: &str| peers.iter().any(|p| p == t);
        let panel = |t: &str| (t == "panel-key-1").then_some(PanelAccess::Readonly);
        // Panel 密钥命中 → 带级别（走管理 WS，权限受限）。
        assert_eq!(
            classify_token("panel-key-1", "mgmt-tok", has, panel),
            Some(ConnRole::Panel(PanelAccess::Readonly))
        );
        // Panel 密钥不得凌驾于 peer 令牌之上（分类顺序：管理 → 联邦 → Panel）。
        let both = |t: &str| {
            if t == "peer-token-1" {
                Some(PanelAccess::Admin)
            } else {
                None
            }
        };
        assert_eq!(
            classify_token("peer-token-1", "mgmt-tok", has, both),
            Some(ConnRole::Federation)
        );
        // 未知密钥 → 401（管理令牌非空时）。
        assert_eq!(classify_token("nope", "mgmt-tok", has, panel), None);
        // 空凭据恒不命中 Panel（即使实现在别处被写成"空=放行"）。
        let empty_hits = |t: &str| (t.is_empty()).then_some(PanelAccess::Admin);
        assert_eq!(
            classify_token("", "mgmt-tok", has, empty_hits),
            None,
            "空凭据绝不能拿到 Panel 身份"
        );
    }

    /// 只读 Panel 身份的服务端门（P1）：观察放行、写拒绝、`Off` 全拒。
    ///
    /// 这条是**权限边界**的单元测试；端到端的令牌分流见
    /// `merged_port_routes_by_token_namespace`。
    #[test]
    fn readonly_panel_gate_blocks_writes() {
        use echo_federation::PanelAccess;
        let observe = echo_protocol::BackendCommand::RequestState;
        let write = echo_protocol::BackendCommand::SendMessage {
            session_id: "s1".into(),
            content: "hi".into(),
            images: vec![],
            team_id: None,
        };
        let secret = echo_protocol::BackendCommand::RequestFederationInvite;
        // 只读：观察类放行，写/密钥类拒绝。
        assert_eq!(readonly_rejection(PanelAccess::Readonly, &observe), None);
        assert!(readonly_rejection(PanelAccess::Readonly, &write)
            .unwrap_or_default()
            .contains("SendMessage"));
        assert!(readonly_rejection(PanelAccess::Readonly, &secret).is_some());
        // 操作级及以上：不设门。
        for level in [PanelAccess::Operate, PanelAccess::Admin] {
            assert_eq!(readonly_rejection(level, &write), None);
            assert_eq!(readonly_rejection(level, &secret), None);
        }
        // Off：兜底全拒（握手期本应已 401）。
        assert!(readonly_rejection(PanelAccess::Off, &observe).is_some());
    }

    /// 联邦监听规划（「缺省合并」语义）全分支：
    #[test]
    fn federation_listen_plan_default_merge() {
        let no_peers = |_: &str| false;
        // ① 缺省 + 非回环 + 令牌 → 合并（effective = 管理地址）。
        assert_eq!(
            plan_federation_listen("0.0.0.0:3132", "tok", "0.0.0.0:3133", false, no_peers),
            (true, Some("0.0.0.0:3132".to_string()), None)
        );
        // ② 缺省 + 非回环 + 空令牌 → 回退独立（保守），带原因。
        let (merged, listen, reason) =
            plan_federation_listen("0.0.0.0:3132", "", "0.0.0.0:3133", false, no_peers);
        assert!(!merged);
        assert_eq!(listen.as_deref(), Some("0.0.0.0:3133"));
        assert!(reason.is_some());
        // ③ 缺省 + 回环管理地址 → 独立监听（不合并、非回退）。
        assert_eq!(
            plan_federation_listen("127.0.0.1:3132", "tok", "0.0.0.0:3133", false, no_peers),
            (false, Some("0.0.0.0:3133".to_string()), None)
        );
        // ③b 缺省 + 令牌与某 peer 令牌相同 → 回退独立（保守），带原因。
        let (merged, listen, reason) =
            plan_federation_listen("0.0.0.0:3132", "same-tok", "0.0.0.0:3133", false, |t| {
                t == "same-tok"
            });
        assert!(!merged);
        assert_eq!(listen.as_deref(), Some("0.0.0.0:3133"));
        assert!(reason.is_some());
        // ④ 显式同址 → 合并（空令牌/冲突交给 fail-closed 校验把关）。
        assert_eq!(
            plan_federation_listen("0.0.0.0:3132", "", "0.0.0.0:3132", true, no_peers),
            (true, Some("0.0.0.0:3132".to_string()), None)
        );
        // ⑤ 显式独立 → 按显式值。
        assert_eq!(
            plan_federation_listen("0.0.0.0:3132", "tok", "0.0.0.0:3199", true, no_peers),
            (false, Some("0.0.0.0:3199".to_string()), None)
        );
        // ⑥ 显式空串 → 纯连出。
        assert_eq!(
            plan_federation_listen("0.0.0.0:3132", "tok", "", true, no_peers),
            (false, None, None)
        );
        // ⑦ 缺省 + 管理地址为空 → 独立缺省。
        assert_eq!(
            plan_federation_listen("", "tok", "0.0.0.0:3133", false, no_peers),
            (false, Some("0.0.0.0:3133".to_string()), None)
        );
    }

    /// 合并启动校验（安全条件 fail-closed）。
    #[test]
    fn merged_validation_fail_closed() {
        // 非回环 + 空令牌 → 拒绝，信息含补救命令。
        let err = validate_merged("0.0.0.0:3132", "", |_| false).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("端口合并配置非法"), "unexpected: {msg}");
        assert!(
            msg.contains("token --rotate"),
            "error must hint remedy: {msg}"
        );

        // 回环 + 空令牌 → 放行（同机信任模型与现状一致）。
        assert!(validate_merged("127.0.0.1:3132", "", |_| false).is_ok());

        // 管理令牌 == 某 peer 令牌 → 拒绝（路由歧义）。
        let err = validate_merged("0.0.0.0:3132", "same-tok", |t| t == "same-tok").unwrap_err();
        assert!(
            format!("{err}").contains("无法区分"),
            "conflict error should explain ambiguity"
        );

        // 有令牌且无冲突 → 放行。
        assert!(validate_merged("0.0.0.0:3132", "mgmt-tok", |t| t == "peer-tok").is_ok());
    }

    /// 合并监听端端到端（真 WS）：同一端口——
    /// ① 管理令牌 → 管理通道（命令送达后端）；
    /// ② peer 令牌 → 联邦链路（Hello → Welcome）；
    /// ③ 未知令牌 → 标准 HTTP 401。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn merged_port_routes_by_token_namespace() {
        let (bridge, handle) = echo_protocol::create_bridge();
        let bridge = Arc::new(bridge);

        // 假“核心”的联邦实例：含一个已知 peer（token = "peer-tok"）。
        let (fed_events, _fed_rx) = tokio::sync::mpsc::channel(16);
        let (fed_shutdown, _fed_sd) = tokio::sync::watch::channel(false);
        let fed = echo_federation::Federation::new(
            "node-merged-host".into(),
            None,
            echo_federation::NodeCaps {
                tools: vec![],
                subagent: false,
                workspaces: vec![],
                active_turns: 0,
                panel_ready: false,
            },
            vec![echo_federation::PeerConfig {
                name: "remote".into(),
                url: String::new(),
                token: "peer-tok".into(),
            }],
            None,
            fed_events,
            fed_shutdown,
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let fed_for_server = fed.clone();
        tokio::spawn(async move {
            let _ = serve_with_listener_and_token(
                listener,
                bridge,
                "mgmt-tok".into(),
                Some(fed_for_server),
                None,
            )
            .await;
        });

        let client_request = |token: Option<&str>| {
            let mut request =
                tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                    format!("ws://{addr}").as_str(),
                )
                .unwrap();
            if let Some(token) = token {
                request
                    .headers_mut()
                    .insert("authorization", format!("Bearer {token}").parse().unwrap());
            }
            request
        };
        // 同一个请求，但把 `Host` 换成"客户端眼中本机的地址"（公网/NAT 视角）。
        let client_request_from = |token: Option<&str>, host: &str| {
            let mut request = client_request(token);
            request
                .headers_mut()
                .insert("host", host.parse().unwrap());
            request
        };

        // ⓪ 观测地址（2026-11）：合并端握手回调必须把客户端用的 `Host` 记进
        // 联邦观测表——邀请串据此宣告「别人实际连我用的地址」。
        assert_eq!(fed.observed_host_sync(), None);
        let (mut observed_ws, _) =
            tokio_tungstenite::connect_async(client_request_from(Some("mgmt-tok"), "114.132.241.46:3132"))
                .await
                .expect("host-carrying upgrade");
        assert_eq!(
            fed.observed_host_sync().as_deref(),
            Some("114.132.241.46"),
            "merged-port handshake must record the client's Host"
        );
        observed_ws.close(None).await.ok();

        // ① 管理令牌 → 管理通道：命令送达后端。
        let (mut mgmt_ws, _) = tokio_tungstenite::connect_async(client_request(Some("mgmt-tok")))
            .await
            .expect("management token must upgrade");
        let text =
            echo_protocol::serialize_command(&BackendCommand::SwitchModel { model: "m".into() });
        mgmt_ws
            .send(tokio_tungstenite::tungstenite::Message::Text(text))
            .await
            .unwrap();
        let received = handle
            .command_rx
            .lock()
            .await
            .recv()
            .await
            .expect("command delivered through merged port");
        assert!(matches!(received, BackendCommand::SwitchModel { .. }));
        mgmt_ws.close(None).await.ok();

        // ② peer 令牌 → 联邦链路：Hello → Welcome（真握手）。
        let (mut fed_ws, _) = tokio_tungstenite::connect_async(client_request(Some("peer-tok")))
            .await
            .expect("peer token must upgrade");
        let hello = echo_federation::FedFrame::Hello(echo_federation::NodeHello {
            node_id: "node-remote".into(),
            node_name: None,
            protocol_version: echo_federation::PROTOCOL_VERSION,
            version: "test".into(),
            caps: echo_federation::NodeCaps {
                tools: vec![],
                subagent: false,
                workspaces: vec![],
                active_turns: 0,
                panel_ready: false,
            },
            advertise: None,
        });
        fed_ws
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&hello).unwrap(),
            ))
            .await
            .unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), fed_ws.next())
            .await
            .expect("welcome timeout")
            .expect("welcome frame")
            .expect("welcome ok");
        let frame: echo_federation::FedFrame =
            serde_json::from_str(&reply.into_text().unwrap()).unwrap();
        assert!(
            matches!(frame, echo_federation::FedFrame::Welcome(_)),
            "expected Welcome on merged port, got {frame:?}"
        );
        fed_ws.close(None).await.ok();

        // ③ 未知令牌 → 标准 401（非静默断开）。
        let error = tokio_tungstenite::connect_async(client_request(Some("wrong-token")))
            .await
            .expect_err("unknown token must fail");
        assert!(
            matches!(
                error,
                tokio_tungstenite::tungstenite::Error::Http(ref resp)
                    if resp.status() == tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED
            ),
            "unknown token on merged port must be HTTP 401, got {error:?}"
        );
    }

    /// **端到端**：Panel 身份在合并端口上的完整语义（P1，2026-11）。
    ///
    /// 一条测试钉住四件事（缺一条就等于边界漏了）：
    /// ① Panel 密钥能升级（不是 401）——否则"一个密钥接全网"无从谈起；
    /// ② 只读级别下**观察命令放行**、**写命令被拒**（且回一条可读 `Error`
    ///    而不是静默丢弃——客户端必须知道是被降级了）；
    /// ③ 观察命令确实到达后端（证明门没把只读身份整个关掉）；
    /// ④ 未配置 Panel 身份时，同一把密钥 = 401（不因"没配"而放行）。
    #[tokio::test]
    async fn merged_port_honours_panel_identity_level() {
        use echo_federation::{PanelAccess, PanelIdentity};
        let (bridge, handle) = echo_protocol::create_bridge();
        let bridge = Arc::new(bridge);

        let (fed_events, _fed_rx) = tokio::sync::mpsc::channel(16);
        let (fed_shutdown, _fed_sd) = tokio::sync::watch::channel(false);
        let fed = echo_federation::Federation::new(
            "node-panel-host".into(),
            None,
            echo_federation::NodeCaps {
                tools: vec![],
                subagent: false,
                workspaces: vec![],
                active_turns: 0,
                panel_ready: true,
            },
            vec![],
            None,
            fed_events,
            fed_shutdown,
        );
        // 只读级别的 Panel 身份。
        fed.set_panel_identity(PanelIdentity::new(
            vec!["panel-key".into()],
            PanelAccess::Readonly,
        ));
        assert!(fed.panel_ready_sync());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let fed_for_server = fed.clone();
        tokio::spawn(async move {
            let _ = serve_with_listener_and_token(
                listener,
                bridge,
                "mgmt-tok".into(),
                Some(fed_for_server),
                None,
            )
            .await;
        });

        let client_request = |token: Option<&str>| {
            let mut request =
                tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                    format!("ws://{addr}").as_str(),
                )
                .unwrap();
            if let Some(token) = token {
                request
                    .headers_mut()
                    .insert("authorization", format!("Bearer {token}").parse().unwrap());
            }
            request
        };

        // ① Panel 密钥能升级。
        let (mut panel_ws, _) = tokio_tungstenite::connect_async(client_request(Some("panel-key")))
            .await
            .expect("panel key must upgrade on merged port");

        // ② 写命令被拒：后端**收不到**，并且本连接收到一条 Error 说明。
        let write =
            echo_protocol::serialize_command(&BackendCommand::SwitchModel { model: "m".into() });
        panel_ws
            .send(tokio_tungstenite::tungstenite::Message::Text(write))
            .await
            .unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), panel_ws.next())
            .await
            .expect("rejection timeout")
            .expect("rejection frame")
            .expect("rejection ok");
        let frame: echo_protocol::WsMessage =
            serde_json::from_str(&reply.into_text().unwrap()).unwrap();
        match frame {
            echo_protocol::WsMessage::Event(event) => match event {
                echo_protocol::BackendEvent::Error { message, .. } => {
                    assert!(
                        message.contains("SwitchModel"),
                        "rejection must name the command: {message}"
                    );
                }
                other => panic!("expected Error event, got {other:?}"),
            },
            other => panic!("expected event frame, got {other:?}"),
        }
        assert!(
            handle.command_rx.lock().await.try_recv().is_err(),
            "readonly panel write must NOT reach the backend"
        );

        // ③ 观察命令放行：后端收到（只读身份看得到东西）。
        let observe = echo_protocol::serialize_command(&BackendCommand::RequestState);
        panel_ws
            .send(tokio_tungstenite::tungstenite::Message::Text(observe))
            .await
            .unwrap();
        let delivered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            handle.command_rx.lock().await.recv().await
        })
        .await
        .expect("readonly observe timeout")
        .expect("readonly observe delivered");
        assert!(matches!(delivered, BackendCommand::RequestState));
        panel_ws.close(None).await.ok();

        // ④ 关掉 Panel 身份后，同一把密钥 401。
        fed.set_panel_identity(PanelIdentity::default());
        let error = tokio_tungstenite::connect_async(client_request(Some("panel-key")))
            .await
            .expect_err("revoked panel key must be rejected");
        assert!(
            matches!(
                error,
                tokio_tungstenite::tungstenite::Error::Http(ref resp)
                    if resp.status() == tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED
            ),
            "revoked panel key must be HTTP 401, got {error:?}"
        );
    }
}

/// 构造插件：管理面监听 + 会话视图命令域。
///
/// **零参数**（全插件化 S6）：监听地址缺省值 / 令牌 / Panel 桥 / 合并模式的
/// 联邦句柄经 [`host_keys::MANAGEMENT_HOST`] 解析，会话视图的数据面经
/// [`host_keys::SESSION_VIEW_HOST`] 解析。
///
/// 唯一仍走 `MountContext::config` 的是**地址覆盖**——`plugins.toml` 的
/// `[plugin.config] address`。它刻意不经过 trait：配置是"这一份部署怎么调"，
/// 而 trait 表达"宿主能提供什么"。令牌**不接受**插件配置覆盖（插件配置会进
/// 插件清单接口与日志排查面，凭证放那里等于主动泄漏）。
///
/// **自锁语义**：禁用本插件 = 关闭 Panel 通道本身，恢复需编辑 `core.toml` 的
/// `disabled_plugins` 后重启（`TogglePlugin` 对它有专门拒绝）。
pub fn plugin(version: &str) -> std::sync::Arc<dyn echo_plugin::Plugin> {
    use echo_plugin::{BuiltinPlugin, Inject, PluginManifest, ServiceSlot};

    let management = ServiceSlot::<std::sync::Arc<dyn host_keys::ManagementHost>>::new();
    let view = ServiceSlot::<std::sync::Arc<dyn session_view::SessionViewHost>>::new();
    let (bind_mgmt, bind_view) = (management.clone(), view.clone());
    let (serve_view, bind_serve_mgmt) = (view.clone(), management.clone());

    let manifest = PluginManifest::builtin(
        PLUGIN_ID,
        "管理面",
        version,
        "management",
        "panel",
        "Panel management WS 桥接（合并模式下同时承接联邦入站）+ 会话视图命令域",
    );
    std::sync::Arc::new(
        BuiltinPlugin::new(manifest, move |mctx| {
            let keep_mgmt = bind_mgmt.bind(mctx, &host_keys::MANAGEMENT_HOST)?;
            let keep_view = bind_view.bind(mctx, &host_keys::SESSION_VIEW_HOST)?;
            let host = bind_serve_mgmt
                .get()
                .ok_or_else(|| echo_plugin::PluginMountError::Mount("management host 未绑定".into()))?;

            // `plugins.toml` 覆盖监听地址（唯一走这条路的一项）。
            let default_addr = host.default_address();
            let addr = mctx
                .config_str("address")
                .map(str::to_string)
                .unwrap_or_else(|| default_addr.clone());
            if addr != default_addr {
                tracing::info!(
                    configured = %default_addr,
                    effective = %addr,
                    "management address overridden by plugins.toml"
                );
            }

            let bridge = host.bridge();
            let token = host.token();
            let federation = host.federation();
            // 观测出处：独立监听时联邦不经过本端口，但**入站 Host 仍要从这里
            // 学**（Panel 怎么连我，邀请串就宣告哪个地址）。
            let observer = host.federation_observer();
            let merged = federation.is_some();
            let server = tokio::spawn(async move {
                let result = match federation {
                    Some(federation) => {
                        serve_with_federation(&addr, bridge, token, federation).await
                    }
                    None => serve_with_token_observing(&addr, bridge, token, observer).await,
                };
                if let Err(error) = result {
                    // 管理面监听退出 = Panel 通道失效（合并模式下联邦入站一并
                    // 失效）；升级为 error 并给出排查线索。
                    tracing::error!(
                        error = %error,
                        "management WS server stopped — Panel channel{} down; \
                         check the bind address is free (ss -tlnp)",
                        if merged { " and federation inbound" } else { "" }
                    );
                }
            });
            let abort = server.abort_handle();
            Ok(vec![
                keep_mgmt,
                keep_view,
                echo_context::Disposer::from_fn(move || abort.abort()),
            ])
        })
        .with_inject(Inject::new([
            host_keys::MANAGEMENT_HOST.name().to_string(),
            host_keys::SESSION_VIEW_HOST.name().to_string(),
        ]))
        .with_commands(session_view::COMMANDS)
        .with_command_handler_async(move |name, payload| {
            let serve_view = serve_view.clone();
            Box::pin(async move {
                let Some(view) = serve_view.get() else {
                    return Err(format!(
                        "命令 {name} 到 {PLUGIN_ID} 时插件尚未挂载（会话视图接缝未绑定）"
                    ));
                };
                session_view::SessionViewHandler::new(view)
                    .handle(&name, payload)
                    .await
                    .unwrap_or_else(|| {
                        Err(format!(
                            "命令 {name} 被 {PLUGIN_ID} 声明，但 session-view 处理器没有认领它"
                        ))
                    })
            })
        }),
    )
}
