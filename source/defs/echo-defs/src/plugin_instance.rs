//! The platform-instance seam — `PluginInstance` trait, status/error types, and the
//! `InstanceManagement` capability extension.
//!
//! Definitions live here (the capability-definition layer); `echo-instance`
//! re-exports them so `echo_defs::plugin_instance::PluginInstance` / `echo_defs::plugin_instance::…`
//! paths keep working, while host glue (registry, ConfigStore) stays in
//! `echo-instance`.
//!
//! **Inbound flow** is not a trait here: instances run out-of-process and push
//! messages through the plugin protocol (`HostCall("core_api",
//! "submit_input")`), which the host turns into an `mpsc` queue consumed by
//! the composition root. An earlier `InboundMessageHook` trait had no
//! registration mechanism and was removed rather than left as a decoy.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Connection state of an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstanceConnectionState {
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

/// Snapshot of instance status for the TUI / API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceInfo {
    /// Unique instance identifier (e.g., "qq").
    pub name: String,
    /// Human-readable label (e.g., "QQ/OneBot").
    pub display_name: String,
    /// Connection status.
    pub status: InstanceConnectionState,
    /// Self ID on the platform (e.g., QQ number as string).
    pub self_id: Option<String>,
    /// When the instance was started (Unix timestamp).
    pub started_at: Option<i64>,
    /// Platform name for session scoping.
    pub platform: String,
    /// Whether this instance is configured and enabled.
    pub configured: bool,
    // 平台专属附加信息不在此（2026-10 抽象泄漏重构）：QQ 实例的
    // persona/容器/URL/绑定地址经
    // [`InstanceManagement::cached_state`]（能力扩展 trait）暴露，
    // 通用快照保持平台中立。
    //
    // [`InstanceManagement::cached_state`]: crate::instance::InstanceManagement::cached_state
}

/// 插件实例向模型暴露的一个工具（声明式）。
///
/// **平台语义住在插件里**：Core 只负责把声明合并后注册、把调用透传给
/// `invoke_custom(name, args)`。工具名 / 描述 / 参数 schema 全部由实例插件
/// 提供，Core 不认识任何一个（此前 10 个 QQ 语义工具的 schema 写死在
/// `core::instance_tools`，约 800 行）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
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

/// PluginInstance errors.
#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
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
/// Instances translate platform-specific events into platform-agnostic
/// [`IncomingMessage`] values, and
/// accept [`MessageTarget`] values to send replies.
///
/// # Lifecycle
///
/// 1. Created with configuration (may be incomplete — see `is_configured()`).
/// 2. `subscribe()` — the agent hooks in to receive events.
/// 3. `start()` — the instance begins listening for platform events.
/// 4. `stop()` — graceful shutdown.
#[async_trait]
pub trait PluginInstance: Send + Sync {
    /// Unique instance identifier (e.g., "qq").
    fn name(&self) -> &str;

    /// Human-readable label (e.g., "QQ / OneBot").
    fn display_name(&self) -> &str;

    /// Platform tag used in session scoping (e.g., "qq").
    fn platform(&self) -> &str;

    /// Whether this instance has a valid configuration to run.
    fn is_configured(&self) -> bool;

    /// Current status info for display.
    fn status_info(&self) -> InstanceInfo;

    /// Start the instance. For a QQ instance this binds the reverse-WS server.
    async fn start(&self) -> Result<(), InstanceError>;

    /// Stop the instance gracefully.
    async fn stop(&self) -> Result<(), InstanceError>;

    /// 平台中立的插件实例管理面（W3.1，2026-10 插件实例管理通道）。
    ///
    /// 平台管理能力的唯一入口；`None` = 该插件实例不提供
    /// 管理面。消费方（命令域 / 面板通道 / 远程插件实例）只应依赖本访问器。
    fn management(&self) -> Option<&dyn InstanceManagement> {
        None
    }

    /// UI 描述符（插件实例管理界面声明；泛化 Core-1，2026-10）。
    ///
    /// 宿主经本访问器把描述符填进 `InstanceStatus.ui` 下发 Panel（通用
    /// 渲染器按 `ui.version` 选择方案）；远程插件实例返回
    /// `InstanceContribution.ui` 原样。默认 `None` = 未声明。
    fn ui_descriptor(&self) -> Option<Value> {
        None
    }

