//! 联邦邀请串（federation Phase 4）：`echofed://` 配对格式。
//!
//! 形态：`echofed://<host>:<port>?name=<别名>#<token>`
//!
//! - fragment（`#` 后）= per-peer 共享密钥——按「互信 ≈ SSH 免密」对待其
//!   分发（同 WireGuard 配置串的密级）
//! - query `name` 可选（人类可读别名，做 peer 配置名建议）
//! - 解析端补 `ws://` scheme 与 `/` 路径，得到 peer.url
//!
//! 生成方：本机 listen 地址 + 一个**为此邀请新设的**随机 token。token
//! 是**一次性**的：生成方仅将占位 peer 注册进运行时（**不写配置**），
//! 对端回连配对成功后自动清理；未配对的邀请在重启后自然失效，避免
//! 配置文件堆积垃圾占位条目。

use crate::error::FederationError;

/// 邀请串 scheme。
pub const INVITE_SCHEME: &str = "echofed://";

/// 从邀请串解析出的 peer 字段。
#[derive(Debug, Clone, PartialEq)]
pub struct InvitePayload {
    /// `ws://host:port`（补全 scheme 后）。
    pub url: String,
    /// 共享密钥。
    pub token: String,
    /// 可选别名（query `name`）。
    pub name: Option<String>,
}

/// 生成邀请串。
pub fn encode_invite(host_port: &str, token: &str, name: Option<&str>) -> String {
    let mut s = format!("{INVITE_SCHEME}{host_port}");
    if let Some(n) = name.filter(|n| !n.trim().is_empty()) {
        s.push_str(&format!("?name={}", urlencoding_encode(n.trim())));
    }
    s.push_str(&format!("#{token}"));
    s
}

/// 解析邀请串（严格：scheme 必匹配；token 非空；host:port 非空）。
pub fn decode_invite(invite: &str) -> Result<InvitePayload, FederationError> {
    let rest = invite
        .trim()
        .strip_prefix(INVITE_SCHEME)
        .ok_or_else(|| FederationError::Invite("不是 echofed:// 邀请串".into()))?;
    let (before_fragment, token) = rest
        .split_once('#')
        .ok_or_else(|| FederationError::Invite("邀请串缺少 #<token>".into()))?;
    if token.trim().is_empty() {
        return Err(FederationError::Invite("邀请串 token 为空".into()));
    }
    let (host_port, name) = match before_fragment.split_once('?') {
        Some((hp, query)) => {
            let name = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("name="))
                .map(urlencoding_decode)
                .filter(|n| !n.is_empty());
            (hp, name)
        }
        None => (before_fragment, None),
    };
    let host_port = host_port.trim().trim_end_matches('/');
    if host_port.is_empty() || !host_port.contains(':') {
        return Err(FederationError::Invite(format!(
            "邀请串 host:port 非法: {host_port:?}"
        )));
    }
    Ok(InvitePayload {
        url: format!("ws://{host_port}"),
        token: token.trim().to_string(),
        name,
    })
}

/// 生成随机共享密钥（32 字节 → 64 位十六进制）。
///
/// 熵源：`Uuid::new_v4()`（由 getrandom 提供加密级随机；两个拼接共
/// 244 bit）。**不使用**自制的非加密熵源（xorshift / 时间戳混合可预测）
/// ——配对（占位提升）即落全权限（`allow_tools` = `*`），token 是唯一
/// 的门禁凭据，必须按密钥强度生成。
pub fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

// ── 观测地址（2026-11）：邀请串优先用「别人连我时实际用的地址」 ────────────
//
// 背景（2026-10 云机配对事故）：邀请串此前只从 `[federation] advertise` /
// `listen` 推导。手工 advertise 极易写成一个**只有本机视角成立**的地址
// （腾讯云 VM 的 `10.1.0.8`、或本机内网 `10.10.10.100`），对端照着拨必然
// 超时；而真正的答案其实**已经在入站连接里了**——Panel 用
// `ws://114.132.241.46:3132` 连进来时，HTTP Upgrade 请求的 `Host` 头就是
// 那个地址（NAT 之后也一样，因为它是客户端填的，不由本机路由决定）。
//
// 所以：先从入站连接观测到的 Host 里选（公网优先、同类取最近），选不到
// 才回退 advertise，最后才回退 listen 推导。

/// 从 `host:port` 里取出主机部分（IPv6 去掉方括号）。
pub fn host_of(host_port: &str) -> Option<&str> {
    let text = host_port.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix('[') {
        let (host, _) = rest.split_once(']')?;
        return Some(host).filter(|h| !h.is_empty());
    }
    match text.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => Some(host).filter(|h| !h.is_empty()),
        // 没有端口（裸主机名）：整体即主机。
        _ => Some(text),
    }
}

