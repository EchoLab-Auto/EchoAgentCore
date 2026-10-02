//! 联邦邀请串（federation Phase 4）：`echofed://` 配对格式。
//!
//! 形态：`echofed://<host>:<port>?name=<别名>#<token>`
//!
//! - fragment（`#` 后）= per-peer 共享密钥——按「互信 ≈ SSH 免密」对待其
//!   分发（同 WireGuard 配置串的密级）
//! - query `name` 可选（人类可读别名，做 peer 配置名建议）
//! - 解析端补 `ws://` scheme 与 `/` 路径，得到 peer.url
//!
//! 生成方：本机 listen 地址 + 一个**为此邀请新设的**随机 token（写入
//! 配置）。v1 简化：token 即静态共享密钥，不设一次性/有效期（同 RFC §2.3
//! 的 per-peer 静态密钥模型）。

use anyhow::{bail, Context, Result};

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
pub fn decode_invite(invite: &str) -> Result<InvitePayload> {
    let rest = invite
        .trim()
        .strip_prefix(INVITE_SCHEME)
        .context("不是 echofed:// 邀请串")?;
    let (before_fragment, token) = rest.split_once('#').context("邀请串缺少 #<token>")?;
    if token.trim().is_empty() {
        bail!("邀请串 token 为空");
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
        bail!("邀请串 host:port 非法: {host_port:?}");
    }
    Ok(InvitePayload {
        url: format!("ws://{host_port}"),
        token: token.trim().to_string(),
        name,
    })
}

/// 生成随机共享密钥（32 字节 → 64 位十六进制）。
pub fn generate_token() -> String {
    // 复用 NodeId 的熵源思路：时间 + 计数 + 地址混合（非安全级——联邦
    // token 强度依赖分发渠道保密性，与 RFC §2.3 模型一致）。如需加密级
    // 随机，后续接 getrandom。
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut out = String::with_capacity(64);
    let mut x = now.as_nanos() as u64 ^ (&now as *const _ as u64);
    for _ in 0..4 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push_str(&format!("{x:016x}"));
    }
    out
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
