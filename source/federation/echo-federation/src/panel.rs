//! 联邦级 **Panel 身份**（P1，2026-11）：一个 Panel 用同一把密钥接入全网任一
//! 节点，权限由被接入节点自己的 `panel_access` 决定。
//!
//! ## 为什么不是"再加一个管理令牌"
//!
//! 此前 Panel 要看到 N 个节点就得配 N 份 `[[cores]]`（每份带该节点的
//! `management_access_token`），于是：
//!
//! - **权限不一致**：每台机器上那把令牌是谁配的、给了多大范围，没有统一口径；
//! - **可见性靠人**：漏配一台就少一台，且没有"我漏了谁"的可观测信号。
//!
//! 这里把身份提到**网络级**：`[federation] panel_keys` 是"谁能以 Panel 身份
//! 接入本节点"，`[federation] panel_access` 是"给了多大权限"。所有节点配同一把
//! 密钥（或同一组，便于轮换）即得到一致的身份；**权限是否一致仍由每个节点
//! 自己决定**——这是刻意的：一致性是运维约定，不是协议强制（不能因为 A 授权
//! 了 admin 就让 B 也 admin）。
//!
//! ## 与其它令牌的关系（关键约束）
//!
//! 端口合并模式下单一监听按**令牌命名空间**分流：管理令牌 → 管理 WS、
//! per-peer 令牌 → 联邦链路、panel 密钥 → 管理 WS（Panel 身份）。三类令牌必须
//! 互不相同，否则连接意图不可判定——启动期 fail-closed 校验（见
//! `echo-plugin-management::validate_panel_identity` 与 `validate_merged`）。
//!
//! ## 存在哪里
//!
//! 挂在 [`crate::Federation`] 上而不是另开一个管理器：合并模式的 WS 升级回调
//! 是**同步闭包**且只拿得到 `Arc<Federation>`（`has_peer_token_sync` 的同款
//! 理由——异步表在升级回调里不可用）。见 [`crate::Federation::panel_gate_sync`]。

use std::sync::{Arc, RwLock};

/// Panel 身份被授予的权限级别。
///
/// 从低到高：`Off` < `Readonly` < `Operate` < `Admin`。
/// 判定一律**白名单**（`readonly` 只放行 `PANEL_READONLY_COMMANDS`），
/// 未知级别解析失败即 `Off`（fail-closed）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum PanelAccess {
    /// 不接受 Panel 身份（默认；`panel_keys` 也通常为空）。
    #[default]
    Off,
    /// 只读：可看状态/时间线/工作区只读查询，不能发消息、不能改配置。
    Readonly,
    /// 操作：等同本机管理面的日常使用（发消息、跑工具、增删会话），
    /// 但**不含**改配置类危险项之外的区别——当前实现与 `Admin` 同权，
    /// 保留级别是为了将来能收紧（例如拒绝 `RequestSelfUpdate`）。
    Operate,
    /// 管理：等同持有 `management_access_token`（含保存 peer / 自更新）。
    Admin,
}

impl PanelAccess {
    /// 配置里的字符串形态（`off` / `readonly` / `operate` / `admin`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Readonly => "readonly",
            Self::Operate => "operate",
            Self::Admin => "admin",
        }
    }

    /// 从配置字符串解析（大小写不敏感；未知值 **`None`**——调用方据此
    /// fail-closed 报配置错误，而不是静默降级成 `Off` 让用户以为已开放）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "off" | "none" | "disabled" => Some(Self::Off),
            "readonly" | "read_only" | "read-only" | "ro" => Some(Self::Readonly),
            "operate" | "operational" | "rw" => Some(Self::Operate),
            "admin" | "full" => Some(Self::Admin),
            _ => None,
        }
    }

    /// 是否接受 Panel 身份接入（`Off` 一律 401）。
    pub fn is_open(self) -> bool {
        self != Self::Off
    }
}

/// 本节点的 Panel 身份配置（密钥表 + 授权级别）。
///
/// `keys` 允许多条：轮换时新旧并存（新增新密钥 → 全部 Panel 换完 → 删旧），
/// 不必全网同时改配置。空 `keys` = 不接受 Panel 身份（与 `access = Off` 等价）。
#[derive(Debug, Clone, Default)]
pub struct PanelIdentity {
    keys: Vec<String>,
    access: PanelAccess,
}

impl PanelIdentity {
    pub fn new(keys: Vec<String>, access: PanelAccess) -> Self {
        let keys = keys
            .into_iter()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        Self { keys, access }
    }

    /// 未配置（默认）：不接受任何 Panel 身份。
    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    pub fn access(&self) -> PanelAccess {
        self.access
    }

    /// 是否对外开放（有密钥且级别非 `Off`）。
    ///
    /// 两个条件**都是**必要条件：只配密钥不配级别 = 不收（避免"配了 key 就
    /// 意外开放"）；只配级别不配密钥 = 没有凭据，也收不了。
    pub fn is_open(&self) -> bool {
        self.access.is_open() && !self.keys.is_empty()
    }