    /// 会话标签（会话分组显示；泛化 Core-1，2026-10）。
    ///
    /// 如 `{"dm":"私聊","group":"群聊"}`；远程插件实例返回
    /// `InstanceContribution.session_labels` 原样。默认 `None` = 未声明。
    fn session_labels(&self) -> Option<Value> {
        None
    }

    /// 能力位：`gating`（门控平台的共享配置写方法——`set_gate_mode` /
    /// `update_allowlist` / `update_denylist` / `set_owner`——按平台跨
    /// 实例共享）。
    ///
    /// 宿主在写方法广播时校验**源实例**本能力位（泛化 S5 / 01b §D.2 a′）；
    /// 远程插件实例返回 `InstanceContribution.capabilities.gating`。默认
    /// `false`（未声明 = 每实例独立门控，不做跨实例对齐）。
    fn gating_capabilities(&self) -> bool {
        false
    }

    /// 宿主按本能力位（而非 platform）筛选可注册消息工具的插件实例
    ///（泛化 S6 / 01b §D.4）；远程插件实例返回
    /// `InstanceContribution.capabilities.send_text`。默认 `false`
    ///（未声明 = 非出站通道，不注册任何插件实例工具）。
    fn sends_text(&self) -> bool {
        false
    }

    /// 该插件实例贡献给模型的工具声明（缺省空 = 不贡献工具）。
    ///
    /// 宿主按 name 跨实例合并后注册；调用时透传为
    /// [`InstanceManagement::invoke_custom`]（method = 工具名）。多实例部署
    /// 由宿主注入可选 `account` 参数寻址，插件实例无需关心。
    fn tool_specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
}

/// 平台中立的插件实例管理面（W3.1，2026-10 插件实例管理通道）。
///
/// 定义在 `echo-defs`（定义层）；`echo-instance` re-export 保持旧路径兼容。
///
/// 平台专属管理能力的目标形态（2026-10 插件实例管理通道；本 trait 已是唯一能力扩展）：
///
/// - 管理动作经 **method 路由表**（[`InstanceManagement::invoke_custom`]）
///   异步执行——方法名由平台自定义；约定与插件协议 `custom op` 及线协议
///   `InvokeInstanceOp` 同名直通（`set_gate_mode` / `update_allowlist` / …）；
/// - UI / 管理状态经 [`InstanceManagement::cached_state`] **同步读**
///   （inproc = 内部状态投影；remote = epoch 缓存）——实现必须零 IO、
///   不可阻塞（调用方在命令处理路径上同步读取）。
///
/// 取用方式：`instance.management()` 返回 `Option<&dyn InstanceManagement>`；
/// `None` = 该插件实例不提供管理面。
#[async_trait]
pub trait InstanceManagement: Send + Sync {
    /// 当前 UI / 管理状态文档（同步读、零 IO；`None` = 暂无状态）。
    ///
    /// 约定键（与实例插件规范的 UI 描述符 `bind` 对齐；实现可扩展）：
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
    async fn invoke_custom(&self, method: &str, payload: Value) -> Result<Value, InstanceError>;
}

// ── 注册表 ────────────────────────────────────────────────────────────────

use std::collections::HashMap;
use std::sync::Arc;

/// 宿主侧的插件实例注册表（`name → Arc<dyn PluginInstance>`）。
///
/// 内部 `RwLock<HashMap>`（内部可变性）——`Arc<InstanceRegistry>` 共享下
/// 注册 / 查询 / 删除安全进行（2026-10 实例插件化：运行期远程插件实例
/// 注册需要 `Arc` 下可写）。
pub struct InstanceRegistry {
    instances: std::sync::RwLock<HashMap<String, Arc<dyn PluginInstance>>>,
}

impl std::fmt::Debug for InstanceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceRegistry")
            .field("names", &self.names())
            .finish()
    }
}

