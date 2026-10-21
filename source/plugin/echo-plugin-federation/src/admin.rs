//! 联邦命令域：**规则在这里，状态迁移在宿主**（2026-11 S11）。
//!
//! ## 为什么这条线要划在这里
//!
//! `SaveFederationPeer` / 邀请串 / `MigrateSession` 此前在组合根
//! （`core/federation_rt.rs::handle_federation_command`，约 300 行）。搬进
//! 插件时最重要的一步不是"移动代码"，而是**找出哪些是规则、哪些是状态**：
//!
//! | 内容 | 归谁 | 判据 |
//! |---|---|---|
//! | "token 不能为空" / "token 不能与 `management_access_token` 相同" / "同一 token 只能属于一个 peer" / "URL 必须是 `ws://host:port`" | **插件**（本模块，纯函数） | 它们是**规则**——把 peer 表换成数据库也照样成立 |
//! | "邀请串 = listen 地址 + 新 token" / "未配对占位 1 小时后清扫" | **插件** | 同上：邀请语义属于联邦，不属于 Core |
//! | `Federation::add_peer` / 配置写回 / `drop_link` / 反查表清理 / `router.drop_peer` | **宿主**（trait 方法） | 它们是**本进程的状态迁移**，需要 `Federation` 句柄与 `ConfigStore` |
//!
//! 规则是纯函数，所以可以直接单测（`mod tests` 里有 9 条），不需要起任何
//! 进程或链接。状态迁移经 [`FederationAdminHost`] 由组合根实现。
//!
//! ## 和 `admin.rs`（供应商池）同一个形状
//!
//! `echo-plugin-provider-llm::admin` 也是"规则进插件、原子动作留宿主"
//! （`PoolState` 进 → `PoolEffect` 出）。这不是巧合：**凡是"用户点了面板
//! 一个按钮"的命令，都长这个样子**——判定要不要做、按什么顺序做、失败文案
//! 是什么，属于领域；真的改内存与磁盘，属于拥有那些内存与磁盘的一方。

use std::sync::Arc;

use echo_protocol::{BackendCommand, BackendEvent};

/// 事件出口（命令的副作用一律经它发出）。
///
/// 与既有命令域同一个签名：进程级汇聚点由组合根在装配期给出（去主智能体
/// 之后"发给哪个 agent"不再是个问题）。
pub type Emit = Arc<dyn Fn(BackendEvent) + Send + Sync>;

/// 一条**已校验**的 peer 保存意图（宿主据此做状态迁移，不再判规则）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSave {
    pub name: String,
    pub url: String,
    pub token: String,
    pub allow_tools: Vec<String>,
    pub require_confirm: Vec<String>,
    pub allow_queries: Vec<String>,
    pub allow_subagent: bool,
}

/// peer 摘要（规则判定用：token 继承与冲突检测）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSummary {
    pub name: String,
    pub token: String,
}

/// 联邦管理面的**状态迁移**出口（由组合根实现，插件只声明需要什么）。
///
/// 每个方法都是**原子动作**——没有"校验 + 动作"的混合，校验已经在本模块的
/// 纯函数里做完了。这条纪律让宿主实现可以短到一眼看完。
#[async_trait::async_trait]
pub trait FederationAdminHost: Send + Sync {
    /// 现有 peer 表（配置名 + token）。
    async fn peers(&self) -> Vec<PeerSummary>;

    /// 端口合并模式下的管理面令牌（`None` = 独立模式，无此校验）。
    fn merged_management_token(&self) -> Option<String>;

    /// 本机联邦监听地址（`""` = 不监听 → 邀请不可用）。
    fn listen(&self) -> String;

    /// `[federation] advertise`（`""` = 从 listen 推导）。
    fn advertise(&self) -> String;

    /// 入站连接观测到的「对端/面板实际访问本机用的主机名」（`Host` 头；
    /// 公网优先、同级取最近；`None` = 还没观测到）。
    ///
    /// 为什么需要它（2026-10 云机配对事故）：邀请串此前只从 `advertise` /
    /// `listen` 推导，而手工 `advertise` 极易写成只有本机视角成立的地址
    /// （腾讯云 VM 的 `10.1.0.8`、本机内网 `10.10.10.100`），对端照着拨
    /// 必然超时。真正的答案就在对端自己的请求头里，所以邀请优先用它。
    fn observed_host(&self) -> Option<String>;