    /// 该密钥对应的授权级别（常时比较；未命中/未开放 → `None`）。
    pub fn lookup(&self, presented: &str) -> Option<PanelAccess> {
        if !self.is_open() || presented.is_empty() {
            return None;
        }
        let hit = self
            .keys
            .iter()
            .any(|key| constant_time_eq(presented.as_bytes(), key.as_bytes()));
        hit.then_some(self.access)
    }
}

/// 连接到本节点的 Panel 身份判定入口（挂在 `Federation` 上的同步门）。
///
/// 为什么是 `Arc<RwLock<_>>` 而不是 `&self` 字段：合并模式的 WS 升级回调是
/// **同步闭包**且要求 `'static`（见模块文档），只能把句柄 clone 进闭包。
#[derive(Debug, Clone, Default)]
pub struct PanelGate {
    inner: Arc<RwLock<PanelIdentity>>,
}

impl PanelGate {
    pub fn new(identity: PanelIdentity) -> Self {
        Self {
            inner: Arc::new(RwLock::new(identity)),
        }
    }

    /// 整体替换（配置热更新/启动装配）。
    pub fn set(&self, identity: PanelIdentity) {
        if let Ok(mut slot) = self.inner.write() {
            *slot = identity;
        }
    }

    pub fn identity(&self) -> PanelIdentity {
        self.inner
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }

    /// 是否对外接受 Panel 身份。
    pub fn is_open(&self) -> bool {
        self.inner
            .read()
            .map(|slot| slot.is_open())
            .unwrap_or(false)
    }

    /// 该凭据对应的级别（`None` = 不是 Panel 身份/未开放）。
    pub fn lookup(&self, presented: &str) -> Option<PanelAccess> {
        self.inner
            .read()
            .ok()
            .and_then(|slot| slot.lookup(presented))
    }
}

/// 常时字符串比较（避免按字节早退泄露前缀）。
///
/// 与 `echo-plugin-management` 的同名函数同实现：**故意重复**而不是互相
/// 依赖——令牌比较是这个 crate 的安全边界，不该为了少 6 行就把它挪到
/// 依赖链的另一端。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_explicit_and_unknown_is_none() {
        assert_eq!(PanelAccess::parse(""), Some(PanelAccess::Off));
        assert_eq!(PanelAccess::parse("OFF"), Some(PanelAccess::Off));
        assert_eq!(PanelAccess::parse("readonly"), Some(PanelAccess::Readonly));
        assert_eq!(PanelAccess::parse("read-only"), Some(PanelAccess::Readonly));
        assert_eq!(PanelAccess::parse("operate"), Some(PanelAccess::Operate));
        assert_eq!(PanelAccess::parse("admin"), Some(PanelAccess::Admin));
        // 未知值必须 None（调用方 fail-closed），不能静默成 Off。
        assert_eq!(PanelAccess::parse("everyone"), None);
        assert_eq!(PanelAccess::parse("rw+"), None);
    }

    #[test]
    fn open_requires_both_keys_and_level() {
        let only_keys = PanelIdentity::new(vec!["k1".into()], PanelAccess::Off);
        assert!(!only_keys.is_open(), "只配密钥不配级别 = 不开放");
        let only_level = PanelIdentity::new(vec![], PanelAccess::Admin);
        assert!(!only_level.is_open(), "只配级别不配密钥 = 无凭据，不开放");
        let both = PanelIdentity::new(vec!["k1".into(), "  ".into()], PanelAccess::Readonly);
        assert!(both.is_open(), "空串密钥必须被过滤掉而不是当成'任意密钥'");
        assert_eq!(both.keys(), &["k1".to_string()]);
    }

    #[test]
    fn lookup_matches_only_registered_keys() {
        let identity = PanelIdentity::new(vec!["k1".into(), "k2".into()], PanelAccess::Operate);
        assert_eq!(identity.lookup("k1"), Some(PanelAccess::Operate));
        assert_eq!(identity.lookup("k2"), Some(PanelAccess::Operate));
        assert_eq!(identity.lookup("k3"), None);
        assert_eq!(identity.lookup(""), None, "空凭据不得命中");
        assert_eq!(identity.lookup("k1 "), None, "前导/尾随空格不宽松匹配");
    }

    #[test]
    fn disabled_identity_never_matches() {
        let identity = PanelIdentity::disabled();
        assert!(!identity.is_open());
        assert_eq!(identity.lookup("anything"), None);
    }

    #[test]
    fn gate_is_swappable_at_runtime() {
        let gate = PanelGate::default();
        assert!(!gate.is_open());
        assert_eq!(gate.lookup("k1"), None);
        gate.set(PanelIdentity::new(vec!["k1".into()], PanelAccess::Admin));
        assert!(gate.is_open());
        assert_eq!(gate.lookup("k1"), Some(PanelAccess::Admin));
        gate.set(PanelIdentity::disabled());
        assert_eq!(gate.lookup("k1"), None, "撤销后旧密钥立即失效");
    }
}
