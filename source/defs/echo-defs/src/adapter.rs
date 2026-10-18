//! The platform-adapter seam — `Adapter` trait, status/error types, and the
//! `AdapterManagement` capability extension.
//!
//! Definitions live here (the capability-definition layer); `echo-adapter`
//! re-exports them so `echo_adapter::Adapter` / `echo_adapter::traits::…`
//! paths keep working, while host glue (registry, ConfigStore) stays in
//! `echo-adapter`.
//!
//! **Inbound flow** is not a trait here: adapters run out-of-process and push
//! messages through the plugin protocol (`HostCall("adapter_host",
//! "deliver_message")`), which the host turns into an `mpsc` queue consumed by
//! the composition root. An earlier `InboundMessageHook` trait had no
//! registration mechanism and was removed rather than left as a decoy.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Connection state of an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterConnectionState {
    /// Not started.
    Stopped,
    /// Running and connected.
    Connected,
    /// Running but disconnected (no client connected).
    Disconnected,
    /// In the process of starting.
    Starting,
    /// In the process of stopping.
    Stopping,
    /// Encountered an error.
    Error,
}

/// Snapshot of adapter status for the TUI / API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterInfo {
    /// Unique adapter identifier (e.g., "qq").
    pub name: String,
    /// Human-readable label (e.g., "QQ/OneBot").
    pub display_name: String,
    /// Connection status.
    pub status: AdapterConnectionState,
    /// Self ID on the platform (e.g., QQ number as string).
    pub self_id: Option<String>,
    /// When the adapter was started (Unix timestamp).
    pub started_at: Option<i64>,
    /// Platform name for session scoping.
    pub platform: String,
    /// Whether this adapter is configured and enabled.
    pub configured: bool,
    // 平台专属附加信息不在此（2026-10 抽象泄漏重构）：QQ 实例的
    // persona/容器/URL/绑定地址经
    // [`AdapterManagement::cached_state`]（能力扩展 trait）暴露，
    // 通用快照保持平台中立。
    //
    // [`AdapterManagement::cached_state`]: crate::adapter::AdapterManagement::cached_state
}

/// 适配器向模型暴露的一个工具（声明式）。
///
/// **平台语义住在插件里**：Core 只负责把声明合并后注册、把调用透传给
/// `invoke_custom(name, args)`。工具名 / 描述 / 参数 schema 全部由适配器插件
/// 提供，Core 不认识任何一个（此前 10 个 QQ 语义工具的 schema 写死在
/// `core::adapter_tools`，约 800 行）。
#[derive(Debug, Clone, PartialEq)]
pub struct AdapterToolSpec {
    /// 工具名，同时是 `invoke_custom` 的 method 名。
    pub name: String,
    /// 给模型看的说明。
    pub description: String,
    /// 参数 JSON Schema（宿主会按需注入可选 `account` 实例寻址参数）。
    pub parameters: Value,
    /// 所属包 id（面板按包批量勾选；`None` = 插件 id）。
    pub package: Option<String>,
    /// 成功调用是否即产生**平台可见输出**（见 `Tool::produces_visible_output`）。
    pub visible_reply: bool,
}

/// Adapter errors.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("adapter {0} is not configured")]
    NotConfigured(String),

    #[error("adapter {0} is already running")]
    AlreadyRunning(String),

    #[error("adapter {0} is not running")]
    NotRunning(String),

    #[error("adapter error: {0}")]
    Internal(String),

    #[error("send failed: {0}")]
    SendFailed(String),

    #[error("timeout")]
    Timeout,
}

/// An abstraction over a messaging platform (QQ, Telegram, Discord, etc.).
///
/// Adapters translate platform-specific events into platform-agnostic
/// [`IncomingMessage`] values, and
/// accept [`MessageTarget`] values to send replies.
///
/// # Lifecycle
///
/// 1. Created with configuration (may be incomplete — see `is_configured()`).
/// 2. `subscribe()` — the agent hooks in to receive events.
/// 3. `start()` — the adapter begins listening for platform events.
/// 4. `stop()` — graceful shutdown.
#[async_trait]
pub trait Adapter: Send + Sync {
    /// Unique adapter identifier (e.g., "qq").
    fn name(&self) -> &str;

    /// Human-readable label (e.g., "QQ / OneBot").
    fn display_name(&self) -> &str;

    /// Platform tag used in session scoping (e.g., "qq").
    fn platform(&self) -> &str;

    /// Whether this adapter has a valid configuration to run.
    fn is_configured(&self) -> bool;

    /// Current status info for display.
    fn status_info(&self) -> AdapterInfo;

    /// Start the adapter. For a QQ adapter this binds the reverse-WS server.
    async fn start(&self) -> Result<(), AdapterError>;

    /// Stop the adapter gracefully.
    async fn stop(&self) -> Result<(), AdapterError>;