impl Default for InstanceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl InstanceRegistry {
    pub fn new() -> Self {
        Self {
            instances: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Register an instance. Replaces any existing instance with the same name.
    ///
    /// 内部可变性（`RwLock`）：`Arc<InstanceRegistry>` 共享下经 `register` 追加
    /// （2026-10 实例插件化：运行期远程插件实例注册需要 `Arc` 下可写）。
    pub fn register(&self, instance: Arc<dyn PluginInstance>) {
        self.instances
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(instance.name().to_string(), instance);
    }

    /// Get an instance by name.
    ///
    /// 按值克隆返回（`RwLock` 读守卫不能跨边界转 `&Arc`；`Arc` 克隆开销可忽略）。
    pub fn get(&self, name: &str) -> Option<Arc<dyn PluginInstance>> {
        self.instances
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// List all registered instances.
    pub fn list_info(&self) -> Vec<InstanceInfo> {
        self.instances
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|a| a.status_info())
            .collect()
    }

    /// List instances that are configured (enabled in config).
    pub fn configured(&self) -> Vec<Arc<dyn PluginInstance>> {
        self.instances
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|a| a.is_configured())
            .cloned()
            .collect()
    }

    /// Start all configured instances.
    pub async fn start_all(&self) -> Vec<(String, Result<(), InstanceError>)> {
        let mut results = Vec::new();
        for a in self.configured() {
            let name = a.name().to_string();
            let result = a.start().await;
            results.push((name, result));
        }
        results
    }

    /// Stop all running instances.
    pub async fn stop_all(&self) {
        let instances: Vec<_> = self
            .instances
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for a in instances {
            let _ = a.stop().await;
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.instances
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.instances
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.instances
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct MockInstance {
        name: String,
        configured: bool,
        started: AtomicBool,
    }

    impl MockInstance {
        fn new(name: &str, configured: bool) -> Self {
            Self {
                name: name.into(),
                configured,
                started: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl PluginInstance for MockInstance {
        fn name(&self) -> &str {
            &self.name
        }
        fn display_name(&self) -> &str {
            &self.name
        }
        fn platform(&self) -> &str {
            "mock"
        }
        fn is_configured(&self) -> bool {
            self.configured
        }
        fn status_info(&self) -> InstanceInfo {
            InstanceInfo {
                name: self.name.clone(),
                display_name: self.name.clone(),
                status: if self.started.load(Ordering::SeqCst) {
                    InstanceConnectionState::Connected
                } else {
                    InstanceConnectionState::Stopped
                },
                self_id: None,
                started_at: None,
                platform: "mock".into(),
                configured: self.configured,
            }
        }
        async fn start(&self) -> Result<(), InstanceError> {
            self.started.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn stop(&self) -> Result<(), InstanceError> {
            self.started.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn registry_register_and_get() {
        let reg = InstanceRegistry::new();
        reg.register(Arc::new(MockInstance::new("test", true)));
        assert!(reg.get("test").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[tokio::test]
    async fn registry_start_configures_only() {
        let reg = InstanceRegistry::new();
        reg.register(Arc::new(MockInstance::new("enabled", true)));
        reg.register(Arc::new(MockInstance::new("disabled", false)));

        let results = reg.start_all().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "enabled");
        assert!(results[0].1.is_ok());

        let configured = reg.configured();
        assert_eq!(configured.len(), 1);
    }

    /// 能力扩展默认：未实现 `InstanceManagement` 的插件实例返回 `None`
    ///（非插件实例上不再存在"调用成功但什么也没发生"的假接口）。
    #[test]
    fn instance_without_management_returns_none() {
        let mock = MockInstance {
            name: "tg".into(),
            configured: true,
            started: std::sync::atomic::AtomicBool::new(false),
        };
        assert!(
            mock.management().is_none(),
            "未实现 InstanceManagement 的插件实例必须返回 None（显式不支持）"
        );
    }

    #[tokio::test]
    async fn registry_list_info() {
        let reg = InstanceRegistry::new();
        reg.register(Arc::new(MockInstance::new("a", true)));
        reg.register(Arc::new(MockInstance::new("b", false)));

        let info = reg.list_info();
        assert_eq!(info.len(), 2);
        assert!(info.iter().any(|i| i.name == "a" && i.configured));
        assert!(info.iter().any(|i| i.name == "b" && !i.configured));
    }
}