    /// 人类可读节点名（邀请串里带它，便于对端识别）。
    fn node_name(&self) -> Option<String>;

    /// 当前联邦状态事件（`FederationStatus`）。
    async fn status_event(&self) -> BackendEvent;

    /// **联邦节点目录**事件（`FederationDirectory`，P2 自动发现）：本机身份 +
    /// 直接邻居清单（含离线条目——Panel 仍可用历史宣告地址试连）。
    ///
    /// 规则（"报谁、报什么"）属于联邦领域，但数据只能由宿主取（链路表、
    /// 诊断表、`peer_names` 映射都在 `FederationRuntime` 里），所以这里是
    /// 一个原子动作而不是纯函数。
    async fn directory_event(&self) -> BackendEvent;

    /// 落一条 peer：运行时表 + 配置原子写回 + 策略同步 + 刷新路由。
    async fn apply_peer(&self, peer: PeerSave);

    /// 删一条 peer：断链 + 在飞调用终结 + 反查表清理 + 配置移除。
    async fn remove_peer(&self, name: &str);

    /// 注册邀请占位（一次性邀请：只进运行时表，**不写配置**）。
    ///
    /// 顺带清扫超过 1 小时未配对的旧占位——这是规则，但清扫需要访问登记表，
    /// 所以由宿主执行；返回被清掉的占位名（用于日志）。
    async fn register_invite(&self, placeholder: &str, token: &str) -> Vec<String>;

    /// 跨机会话迁移（实现细节在 `federation_import`，属宿主编排）。
    async fn migrate_session(
        &self,
        session_id: String,
        team_id: Option<String>,
        target_peer: String,
        emit: Emit,
    );
}

// ── 规则（纯函数，可单测）──────────────────────────────────────────────

/// peer 名不能为空（空名会让配置写出一个无名表项）。
pub fn validate_peer_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("peer 名称不能为空".into());
    }
    Ok(name.to_string())
}

/// 决定这条 peer 用哪个 token。
///
/// 三种情形：① 显式给了新 token → 用它；② 没给但 peer 已存在 → **继承旧值**
/// （面板改白名单不该顺手清掉密钥）；③ 都没有 → 报错。
///
/// 空 token 必须拒绝：accept 侧 `!known.is_empty()` 会永远拒绝、dial 侧不发
/// 认证头——表现是"保存成功但连不上，且日志里什么也没有"。
pub fn resolve_token(
    provided: &str,
    existing: Option<&PeerSummary>,
) -> Result<String, String> {
    let provided = provided.trim();
    if !provided.is_empty() {
        return Ok(provided.to_string());
    }
    match existing {
        Some(peer) if !peer.token.is_empty() => Ok(peer.token.clone()),
        _ => Err("联邦 peer token 不能为空（新建时必须提供共享密钥）".into()),
    }
}

/// token 不得与管理面令牌相同（端口合并模式下无法区分两类连接）。
pub fn validate_token_not_management(
    token: &str,
    merged_management_token: Option<&str>,
) -> Result<(), String> {
    if let Some(mgmt) = merged_management_token {
        if !mgmt.is_empty() && token == mgmt {
            return Err(
                "该 token 与 management_access_token 相同——端口合并下无法区分管理/联邦连接，\
                 请使用不同令牌（改 peer token 或 `echo-agent-core token --rotate`）"
                    .into(),
            );
        }
    }
    Ok(())
}

/// 同一 token 只能属于一个 peer。
///
/// 在线判定（`is_link_token_online`）与连入侧名字解析都按 **token 匹配**：
/// 两个 peer 共用 token 会让二者歧义——在线状态互相串、连入解析到错误的
/// 配置名（2026-10 审计 P2-④）。
pub fn validate_token_unique<'a>(
    name: &str,
    token: &str,
    peers: impl IntoIterator<Item = &'a PeerSummary>,
) -> Result<(), String> {
    for peer in peers {
        if peer.name != name && !peer.token.is_empty() && peer.token == token {
            return Err(format!(
                "该 token 已被 peer「{}」使用——同一 token 只能属于一个 peer\
                 （在线判定与连入名解析按 token 匹配）",
                peer.name
            ));
        }
    }
    Ok(())
}

