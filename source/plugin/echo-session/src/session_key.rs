//! 会话标识词汇：`SessionKey` —— 结构化的会话 id 与解析。
//!
//! 这是**跨边界契约**（会话 id 出现在事件日志文件名、工作区通道、联邦
//! `node://` 引用、Panel 会话列表与插件入站消息里），因此它的规范形式与
//! 解析规则属于定义层级的词汇，不属于任何一个消费者。
//!
//! 依赖纪律：本模块只依赖 `echo-defs`（`NodeId` 的 `node://` 拆解），
//! 不依赖任何实现 crate。

/// Structured session identity, replacing the old `user_{qq_number}` format.
///
/// ```text
/// format: {platform}:{scope}:{scope_id}:{user_id}[@{account}]
///
/// Examples:
///   qq:dm::123456           — QQ direct message, user 123456
///   qq:group:987654:123456  — QQ group 987654, user 123456
///   qq:dm::123456@alix-2    — 同上，但来自 QQ 实例 "alix-2"（多实例维度）
///   local:tui::local_user   — local TUI interaction
/// ```
///
/// `account` 是**插件实例名**（`[instances.<platform>.instances.<id>]`）。
/// 单实例且实例名等于平台名（如 `qq`）时不产生后缀，历史会话 id 全部保持有效。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    /// Platform (e.g., "qq", "local").
    pub platform: String,
    /// Scope: "dm", "group", "tui".
    pub scope: String,
    /// Scope identifier (group_id for groups, empty for dm).
    pub scope_id: String,
    /// User identifier (platform-specific user ID as string).
    pub user_id: String,
    /// 插件实例名（多实例维度；None 或等于平台名 = 默认单实例，不写入 id）。
    pub account: Option<String>,
}

impl SessionKey {
    /// 本机会话的平台标识（区别于任何插件实例平台）。
    pub const LOCAL_PLATFORM: &'static str = "local";

    /// Build the canonical session ID string.
    ///
    /// 多实例（实例名 ≠ 平台名）时以 `@{account}` 结尾，与旧格式天然区分：
    /// 单实例部署（实例名恰为平台名，如 `qq`）的既有会话 id 一字不变。
    pub fn to_session_id(&self) -> String {
        let base = format!(
            "{}:{}:{}:{}",
            self.platform, self.scope, self.scope_id, self.user_id
        );
        match self.account() {
            Some(account) => format!("{base}@{account}"),
            None => base,
        }
    }

    /// 非默认实例的实例名（None = 默认实例 / 未指定）。
    ///
    /// 「默认」= 空串，或恰好等于平台名（`qq` 实例之于 `qq` 平台）——用
    /// 「实例名 == 平台名」而非写死的默认实例名，新增平台的默认实例
    /// （`<platform>`）同样不产生后缀。
    pub fn account(&self) -> Option<&str> {
        self.account
            .as_deref()
            .filter(|a| !a.is_empty() && !a.eq_ignore_ascii_case(&self.platform))
    }

    /// 是否为**本机会话**（非平台插件投递）。
    ///
    /// 判据是 `platform == "local"`——`local` 是宿主自己的平台标识，与
    /// 「哪个实例插件」无关；框架由此不需要认识任何平台名。
    pub fn is_local(&self) -> bool {
        self.platform.eq_ignore_ascii_case(Self::LOCAL_PLATFORM)
    }

    /// Create a session key for local TUI use.
    pub fn local_tui() -> Self {
        SessionKey {
            platform: "local".into(),
            scope: "tui".into(),
            scope_id: String::new(),
            user_id: "local_user".into(),
            account: None,
        }
    }

    /// Create a session key for a local workspace channel
    /// (`local:workspace:<workspace_id>:local_user`): the workspace's own
    /// local conversation — activating a workspace enters this channel
    /// (2026-09-14「激活 = 进入项目对话」).
    pub fn local_workspace(workspace_id: &str) -> Self {
        SessionKey {
            platform: "local".into(),
            scope: "workspace".into(),
            scope_id: workspace_id.to_string(),
            user_id: "local_user".into(),
            account: None,
        }
    }

    /// Parse a session ID string back into a key. Returns `None` on legacy
    /// `user_{id}` format (falls back to `local:tui:`) or malformed strings.
    pub fn parse(session_id: &str) -> Option<Self> {
        // federation Phase 0：`node://<node>/<local>` 全局引用——剥离节点
        // 前缀按本机格式解析，节点归属由 [`SessionKey::parse_qualified`]
        // 保留（Phase 1 联邦路由消费）。
        let (_node, local) = echo_defs::NodeId::split_ref(session_id);
        let session_id = local;
        // Legacy format: user_{digits}
        if session_id.starts_with("user_") {
            let user_id = session_id.strip_prefix("user_")?;
            return Some(SessionKey {
                platform: "local".into(),
                scope: "tui".into(),
                scope_id: String::new(),
                user_id: user_id.to_string(),
                account: None,
            });
        }
        let parts: Vec<&str> = session_id.splitn(4, ':').collect();
        if parts.len() != 4 {
            return None;
        }
        // 多实例维度：user_id 尾部 `@<account>`（默认实例不写后缀）。
        let (user_id, account) = match parts[3].split_once('@') {
            Some((user, account)) if !account.is_empty() => (user, Some(account.to_string())),
            _ => (parts[3], None),
        };
        Some(SessionKey {
            platform: parts[0].to_string(),
            scope: parts[1].to_string(),
            scope_id: parts[2].to_string(),
            user_id: user_id.to_string(),
            account,
        })
    }

    /// 解析一条可能带 `node://` 前缀的会话引用，保留节点归属。
    ///
    /// 返回 `(node, key)`：`node = None` 表示本机会话（既有一切 id 不变）；
    /// `Some(node)` 表示远程节点上的会话——Phase 0 仅解析，Core 内所有
    /// 现存调用方仍用 [`SessionKey::parse`]（节点前缀被剥离、按本机处理，
    /// 行为与此前一致）。
    pub fn parse_qualified(reference: &str) -> Option<(Option<String>, Self)> {
        let (node, local) = echo_defs::NodeId::split_ref(reference);
        Self::parse(local).map(|key| (node.map(str::to_string), key))
    }
}
