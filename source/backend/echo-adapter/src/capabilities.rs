//! 平台能力扩展 trait（capability extension）。
//!
//! `Adapter` trait 只保留**平台中立**的最小面：生命周期（start/stop/
//! remove_instance）、收发（send_message/subscribe）、状态（status_info）。
//! 平台专属的管理能力经**扩展 trait** 暴露，`Adapter` 面只增加一个
//! `Option<&dyn XxxCapability>` 访问器（见 [`crate::traits::Adapter::qq_management`]）。
//!
//! # 为什么不放默认 no-op
//!
//! 这些方法原先内嵌在 `Adapter` trait 上、带默认空实现——非 QQ 适配器上
//! 「调用成功但什么也没发生」：命令层拿到的成功回执是假的，错误在重启后
//! 才以"配置没生效"的形式浮现。下沉为扩展 trait 后，`None` 明确表示
//! 「该平台不支持此能力」，调用方必须显式处理（回明确错误）。
//!
//! # 模式（新增平台时）
//!
//! Telegram 等新平台同理：定义 `TelegramManagement` 之类的独立能力 trait，
//! 在 `Adapter` 面上加一个 `telegram_management()` 访问器；平台无关的
//! 能力（如"有联系人列表"）才考虑提升到公共面。

use async_trait::async_trait;

use crate::traits::AdapterError;
use echo_defs::GateMode;

/// QQ（OneBot）实例的附加运行信息（多实例面板 / 诊断用）。
///
/// 这些字段原先内嵌在 [`crate::traits::AdapterInfo`] 上（QQ 专属却占用
/// 通用快照类型）；移到能力扩展后，通用快照保持平台中立。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QqInstanceInfo {
    /// 归属人格 id（多实例；`None` = legacy 单实例未指定）。
    pub persona: Option<String>,
    /// NapCat 容器名（展示 / 诊断）。
    pub container: Option<String>,
    /// NapCat WebUI 地址（扫码登录页面链接）。
    pub webui_url: Option<String>,
    /// OneBot HTTP API 地址。
    pub onebot_url: Option<String>,
    /// 反向 WS 监听地址（实例端口分配后的实际绑定值）。
    #[serde(default)]
    pub bind_address: String,
}

/// QQ（OneBot）平台管理能力：门控模式 / 名单 / 管理员 / 二维码 / 群友列表。
///
/// 取用方式：`adapter.qq_management()` 返回 `Option<&dyn QqManagement>`；
/// `None` = 该适配器不是 QQ 实现（不支持此管理面）。
#[async_trait]
pub trait QqManagement: Send + Sync {
    /// 实例附加信息（persona / 容器 / URL / 绑定地址）。
    fn instance_info(&self) -> QqInstanceInfo;

    /// 更新运行时白名单（ID 以字符串形态跨层传递，实现方自行解析）。
    fn update_allowlist(&self, user_ids: Vec<String>, group_ids: Vec<String>);

    /// 更新运行时黑名单。
    fn update_denylist(&self, user_ids: Vec<String>, group_ids: Vec<String>);

    /// 当前过滤配置 `(allow_users, allow_groups, deny_users, deny_groups)`。
    fn get_filter_info(&self) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>);

    /// 设置门控模式。
    fn set_gate_mode(&self, mode: GateMode);

    /// 当前门控模式。
    fn get_gate_mode(&self) -> GateMode;

    /// 设置管理员 QQ（0 = 清除）。
    fn set_owner_qq(&self, owner_qq: i64);

    /// 当前管理员 QQ（0 = 未设置）。
    fn get_owner_qq(&self) -> i64;

    /// 取登录二维码 PNG（Core 代 Panel 从平台容器取回）。
    async fn login_qrcode_png(&self) -> Result<Vec<u8>, String>;

    /// 全部群列表（不过滤；管理面编辑白/黑名单用）。
    async fn get_all_groups(&self) -> Result<Vec<(i64, String)>, AdapterError>;

    /// 全部好友列表（不过滤；管理面编辑白/黑名单用）。
    async fn get_all_friends(&self) -> Result<Vec<(i64, String)>, AdapterError>;
}