/// peer URL 形状校验。空 = 仅接受连入（合法）；否则必须 `ws://host:port`。
///
/// 坏地址保存成功但 dial 反复失败（此前只有 debug 日志），面板看不出配置
/// 错误——所以保存时就拒绝（2026-10 审计 P1-4）。
pub fn validate_peer_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(());
    }
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
        .ok_or_else(|| {
            format!("联邦 peer 地址必须以 ws:// 或 wss:// 开头（空 = 仅接受连入）：{url}")
        })?;
    let hostport = rest.split('/').next().unwrap_or("");
    let (host, port) = hostport
        .rsplit_once(':')
        .ok_or_else(|| format!("联邦 peer 地址缺少端口（应为 ws://host:port）：{url}"))?;
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(format!("联邦 peer 地址的主机或端口无效：{url}"));
    }
    Ok(())
}

/// 邀请串对外地址：观测到的访问主机优先，其次显式 `advertise`，最后从
/// `listen` 推导。
///
/// `None` = 不监听或推导不出（此时邀请无意义，必须让人去配
/// `[federation] listen` / `advertise`）。
pub fn invite_host(listen: &str, advertise: &str, observed: Option<&str>) -> Option<String> {
    let listen = listen.trim();
    if listen.is_empty() {
        return None;
    }
    // ① 观测地址（2026-11）：端口**一律取联邦监听端口**——观测值里的端口
    // 属于管理面，独立监听时两者不同（合并时相同，故行为不变）。
    if let Some(observed) = observed.map(str::trim).filter(|o| !o.is_empty()) {
        if let Some(host) = echo_federation::invite::host_of(observed) {
            if echo_federation::invite::host_rank(host) > 0 {
                if let Some(host_port) = echo_federation::invite::replace_host(listen, host) {
                    return Some(host_port);
                }
            }
        }
    }
    // ② 显式 advertise（端口缺省时沿用 listen 端口）。
    let adv = advertise.trim();
    if !adv.is_empty() {
        let has_port = adv
            .rsplit_once(':')
            .map(|(_, p)| p.parse::<u16>().is_ok())
            .unwrap_or(false);
        if has_port {
            return Some(adv.to_string());
        }
        let port = listen
            .rsplit_once(':')
            .map(|(_, p)| p)
            .filter(|p| !p.is_empty())
            .unwrap_or("3133");
        return Some(format!("{adv}:{port}"));
    }
    // ③ 从 listen 推导。
    advertise_host(listen)
}

/// 邀请用的对外地址：`listen` 为具体 IP/主机名时直接用；`0.0.0.0` / `[::]`
/// 时退化为"首个非回环 IPv4"（无则 None，提示手工配置 advertise）。
fn advertise_host(listen: &str) -> Option<String> {
    let listen = listen.trim();
    let (host, port) = listen.rsplit_once(':')?;
    let port = if port.is_empty() { "3133" } else { port };
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if host.is_empty() || host == "0.0.0.0" || host == "::" {
        // 退化为首个非回环 IPv4（与 `local_ipv4` 的既有选择一致）。
        let ip = first_non_loopback_ipv4()?;
        return Some(format!("{ip}:{port}"));
    }
    if host.contains(':') {
        // IPv6 字面量：重新加回方括号。
        return Some(format!("[{host}]:{port}"));
    }
    Some(format!("{host}:{port}"))
}

