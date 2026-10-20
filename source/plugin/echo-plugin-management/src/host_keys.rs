//! 宿主能力的服务键（管理面本身 + 会话视图域）。
//!
//! ## 管理面这一段为什么需要宿主
//!
//! 监听地址的**缺省值**来自 `core.toml`、令牌只在 `core.toml` / 环境变量里、
//! Panel 通道桥是组合根造的、合并模式下还要接上联邦入站句柄——四件事都是
//! 组装期的知识，插件不认识配置文件。插件只负责"拿到这四样之后怎么监听"。
//!
//! 注意 `plugins.toml` 可以覆盖**地址**（`[plugin.config] address`）：那条
//! 通路读的是 `MountContext::config`，不经过本 trait。

use std::sync::Arc;

use echo_context::ServiceKey;
use echo_federation::Federation;
use echo_protocol::BackendBridge;

use crate::session_view::SessionViewHost;

/// 管理面需要宿主提供的一切。
pub trait ManagementHost: Send + Sync {
    /// `core.toml` 里的监听地址（缺省值；`plugins.toml` 可覆盖）。
    fn default_address(&self) -> String;
    /// 访问令牌（只从宿主配置来——**不接受**插件配置覆盖：插件配置会进
    /// 插件清单接口与日志排查面，凭证放那里等于主动泄漏）。
    fn token(&self) -> String;
    /// Panel 通道桥（入站命令塞进去、事件从里面出）。
    fn bridge(&self) -> Arc<BackendBridge>;
    /// 合并模式下承接联邦入站的句柄（`None` = 独立监听）。
    fn federation(&self) -> Option<Arc<Federation>>;
}

/// 管理面的宿主接缝。
pub static MANAGEMENT_HOST: ServiceKey<Arc<dyn ManagementHost>> =
    ServiceKey::new("echo-agent.host.management");

/// 会话视图命令域的宿主接缝（面板快照 / 上下文 / 时间线 / 派生历史）。
pub static SESSION_VIEW_HOST: ServiceKey<Arc<dyn SessionViewHost>> =
    ServiceKey::new("echo-agent.host.session_view");