    /// 平台中立的适配器管理面（W3.1，2026-10 适配器管理通道）。
    ///
    /// 平台管理能力的唯一入口；`None` = 该适配器不提供
    /// 管理面。消费方（命令域 / 面板通道 / 远程适配器）只应依赖本访问器。
    fn management(&self) -> Option<&dyn AdapterManagement> {
        None
    }

    /// UI 描述符（适配器管理界面声明；泛化 Core-1，2026-10）。
    ///
    /// 宿主经本访问器把描述符填进 `AdapterStatus.ui` 下发 Panel（通用
    /// 渲染器按 `ui.version` 选择方案）；远程适配器返回
    /// `AdapterContribution.ui` 原样。默认 `None` = 未声明。
    fn ui_descriptor(&self) -> Option<Value> {
        None
    }

    /// 会话标签（会话分组显示；泛化 Core-1，2026-10）。
    ///
    /// 如 `{"dm":"私聊","group":"群聊"}`；远程适配器返回
    /// `AdapterContribution.session_labels` 原样。默认 `None` = 未声明。
    fn session_labels(&self) -> Option<Value> {
        None
    }

    /// 能力位：`gating`（门控平台的共享配置写方法——`set_gate_mode` /
    /// `update_allowlist` / `update_denylist` / `set_owner`——按平台跨
    /// 实例共享）。
    ///
    /// 宿主在写方法广播时校验**源实例**本能力位（泛化 S5 / 01b §D.2 a′）；
    /// 远程适配器返回 `AdapterContribution.capabilities.gating`。默认
    /// `false`（未声明 = 每实例独立门控，不做跨实例对齐）。
    fn gating_capabilities(&self) -> bool {
        false
    }

    /// 宿主按本能力位（而非 platform）筛选可注册消息工具的适配器
    ///（泛化 S6 / 01b §D.4）；远程适配器返回
    /// `AdapterContribution.capabilities.send_text`。默认 `false`
    ///（未声明 = 非出站通道，不注册任何适配器工具）。
    fn sends_text(&self) -> bool {
        false
    }

    /// 该适配器贡献给模型的工具声明（缺省空 = 不贡献工具）。
    ///
    /// 宿主按 name 跨实例合并后注册；调用时透传为
    /// [`AdapterManagement::invoke_custom`]（method = 工具名）。多实例部署
    /// 由宿主注入可选 `account` 参数寻址，适配器无需关心。
    fn tool_specs(&self) -> Vec<AdapterToolSpec> {
        Vec::new()
    }
}

/// 平台中立的适配器管理面（W3.1，2026-10 适配器管理通道）。
///
/// 定义在 `echo-defs`（定义层）；`echo-adapter` re-export 保持旧路径兼容。
///
/// 平台专属管理能力的目标形态（2026-10 适配器管理通道；本 trait 已是唯一能力扩展）：
///
/// - 管理动作经 **method 路由表**（[`AdapterManagement::invoke_custom`]）
///   异步执行——方法名由平台自定义；约定与插件协议 `custom op` 及线协议
///   `InvokeAdapterOp` 同名直通（`set_gate_mode` / `update_allowlist` / …）；
/// - UI / 管理状态经 [`AdapterManagement::cached_state`] **同步读**
///   （inproc = 内部状态投影；remote = epoch 缓存）——实现必须零 IO、
///   不可阻塞（调用方在命令处理路径上同步读取）。
///
/// 取用方式：`adapter.management()` 返回 `Option<&dyn AdapterManagement>`；
/// `None` = 该适配器不提供管理面。
#[async_trait]
pub trait AdapterManagement: Send + Sync {
    /// 当前 UI / 管理状态文档（同步读、零 IO；`None` = 暂无状态）。
    ///
    /// 约定键（与适配器插件规范的 UI 描述符 `bind` 对齐；实现可扩展）：
    /// - `instance`：`{ persona?, container?, webui_url?, onebot_url?, bind_address? }`
    /// - `online` / `self_id`：连接态快照
    /// - `gate`：`{ mode, allowlist_users[], allowlist_groups[], denylist_users[], denylist_groups[] }`
    /// - `owner`：管理员 id（0 = 未设置）
    fn cached_state(&self) -> Option<Value>;

    /// 执行一个管理方法（写路径；方法名与参数由平台定义）。
    ///
    /// 推荐方法名（与 QQ 参考实现对齐，供跨平台消费方复用）：
    /// `set_gate_mode` / `get_gate_mode` / `update_allowlist` /
    /// `update_denylist` / `get_filter_config` / `set_owner` / `get_owner` /
    /// `login_status` / `login_qrcode` / `get_groups` / `get_friends` /
    /// `instance_info`。未知方法返回可读错误（fail-closed）。
    async fn invoke_custom(&self, method: &str, payload: Value) -> Result<Value, AdapterError>;
}