/// 把 `host_port` 的主机部分替换为 `host`，**端口保持不变**。
pub fn replace_host(host_port: &str, host: &str) -> Option<String> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    let text = host_port.trim();
    let port = match text.rsplit_once(':') {
        Some((_, p)) if p.parse::<u16>().is_ok() => p,
        _ => return None,
    };
    if host.contains(':') {
        Some(format!("[{host}]:{port}"))
    } else {
        Some(format!("{host}:{port}"))
    }
}

/// 主机名等级：`2` = 公网（域名 / 全局单播）、`1` = 私网（局域网 / ULA /
/// 链路本地）、`0` = 不可用于对外邀请（空 / 回环 / 通配 / 广播）。
pub fn host_rank(host: &str) -> u8 {
    use std::net::IpAddr;
    // 入参可能是 `Host` 头原样（带端口）或裸主机名：先剥离端口，否则
    // `127.0.0.1:3132` 会被当成"域名"而误判为公网。
    let Some(host) = host_of(host) else {
        return 0;
    };
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if host.is_empty() {
        return 0;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => {
                if v4.is_loopback() || v4.is_unspecified() || v4.is_broadcast() {
                    0
                } else if v4.is_private() || v4.is_link_local() || is_cgnat(v4) {
                    1
                } else {
                    2
                }
            }
            IpAddr::V6(v6) => {
                if v6.is_loopback() || v6.is_unspecified() {
                    0
                } else if v6.is_unique_local() || v6.is_unicast_link_local() {
                    1
                } else {
                    2
                }
            }
        };
    }
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return 0;
    }
    // 域名：由 DNS / 端口映射决定可达性，按公网对待（如 `xxx.compshare.cn`）。
    2
}

/// 运营商级 NAT 段 `100.64.0.0/10`（`Ipv4Addr::is_private` 不含它，但云
/// 厂商的内网地址常见于此段，对公网对端同样不可达）。
fn is_cgnat(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (o[1] & 0xc0) == 0x40
}

/// 从观测池里选一个可用于邀请的主机：等级高者优先，**同级取最近**
/// （池按到达顺序追加，最近的在尾部）。
pub fn pick_observed_host(hosts: &[String]) -> Option<String> {
    let mut best: Option<(u8, &String)> = None;
    for host in hosts {
        let rank = host_rank(host);
        if rank == 0 {
            continue;
        }
        if best.is_none_or(|(best_rank, _)| rank >= best_rank) {
            best = Some((rank, host));
        }
    }
    // 返回**主机名**（不含端口）：调用方按自己的监听端口拼 host:port。
    best.and_then(|(_, host)| host_of(host).map(str::to_string))
}

/// 记录一次观测（去重并挪到尾部 = 最近优先；池上限 `cap`）。
///
/// 返回 `true` = 该主机确实进了池（调用方据此打一条握手日志）。
pub fn note_observed(hosts: &mut Vec<String>, host: &str, cap: usize) -> bool {
    let host = host.trim();
    if host.is_empty() || host_rank(host) == 0 {
        return false; // 回环/通配对远端毫无意义，不进池（否则会顶掉有效值）
    }
    hosts.retain(|known| !known.eq_ignore_ascii_case(host));
    hosts.push(host.to_string());
    if hosts.len() > cap {
        let excess = hosts.len() - cap;
        hosts.drain(0..excess);
    }
    true
}

fn urlencoding_encode(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf)
                    .bytes()
                    .map(|b| format!("%{b:02X}"))
                    .collect()
            }
        })
        .collect()
}

fn urlencoding_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_roundtrip() {
        let s = encode_invite("192.168.1.10:3133", "deadbeef", Some("gpu box"));
        assert_eq!(s, "echofed://192.168.1.10:3133?name=gpu%20box#deadbeef");
        let p = decode_invite(&s).unwrap();
        assert_eq!(p.url, "ws://192.168.1.10:3133");
        assert_eq!(p.token, "deadbeef");
        assert_eq!(p.name.as_deref(), Some("gpu box"));
    }

    #[test]
    fn invite_without_name() {
        let s = encode_invite("10.0.0.2:3133", "tok", None);
        assert_eq!(s, "echofed://10.0.0.2:3133#tok");
        let p = decode_invite(&s).unwrap();
        assert_eq!(p.name, None);
    }

    #[test]
    fn decode_rejects_malformed() {
        assert!(decode_invite("http://x:1#t").is_err());
        assert!(decode_invite("echofed://x:3133").is_err());
        assert!(decode_invite("echofed://x:3133#").is_err());
        assert!(decode_invite("echofed://#tok").is_err());
    }

    #[test]
    fn token_has_expected_shape() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(generate_token(), generate_token());
    }
}
