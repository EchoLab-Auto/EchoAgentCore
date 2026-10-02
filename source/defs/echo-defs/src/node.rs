//! 节点身份词汇（federation Phase 0）：`NodeId` 与全局引用的 `node://` 命名空间。
//!
//! 每台 Core 节点拥有一个持久化的 [`NodeId`]。跨机引用（会话、工作区目录）
//! 以 `node://<node_id>/<local-ref>` 形式书写；本机引用省略 scheme，与既有
//! 格式完全兼容。Phase 0 只定义词汇与解析，不改变任何运行时行为。

use std::fmt;

/// 节点唯一标识（federation Phase 0）。
///
/// 线格式：`node-<26 位 ULID>`（如 `node-01j4z8k0m2v1x3q5w7e9r8t6y5`）。
/// 由组合根首次启动时生成并持久化到 `echo-node.json`（原子写），此后稳定。
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    /// 从既有字符串包装（不校验格式——持久化/线格式兼容优先）。
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// 字符串视图。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 生成一个全新的随机 NodeId（`node-<ulid>`）。
    ///
    /// ULID 用时间戳 + 随机位手工构造（ Crockford Base32 ），不引入额外依赖。
    pub fn generate() -> Self {
        Self(format!("node-{}", ulid_now()))
    }

    /// 拆分一条可能带 `node://` 前缀的引用。
    ///
    /// - `"node://node-abc/local:tui::local_user"` → `(Some("node-abc"), "local:tui::local_user")`
    /// - `"local:tui::local_user"` → `(None, "local:tui::local_user")`（本机）
    /// - `"node://node-abc/"` / `"node://"` → `(Some(...), "")` 由调用方判错
    pub fn split_ref(reference: &str) -> (Option<&str>, &str) {
        match reference.strip_prefix("node://") {
            Some(rest) => match rest.split_once('/') {
                Some((node, local)) if !node.is_empty() => (Some(node), local),
                _ => (None, reference),
            },
            None => (None, reference),
        }
    }

    /// 组装一条全局引用：`node://<node>/<local>`。
    pub fn qualify(node: &str, local: &str) -> String {
        format!("node://{node}/{local}")
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// 最小 ULID 实现：48 位毫秒时间戳 + 80 位随机（getrandom 经 uuid 的 rng
/// 不可用于本 crate 的零依赖约束，这里用系统时间 + 进程内原子计数 + 地址
/// 熵混合——NodeId 只做身份标识，无安全用途；碰撞概率足够低且持久化后
/// 不再重新生成）。
fn ulid_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // 80 位随机：时间戳低位 ^ 原子计数 ^ 栈地址熵 ^ 纳秒
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let stack_addr = &millis as *const u64 as u64;
    let rand_hi = nanos.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ COUNTER.fetch_add(0x9e37_79b9, std::sync::atomic::Ordering::Relaxed)
        ^ stack_addr.rotate_left(17);
    let rand_lo = rand_hi.wrapping_mul(0xc2b2_ae3d_27d4_eb4f) ^ millis.rotate_right(23);

    let mut out = String::with_capacity(26);
    // 时间戳 48 位 → 10 字符（高 2 位补 0）
    let mut v = millis;
    for i in 0..10 {
        let shift = 5 * (9 - i);
        out.push(CROCKFORD[((v >> shift) & 0x1f) as usize] as char);
    }
    // 随机 80 位 → 16 字符
    for i in 0..8 {
        let shift = 5 * (7 - i);
        out.push(CROCKFORD[((rand_hi >> shift) & 0x1f) as usize] as char);
    }
    for i in 0..8 {
        let shift = 5 * (7 - i);
        out.push(CROCKFORD[((rand_lo >> shift) & 0x1f) as usize] as char);
    }
    let _ = &mut v;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_has_expected_shape() {
        let id = NodeId::generate();
        assert!(id.as_str().starts_with("node-"));
        assert_eq!(id.as_str().len(), "node-".len() + 26);
        // 两次生成不相同
        assert_ne!(NodeId::generate(), NodeId::generate());
    }

    #[test]
    fn split_ref_parses_qualified_and_local() {
        let (node, local) = NodeId::split_ref("node://node-abc/local:tui::local_user");
        assert_eq!(node, Some("node-abc"));
        assert_eq!(local, "local:tui::local_user");

        let (node, local) = NodeId::split_ref("local:tui::local_user");
        assert_eq!(node, None);
        assert_eq!(local, "local:tui::local_user");
    }

    #[test]
    fn split_ref_rejects_malformed() {
        // 无 node 段或缺 local 段时原样返回（调用方判错）
        assert_eq!(NodeId::split_ref("node://"), (None, "node://"));
        let (node, local) = NodeId::split_ref("node://node-abc/");
        assert_eq!(node, Some("node-abc"));
        assert_eq!(local, "");
    }

    #[test]
    fn qualify_roundtrip() {
        let q = NodeId::qualify("node-abc", "local:workspace:ws1:local_user");
        assert_eq!(q, "node://node-abc/local:workspace:ws1:local_user");
        assert_eq!(
            NodeId::split_ref(&q),
            (Some("node-abc"), "local:workspace:ws1:local_user")
        );
    }

    #[test]
    fn serde_transparent_roundtrip() {
        let id = NodeId::new("node-abc");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"node-abc\"");
        let back: NodeId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
