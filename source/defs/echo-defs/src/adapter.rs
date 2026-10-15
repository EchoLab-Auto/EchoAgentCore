//! The platform-adapter seam — `Adapter` trait, status/error types, and the
//! `AdapterManagement` capability extension.
//!
//! Definitions live here (the capability-definition layer); `echo-adapter`
//! re-exports them so `echo_adapter::Adapter` / `echo_adapter::traits::…`
//! paths keep working, while implementations (filter pipeline, registry,
//! ConfigStore) stay in `echo-adapter`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::chat::{AdapterEvent, IncomingMessage, MessageTarget, SendResult};

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

/// Hook that platform adapters use to deliver inbound messages into the
/// agent framework, without depending on echo-agent directly.
///
/// This boundary is deliberately one-way. Agent output is not returned from
/// the hook; sending a platform message must happen through an adapter tool.
#[async_trait]
pub trait InboundMessageHook: Send + Sync {
    /// Deliver one inbound platform message to the agent.
    ///
    /// The hook owns session lookup, event emission, and LLM processing.
    /// A successful return only acknowledges processing; it never contains
    /// text for the adapter to send automatically.
    async fn on_incoming_message(&self, msg: IncomingMessage) -> Result<(), String>;

    /// Notify the agent that an adapter's connection state changed.
    fn on_connection_state(&self, adapter_name: &str, connected: bool, self_id: Option<String>);
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

    /// 彻底移除实例资源（实例**删除**语义，2026-10 巡检遗留）。
    ///
    /// 与 `stop`（临时停用，保留数据/登录态）不同：实现方应停止适配器
    /// 并回收外部资源（QQ/NapCat：容器 + 数据卷 + compose 文件）。
    /// 默认 no-op（无外部资源的适配器无需实现）。幂等：重复调用安全。
    async fn remove_instance(&self) -> Result<(), AdapterError> {
        Ok(())
    }

    /// Send a text message to a specific platform target.
    async fn send_message(
        &self,
        target: &MessageTarget,
        content: &str,
    ) -> Result<SendResult, AdapterError>;

    /// Subscribe to adapter events. May be called multiple times.
    fn subscribe(&self, tx: mpsc::UnboundedSender<AdapterEvent>);

    /// 平台中立的适配器管理面（W3.1，2026-10 适配器管理通道）。
    ///
    /// 平台管理能力的唯一入口；`None` = 该适配器不提供
    /// 管理面。消费方（命令域 / 面板通道 / 远程适配器）只应依赖本访问器。
    fn management(&self) -> Option<&dyn AdapterManagement> {
        None
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