/// 首个非回环 IPv4（无则 None）。
fn first_non_loopback_ipv4() -> Option<std::net::Ipv4Addr> {
    // 不引第三方 crate：连一个 UDP socket 到外部地址取本地 addr 是标准做法
    // （不实际发包），失败再枚举网卡由 `std` 之外的能力提供——此处只做第一
    // 步，取不到就让调用方看到 `None` 并提示手工配 advertise。
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

/// 一次性邀请的占位名（对端回连时会按 token 认领）。
pub fn invite_placeholder(token: &str) -> String {
    let prefix: String = token.chars().take(8).collect();
    format!("invite-{prefix}")
}

// ── 命令域实现 ─────────────────────────────────────────────────────────

/// 本插件处理的命令名。
pub const COMMANDS: [&str; 6] = [
    "SaveFederationPeer",
    "DeleteFederationPeer",
    "RequestFederationStatus",
    "RequestFederationDirectory",
    "RequestFederationInvite",
    "MigrateSession",
];

/// 命令域入口：认领本域的命令，规则在此判定，状态迁移交宿主。
///
/// 返回 `None` = 不是本域；`Some(Ok)` = 处理完成（副作用已经以事件形式发出，
/// 与既有命令域一致：面板的状态一律来自事件流，不是命令回执）。
pub async fn dispatch(
    host: &dyn FederationAdminHost,
    name: &str,
    payload: serde_json::Value,
    emit: &Emit,
) -> Option<Result<serde_json::Value, String>> {
    if !COMMANDS.contains(&name) {
        return None;
    }
    let tagged = serde_json::json!({ name: payload });
    let Ok(cmd) = serde_json::from_value::<BackendCommand>(tagged) else {
        return Some(Err(format!("federation 命令载荷解析失败（{name}）")));
    };
    Some(Ok(handle(host, cmd, emit).await))
}

/// 具体分发（每条命令一臂）。
async fn handle(
    host: &dyn FederationAdminHost,
    cmd: BackendCommand,
    emit: &Emit,
) -> serde_json::Value {
    match cmd {
        BackendCommand::RequestFederationStatus => {
            emit(host.status_event().await);
        }
        BackendCommand::RequestFederationDirectory => {
            emit(host.directory_event().await);
        }
        BackendCommand::MigrateSession {
            session_id,
            team_id,
            target_peer,
        } => {
            host.migrate_session(session_id, team_id, target_peer, emit.clone())
                .await;
        }
        BackendCommand::SaveFederationPeer { peer } => {
            let result = save_peer(host, &peer).await;
            match result {
                Ok(()) => emit(host.status_event().await),
                Err(message) => emit(BackendEvent::Error {
                    session_id: None,
                    message,
                }),
            }
        }
        BackendCommand::DeleteFederationPeer { name } => {
            host.remove_peer(&name).await;
            emit(host.status_event().await);
        }
        BackendCommand::RequestFederationInvite => {
            match create_invite(host).await {
                Ok(invite) => emit(BackendEvent::FederationInvite { invite }),
                Err(message) => emit(BackendEvent::Error {
                    session_id: None,
                    message,
                }),
            }
        }
        _ => {}
    }
    serde_json::Value::Null
}

/// 保存 peer：规则判定 → 交宿主落状态。
///
/// 判定的**顺序**有意义（每一条都对应一次真实故障）：名字 → token 继承 →
/// 管理面冲突 → URL 形状 → token 唯一。前两条决定"要写什么"，后三条决定
/// "能不能写"。
async fn save_peer(
    host: &dyn FederationAdminHost,
    peer: &echo_protocol::FederationPeerInfo,
) -> Result<(), String> {
    let name = validate_peer_name(&peer.name)?;
    let peers = host.peers().await;
    let existing = peers.iter().find(|p| p.name == name);
    let token = resolve_token(&peer.token, existing)?;
    validate_token_not_management(&token, host.merged_management_token().as_deref())?;
    validate_peer_url(&peer.url)?;
    validate_token_unique(&name, &token, peers.iter())?;

    host.apply_peer(PeerSave {
        name,
        url: peer.url.trim().to_string(),
        token,
        allow_tools: peer.allow_tools.clone(),
        require_confirm: peer.require_confirm.clone(),
        allow_queries: peer.allow_queries.clone(),
        allow_subagent: peer.allow_subagent,
    })
    .await;
    Ok(())
}

/// 生成一次性邀请串：新 token + 占位 peer（只进运行时，不写配置）。
async fn create_invite(host: &dyn FederationAdminHost) -> Result<String, String> {
    let observed = host.observed_host();
    let Some(host_port) = invite_host(&host.listen(), &host.advertise(), observed.as_deref())
    else {
        return Err(
            "联邦未监听（listen 为空）或无法推导对外地址；请在配置 [federation] 写 \
             listen / advertise（对端可达地址）"
                .into(),
        );
    };
    let token = echo_federation::generate_token();
    let invite = echo_federation::encode_invite(&host_port, &token, host.node_name().as_deref());
    let placeholder = invite_placeholder(&token);
    let stale = host.register_invite(&placeholder, &token).await;
    if !stale.is_empty() {
        tracing::info!(
            target: "federation",
            cleaned = ?stale,
            "清理超过 1 小时未配对的邀请占位"
        );
    }
    Ok(invite)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_name_must_not_be_blank() {
        assert_eq!(validate_peer_name("  a  ").as_deref(), Ok("a"));
        assert!(validate_peer_name("   ").is_err());
    }

    /// 空 token 必须拒绝——它是"保存成功但永远连不上"的经典成因。
    #[test]
    fn token_is_inherited_when_omitted_but_required_when_new() {
        let existing = PeerSummary {
            name: "a".into(),
            token: "keep-me".into(),
        };
        assert_eq!(
            resolve_token("", Some(&existing)).as_deref(),
            Ok("keep-me"),
            "改白名单不该顺手清掉密钥"
        );
        assert_eq!(resolve_token("new", Some(&existing)).as_deref(), Ok("new"));
        assert!(resolve_token("", None).is_err(), "新建必须给 token");
        assert!(
            resolve_token("", Some(&PeerSummary { name: "a".into(), token: String::new() }))
                .is_err(),
            "旧值也是空时不能当继承成功"
        );
    }

    #[test]
    fn token_must_differ_from_management_token() {
        assert!(validate_token_not_management("t", None).is_ok());
        assert!(validate_token_not_management("t", Some("")).is_ok(), "空管理令牌不参战");
        assert!(validate_token_not_management("t", Some("t")).is_err());
    }

    /// 同一 token 只能属于一个 peer（在线判定与连入解析都按 token 匹配）。
    #[test]
    fn token_must_be_unique_across_peers() {
        let peers = vec![
            PeerSummary { name: "a".into(), token: "t1".into() },
            PeerSummary { name: "b".into(), token: String::new() },
        ];
        assert!(validate_token_unique("c", "t2", peers.iter()).is_ok());
        let err = validate_token_unique("c", "t1", peers.iter()).expect_err("冲突必须拒绝");
        assert!(err.contains("已被 peer「a」使用"), "{err}");
        // 自己更新自己不算冲突。
        assert!(validate_token_unique("a", "t1", peers.iter()).is_ok());
    }

    #[test]
    fn peer_url_shape() {
        assert!(validate_peer_url("").is_ok(), "空 = 仅接受连入");
        assert!(validate_peer_url("ws://10.0.0.1:3133").is_ok());
        assert!(validate_peer_url("wss://host:443/path").is_ok());
        assert!(validate_peer_url("http://host:80").is_err(), "必须是 ws/wss");
        assert!(validate_peer_url("ws://host").is_err(), "缺端口");
        assert!(validate_peer_url("ws://:3133").is_err(), "缺主机");
        assert!(validate_peer_url("ws://host:99999").is_err(), "端口越界");
    }

    #[test]
    fn invite_host_prefers_explicit_advertise() {
        assert_eq!(
            invite_host("0.0.0.0:3133", "1.2.3.4:4000", None).as_deref(),
            Some("1.2.3.4:4000")
        );
        assert_eq!(
            invite_host("0.0.0.0:3133", "1.2.3.4", None).as_deref(),
            Some("1.2.3.4:3133"),
            "advertise 只给主机时用 listen 的端口"
        );
        assert_eq!(
            invite_host("", "1.2.3.4", None).as_deref(),
            None,
            "不监听则无邀请"
        );
        assert_eq!(
            invite_host("10.1.2.3:3133", "", None).as_deref(),
            Some("10.1.2.3:3133"),
            "listen 是具体地址时直接用它"
        );
    }

    /// 2026-11 云机配对事故的回归锚点：观测到的访问主机**优先于**手工
    /// advertise（后者常停在过期内网地址），且端口一律取联邦监听端口。
    #[test]
    fn invite_host_prefers_observed_host_over_advertise() {
        // 腾讯云实况：advertise 缺省推导出内网 10.1.0.8，Panel 从公网连入。
        assert_eq!(
            invite_host("0.0.0.0:3132", "10.1.0.8:3132", Some("114.132.241.46:3132")).as_deref(),
            Some("114.132.241.46:3132")
        );
        // 独立监听：观测值的端口属于管理面，邀请必须用联邦监听端口。
        assert_eq!(
            invite_host("0.0.0.0:3133", "10.10.10.100:3133", Some("10.10.10.100:3132")).as_deref(),
            Some("10.10.10.100:3133")
        );
        // 回环观测值对远端无意义 → 忽略它，回退 advertise。
        assert_eq!(
            invite_host("0.0.0.0:3133", "1.2.3.4:3133", Some("127.0.0.1:3132")).as_deref(),
            Some("1.2.3.4:3133")
        );
        // 未观测到（None）= 老行为，advertise 仍然生效。
        assert_eq!(
            invite_host("0.0.0.0:3133", "1.2.3.4:3133", None).as_deref(),
            Some("1.2.3.4:3133")
        );
    }
    #[test]
    fn placeholder_uses_token_prefix() {
        assert_eq!(invite_placeholder("abcdefghijkl"), "invite-abcdefgh");
    }
}
