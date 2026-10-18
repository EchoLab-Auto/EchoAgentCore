//! 适配器插件宿主侧（P1，2026-10 适配器插件化）。
//!
//! - [`RemoteInstance`]：`echo_defs::plugin_instance::PluginInstance` 的远程实现——异步方法经
//!   插件协议 `Invoke("<name>:<op>")`（能力位 fail-closed）；状态经
//!   `PluginState` 进程态 ∧ 连接态缓存仲裁（见 §交接与崩溃恢复语义）；
//! - [`RemoteInstanceManagement`]：`InstanceManagement` 的远程实现——
//!   `invoke_custom` 直通 `custom` op；`cached_state` 读 epoch 缓存；
//! - [`CoreApiService`]：插件 → 宿主的 `core_api` 宿主服务——
//!   `submit_input` 入队（回执 = 已入队，每实例有界队列 + 串行 worker
//!   保序）、`report_*` 更新连接态缓存。
//!
//! 选型与语义见 `document/design/instance-plugin.md`（协议规范 / 宿主侧实现）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::api::{IncomingMessage, InstanceContribution, InvokeContext, InvokeOutcome};
use crate::supervisor::{PluginHandle, PluginState};
use crate::transport::TransportError;

/// 入站队列容量上限（每实例；满 → `backpressure` 错误码，插件退避）。
const INBOUND_QUEUE_CAPACITY: usize = 256;

/// 远程适配器（`echo_defs::plugin_instance::PluginInstance` 的插件进程实现）。
///
/// 能力位 fail-closed：宿主只调用该适配器贡献中声明过的方法位
/// （[`InstanceContribution::capabilities`]），未声明的调用拒绝为
/// `unsupported`（不发起 Invoke）。
pub struct RemoteInstance {
    /// 适配器实例名（= 插件协议 `Invoke` 的贡献前缀）。
    pub name: String,
    contribution: InstanceContribution,
    handle: PluginHandle,
    /// 连接态缓存（`report_state` 上报刷新；进程态仲裁见 `status_info`）。
    connection: Mutex<ConnectionCache>,
    /// 管理面状态缓存（epoch 语义；泛化 01a §1.3：记录写入时的连接代际
    /// `generation`，读取时不一致视为失效——重启 / 升级换代后宿主缓存
    /// 不可信，等插件下一次推送恢复）。
    management_state: Mutex<Option<(u64, Value)>>,
    /// 管理面（构造后以 `PluginInstance::management()` 按引返回；`PluginInstance` 的
    /// 签名是 `Option<&dyn InstanceManagement>`——返回值借用本结构的字段）。
    management: RemoteInstanceManagement,
}

/// 连接态缓存（`report_state` 刷新）。
#[derive(Debug, Clone, Default)]
struct ConnectionCache {
    connected: bool,
    self_id: Option<String>,
}

impl RemoteInstance {
    /// 以插件句柄与适配器贡献构造。
    ///
    /// 管理面在构造后由 `PluginInstance::management()` 返回（`Option<&dyn …>`
    /// 按引借用本结构的 `management` 字段——`InstanceManagement` 的实现
    /// 只读 `connection` / `management_state` / `handle` / `contribution`，
    /// 不读名字段，构造顺序无自引用问题）。
    pub fn new(contribution: InstanceContribution, handle: PluginHandle) -> Arc<Self> {
        let name = contribution.name.clone();
        // 管理面持**指向本结构的弱引用**，生命周期严格绑定本结构。
        // `Arc::new_cyclic` 是拿到它的唯一正确方式：先用 `Weak::new()`
        // 占位会让 `management()` 永远失效（`invoke_custom` / `cached_state`
        // 一律返回 "has been destroyed"——宿主侧一切管理动作全废）。
        Arc::new_cyclic(|weak| Self {
            name: name.clone(),
            contribution,
            handle,
            connection: Mutex::new(ConnectionCache::default()),
            management_state: Mutex::new(None),
            management: RemoteInstanceManagement::from_weak(weak.clone(), name),
        })
    }

    /// 能力位校验：未声明能力位时拒绝为 `unsupported`（fail-closed）。
    fn require(&self, capability: bool, op: &str) -> Result<(), TransportError> {
        if capability {
            Ok(())
        } else {
            Err(TransportError::Protocol(format!(
                "adapter {} does not declare capability for {op}",
                self.name
            )))
        }
    }

    /// 更新连接态缓存（`report_state` 上报入口；`CoreApiService` 调用）。
    pub fn note_connection_state(&self, connected: bool, self_id: Option<String>) {
        let mut cache = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.connected = connected;
        cache.self_id = self_id;
    }

    /// 失效管理面状态缓存（epoch 语义：重启 / 升级后宿主调用）。
    pub fn invalidate_management_state(&self) {
        *self
            .management_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// 取管理面状态缓存（`InstanceManagement::cached_state` 的远程侧）。
    ///
    /// epoch（泛化 01a §1.3）：缓存记录写入时的连接代际；当前
    /// `handle.generation()` 不一致（重启 / 热替换换代）即视为失效返回
    /// `None`（惰性失效，不清除陈旧值），等插件下一次推送（`report_ui_state`
    /// / `get_state` 拉取）以新代际覆盖恢复。
    pub fn management_state(&self) -> Option<Value> {
        let guard = self
            .management_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (generation, state) = guard.as_ref()?;
        if *generation == self.handle.generation() {
            Some(state.clone())
        } else {
            None
        }
    }

    /// 更新管理面状态缓存（`report_ui_state` / 状态拉取入口；记录当前
    /// 连接代际，供 epoch 惰性失效比对）。
    pub fn note_management_state(&self, state: Value) {
        *self
            .management_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((self.handle.generation(), state));
    }

    /// 发起一次适配器 op（`Invoke("<name>:<op>")`）；能力位未声明时
    /// fail-closed 拒绝。
    async fn invoke_op(
        &self,
        capability: bool,
        op: &str,
        payload: Value,
    ) -> Result<Value, TransportError> {
        self.require(capability, op)?;
        let outcome = self
            .handle
            .invoke(
                &format!("{}:{}", self.name, op),
                InvokeContext::default(),
                payload,
            )
            .await;
        match outcome {
            InvokeOutcome::Ok { result, .. } => Ok(result),
            InvokeOutcome::Error { code, message } => {
                Err(TransportError::Protocol(format!("{code}: {message}")))
            }
        }
    }
}

/// `echo_defs::plugin_instance::PluginInstance` 的远程实现（`echo-agent` 侧注册点统一走
/// `InstanceRegistry`，trait 来自 `echo-defs`）。
#[async_trait]
impl echo_defs::plugin_instance::PluginInstance for RemoteInstance {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        &self.contribution.display_name
    }

    fn platform(&self) -> &str {
        &self.contribution.platform
    }

    fn is_configured(&self) -> bool {
        // 远程适配器以插件握手成功为配置完成（贡献已上报 = 插件已就绪）。
        true
    }

    fn status_info(&self) -> echo_defs::plugin_instance::InstanceInfo {
        // 进程态仲裁（最高优先）：插件 Pending / Failed 时一律显示非
        // Connected，绝不显示缓存的连接态（重启期间缓存不可信）。
        let state = self.handle.state();
        let cache = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let connected = match state {
            PluginState::Ready => cache.connected,
            PluginState::Pending => false,
            PluginState::Draining => cache.connected,
            PluginState::Stopped => false,
            PluginState::Failed(_) => false,
        };
        echo_defs::plugin_instance::InstanceInfo {
            name: self.name.clone(),
            display_name: self.contribution.display_name.clone(),
            status: match state {
                PluginState::Ready if connected => {
                    echo_defs::plugin_instance::InstanceConnectionState::Connected
                }
                PluginState::Ready | PluginState::Draining => {
                    echo_defs::plugin_instance::InstanceConnectionState::Disconnected
                }
                PluginState::Pending => {
                    echo_defs::plugin_instance::InstanceConnectionState::Starting
                }
                PluginState::Stopped => {
                    echo_defs::plugin_instance::InstanceConnectionState::Stopped
                }
                PluginState::Failed(_) => {
                    echo_defs::plugin_instance::InstanceConnectionState::Error
                }
            },
            self_id: cache.self_id.clone(),
            started_at: None,
            platform: self.contribution.platform.clone(),
            configured: true,
        }
    }

    async fn start(&self) -> Result<(), echo_defs::plugin_instance::InstanceError> {
        // 生命周期经插件协议直通（`<name>:start` / `<name>:stop`）。
        self.invoke_op(true, "start", json!({}))
            .await
            .map_err(|e| echo_defs::plugin_instance::InstanceError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), echo_defs::plugin_instance::InstanceError> {
        self.invoke_op(true, "stop", json!({}))
            .await
            .map_err(|e| echo_defs::plugin_instance::InstanceError::Internal(e.to_string()))?;
        Ok(())
    }

    fn management(&self) -> Option<&dyn echo_defs::plugin_instance::InstanceManagement> {
        // 远程管理面（cached_state / invoke_custom；按引返回本结构字段）。
        Some(&self.management)
    }

    fn ui_descriptor(&self) -> Option<Value> {
        // 泛化 Core-1：UI 描述符 = 插件贡献原样透传（注册时静态）。
        self.contribution.ui.clone()
    }

    fn session_labels(&self) -> Option<Value> {
        self.contribution.session_labels.clone()
    }

    fn tool_specs(&self) -> Vec<echo_defs::plugin_instance::ToolSpec> {
        // 平台语义住在插件贡献里：宿主只做形状转换，不认识任何工具名。
        self.contribution
            .tools
            .iter()
            .map(|tool| echo_defs::plugin_instance::ToolSpec {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
                package: tool.package.clone(),
                visible_reply: tool.visible_reply,
            })
            .collect()
    }

    fn gating_capabilities(&self) -> bool {
        // 泛化 S5（01b §D.2 a′）：写方法广播的源实例能力位。
        self.contribution.capabilities.gating
    }

    fn sends_text(&self) -> bool {
        // 泛化 S6（01b §D.4）：宿主按能力位过滤可注册消息工具的适配器。
        self.contribution.capabilities.send_text
    }
}

/// `InstanceManagement` 的远程实现（`invoke_custom` 直通 `custom` op；
/// `cached_state` 读 epoch 缓存）。
///
/// 持有对宿主 [`RemoteInstance`] 的弱引用（`Weak`）——管理面挂在宿主
/// 字段上、生命周期与宿主绑定，不形成循环强引用。
pub struct RemoteInstanceManagement {
    instance: std::sync::Weak<RemoteInstance>,
    /// 适配器名（弱引用失效后的错误消息用；弱引用升级失败 = 宿主已销毁）。
    name: String,
}

impl RemoteInstanceManagement {
    /// 以宿主适配器构造（宿主侧组合根在 `RemoteInstance` 完成构造后调用）。
    pub fn new(instance: Arc<RemoteInstance>) -> Self {
        let name = instance.name.clone();
        Self {
            instance: Arc::downgrade(&instance),
            name,
        }
    }

    /// 由宿主 `RemoteInstance` 内部构造：`weak` 来自 `Arc::new_cyclic`，
    /// 指向宿主自身（宿主销毁后自然失效，调用方拿到可读错误而非 UB）。
    fn from_weak(instance: std::sync::Weak<RemoteInstance>, name: String) -> Self {
        Self { instance, name }
    }

    /// 弱引用升级（宿主已销毁 → 可读错误）。
    fn instance(&self) -> Result<Arc<RemoteInstance>, echo_defs::plugin_instance::InstanceError> {
        self.instance.upgrade().ok_or_else(|| {
            echo_defs::plugin_instance::InstanceError::Internal(format!(
                "adapter {} has been destroyed",
                self.name
            ))
        })
    }
}

#[async_trait]
impl echo_defs::plugin_instance::InstanceManagement for RemoteInstanceManagement {
    fn cached_state(&self) -> Option<Value> {
        self.instance.upgrade()?.management_state()
    }

    async fn invoke_custom(
        &self,
        method: &str,
        payload: Value,
    ) -> Result<Value, echo_defs::plugin_instance::InstanceError> {
        let instance = self.instance()?;
        let gating_methods = [
            "set_gate_mode",
            "get_gate_mode",
            "update_allowlist",
            "update_denylist",
            "get_filter_config",
            "set_owner",
            "get_owner",
        ];
        let login_methods = ["login_status", "login_qrcode"];
        let capability = if gating_methods.contains(&method) {
            instance.contribution.capabilities.gating
        } else if login_methods.contains(&method) {
            instance.contribution.capabilities.login
        } else if method == "get_groups" {
            instance.contribution.capabilities.groups
        } else if method == "get_friends" {
            instance.contribution.capabilities.friends
        } else {
            true
        };
        instance
            .invoke_op(
                capability,
                "custom",
                json!({"method": method, "payload": payload}),
            )
            .await
            .map_err(|e| echo_defs::plugin_instance::InstanceError::Internal(e.to_string()))
    }
}

/// 插件 → 宿主的适配器宿主服务（`core_api`）。
///
/// 语义（见 §插件动作 表）：`submit_input` 入队（回执 = 已入队；
/// 每实例有界队列 + 串行 worker 保序）；`report_state` / `report_stopped` /
/// `report_error` 更新连接态缓存；`report_ui_state` 更新管理面状态缓存。
pub struct CoreApiService {
    instances: Mutex<HashMap<String, Arc<RemoteInstance>>>,
    /// 每实例入站队列（有界；worker 串行消费）。
    inbound_queues: Mutex<HashMap<String, tokio::sync::mpsc::Sender<IncomingMessage>>>,
    /// 连接态变化订阅者（组合根把 `InstanceStateChanged` 事件转发给 Panel；
    /// 订阅者退出（channel 关闭）时自动摘除）。
    state_sinks: Mutex<Vec<tokio::sync::mpsc::UnboundedSender<InstanceStateChange>>>,
    /// UI 状态（管理面状态文档）上报订阅者（泛化 01a §1.2）：`report_ui_state`
    /// 广播给组合根转 `InstanceState`；订阅者退出时自动摘除。
    ui_state_sinks: Mutex<Vec<tokio::sync::mpsc::UnboundedSender<InstanceUiStateChange>>>,
    /// `store_media` 落盘目录（缺省 = `media_store::media_dir()`）。
    media_dir: std::path::PathBuf,
}

/// 一次连接态变化（`report_state` / `report_stopped` / `report_error` 广播）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceStateChange {
    /// 适配器实例名。
    pub instance: String,
    /// 当前是否已连接。
    pub connected: bool,
    /// 平台账号（连接时自报；断开为 None）。
    pub self_id: Option<String>,
}

/// 一次 UI 状态上报（`report_ui_state` 广播；泛化 01a §1.2 S4）。
///
/// 与 [`InstanceStateChange`]（连接态，`Eq`）**分离**：状态文档是任意
/// `Value`，混入会破坏既有结构的派生与测试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceUiStateChange {
    /// 适配器实例名。
    pub instance: String,
    /// 管理面状态文档（`cached_state` 同构；形状见 00 §三）。
    pub state: Value,
}

/// `store_media` 单次登记上限（与 `media_store` 落盘上限同量级；path 形态
/// 读文件前先按元数据校验，避免把超大文件读进内存）。
const MAX_STORE_MEDIA_BYTES: u64 = 32 * 1024 * 1024;

impl CoreApiService {
    /// 空注册表（媒体目录 = 宿主媒体库）。
    pub fn new() -> Self {
        Self::with_media_dir(echo_defs::media_store::media_dir())
    }

    /// 空注册表，`store_media` 落盘到指定目录（测试 / 多数据根部署用）。
    pub fn with_media_dir(media_dir: std::path::PathBuf) -> Self {
        Self {
            instances: Mutex::new(HashMap::new()),
            inbound_queues: Mutex::new(HashMap::new()),
            state_sinks: Mutex::new(Vec::new()),
            ui_state_sinks: Mutex::new(Vec::new()),
            media_dir,
        }
    }

    /// 订阅连接态变化（组合根消费并发射 `InstanceStateChanged` 事件）。
    ///
    /// 多订阅者各自收全量；接收端 drop 后自动摘除。`report_stopped` /
    /// `report_error` 以 `connected=false` 广播（进程态仲裁在 `RemoteInstance`
    /// 一侧另行处理）。
    pub fn subscribe_states(&self) -> tokio::sync::mpsc::UnboundedReceiver<InstanceStateChange> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.state_sinks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(tx);
        rx
    }

    /// 广播一次连接态变化（发送失败的订阅者摘除）。
    fn broadcast_state(&self, change: InstanceStateChange) {
        let mut sinks = self
            .state_sinks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sinks.retain(|tx| tx.send(change.clone()).is_ok());
    }

    /// 订阅 UI 状态上报（`report_ui_state`；泛化 01a §1.2 S4）。
    ///
    /// 组合根消费并发射 `InstanceState` 事件（+ 派生旧 QQ 事件）；多订阅者
    /// 各自收全量，接收端 drop 后自动摘除。与连接态通道分离。
    pub fn subscribe_ui_states(
        &self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<InstanceUiStateChange> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.ui_state_sinks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(tx);
        rx
    }

    /// 广播一次 UI 状态上报（发送失败的订阅者摘除）。
    fn broadcast_ui_state(&self, change: InstanceUiStateChange) {
        let mut sinks = self
            .ui_state_sinks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sinks.retain(|tx| tx.send(change.clone()).is_ok());
    }

    /// `store_media`：`{path | bytes_b64, mime?}` → `{ref: "/media/<id>"}`。
    ///
    /// `path` 形态由宿主读取同机文件（插件为用户自装的可信代码，与宿主同
    /// 用户运行——读取范围不超出插件自身权限）；`bytes_b64` 用于小载荷。
    /// 两者同时给出时 `path` 优先。内容寻址：相同内容复用同一 id。
    fn store_media(&self, payload: &Value) -> Result<Value, (String, String)> {
        let invalid = |m: String| ("invalid_payload".to_string(), m);
        let mime = payload["mime"]
            .as_str()
            .unwrap_or("application/octet-stream");
        let bytes = if let Some(path) = payload["path"].as_str().filter(|p| !p.is_empty()) {
            let meta = std::fs::metadata(path)
                .map_err(|e| invalid(format!("store_media path {path}: {e}")))?;
            if !meta.is_file() {
                return Err(invalid(format!(
                    "store_media path {path}: not a regular file"
                )));
            }
            if meta.len() > MAX_STORE_MEDIA_BYTES {
                return Err((
                    "too_large".to_string(),
                    format!(
                        "store_media: {} bytes > {MAX_STORE_MEDIA_BYTES}",
                        meta.len()
                    ),
                ));
            }
            std::fs::read(path).map_err(|e| invalid(format!("store_media read {path}: {e}")))?
        } else if let Some(b64) = payload["bytes_b64"].as_str() {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| invalid(format!("store_media bytes_b64: {e}")))?
        } else {
            return Err(invalid(
                "store_media requires `path` or `bytes_b64`".to_string(),
            ));
        };
        let id = echo_defs::media_store::save_image_bytes_in(&self.media_dir, &bytes, mime)
            .map_err(|e| ("store_failed".to_string(), e))?;
        Ok(json!({"ref": echo_defs::media_store::media_ref(&id)}))
    }

    /// 注册一个远程适配器（宿主侧组合根在插件启动后调用）。
    pub fn register(
        &self,
        instance: Arc<RemoteInstance>,
    ) -> tokio::sync::mpsc::Receiver<IncomingMessage> {
        let (tx, rx) = tokio::sync::mpsc::channel(INBOUND_QUEUE_CAPACITY);
        let name = instance.name.clone();
        self.instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.clone(), instance);
        self.inbound_queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name, tx);
        rx
    }

    /// 取一个远程适配器（命令域 / 面板通道用）。
    pub fn get(&self, name: &str) -> Option<Arc<RemoteInstance>> {
        self.instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// 全部已注册远程适配器（`StartedPlugins::instances` 汇总用）。
    pub fn instances(&self) -> Vec<Arc<RemoteInstance>> {
        self.instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }
}

impl Default for CoreApiService {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::host_service::HostService for CoreApiService {
    fn call(&self, method: &str, payload: Value) -> Result<Value, (String, String)> {
        match method {
            "submit_input" => {
                let message: IncomingMessage =
                    serde_json::from_value(payload.get("message").cloned().unwrap_or(Value::Null))
                        .map_err(|e| {
                            (
                                "invalid_payload".to_string(),
                                format!("submit_input payload: {e}"),
                            )
                        })?;
                let instance_name = message.instance.clone();
                let Some(tx) = self
                    .inbound_queues
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&instance_name)
                    .cloned()
                else {
                    return Err((
                        "adapter_not_found".to_string(),
                        format!("adapter {instance_name} not registered"),
                    ));
                };
                // 有界队列：满 → backpressure（插件退避 / 落盘重试）。
                match tx.try_send(message) {
                    Ok(()) => Ok(json!({"accepted": true})),
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Err((
                        "backpressure".to_string(),
                        format!("adapter {instance_name} inbound queue full"),
                    )),
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err((
                        "adapter_stopped".to_string(),
                        format!("adapter {instance_name} inbound worker stopped"),
                    )),
                }
            }
            "report_state" => {
                let instance_name = payload["adapter"].as_str().unwrap_or("").to_string();
                let connected = payload["connected"].as_bool().unwrap_or(false);
                let self_id = payload["self_id"].as_str().map(str::to_string);
                let Some(instance) = self.get(&instance_name) else {
                    return Err((
                        "adapter_not_found".to_string(),
                        format!("adapter {instance_name} not registered"),
                    ));
                };
                instance.note_connection_state(connected, self_id.clone());
                self.broadcast_state(InstanceStateChange {
                    instance: instance_name,
                    connected,
                    self_id,
                });
                Ok(json!({}))
            }
            "report_stopped" | "report_error" => {
                // 生命周期 / 错误上报：连接态置未连接（状态文档由插件另行推送）。
                let instance_name = payload["adapter"].as_str().unwrap_or("").to_string();
                let Some(instance) = self.get(&instance_name) else {
                    return Err((
                        "adapter_not_found".to_string(),
                        format!("adapter {instance_name} not registered"),
                    ));
                };
                instance.note_connection_state(false, None);
                // 泛化 01a §1.3：`report_stopped` = 实例已停——管理面状态
                // 不再代表运行实例，显式失效（等插件再次推送恢复）。
                // `report_error`（连接层错误）保留缓存（插件可能仍在运行）。
                if method == "report_stopped" {
                    instance.invalidate_management_state();
                }
                self.broadcast_state(InstanceStateChange {
                    instance: instance_name,
                    connected: false,
                    self_id: None,
                });
                Ok(json!({}))
            }
            "report_ui_state" => {
                let instance_name = payload["adapter"].as_str().unwrap_or("").to_string();
                let state = payload["state"].clone();
                let Some(instance) = self.get(&instance_name) else {
                    return Err((
                        "adapter_not_found".to_string(),
                        format!("adapter {instance_name} not registered"),
                    ));
                };
                instance.note_management_state(state.clone());
                // 泛化 01a §1.2：上报即广播（组合根转 `InstanceState` +
                // 派生旧事件；宿主缓存刷新与推送同点完成）。
                // 注：插件 payload 现阶段**不带 epoch**——无 epoch 不校验
                //（epoch 校验待协议扩展；现状由 `RemoteInstance` 读时按
                //  generation 惰性失效兜底，见 01a §1.3）。
                self.broadcast_ui_state(InstanceUiStateChange {
                    instance: instance_name,
                    state,
                });
                Ok(json!({}))
            }
            "store_media" => self.store_media(&payload),
            other => Err((
                "method_not_found".to_string(),
                format!("unknown core_api method: {other}"),
            )),
        }
    }
}

/// 线协议入站消息（`IncomingMessage`）→ 宿主词汇（`echo_defs::chat::IncomingMessage`）。
///
/// 组合根在入站 worker 里用本函数把插件的 `submit_input` 载荷转成
/// `AgentMessageHook` 可直接消费的类型（字段差异：`instance`→`instance_name`、
/// `Dm`→`Direct`、`user_name` 缺省空串、`files` 由 JSON 值解析为
/// [`echo_defs::chat::IncomingFile`]；无法解析的文件条目跳过并保序）。
pub fn message_to_incoming(message: IncomingMessage) -> echo_defs::chat::IncomingMessage {
    use echo_defs::chat::{ChannelType, IncomingFile, IncomingMessage};

    let channel = match message.channel {
        crate::api::ChannelType::Dm => ChannelType::Direct,
        crate::api::ChannelType::Group { group_id } => ChannelType::Group { group_id },
    };
    let files: Vec<IncomingFile> = message
        .files
        .iter()
        .filter_map(|value| {
            let name = value.get("name").and_then(Value::as_str)?.to_string();
            Some(IncomingFile {
                name,
                path: value
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                size: value.get("size").and_then(Value::as_u64).unwrap_or(0),
                error: value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect();

    IncomingMessage {
        instance_name: message.instance,
        platform: message.platform,
        user_id: message.user_id,
        user_name: message.user_name.unwrap_or_default(),
        channel,
        group_name: message.group_name,
        content: message.content,
        timestamp: message.timestamp,
        at_me: message.at_me,
        metadata: message.metadata,
        images: message.images,
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        capabilities, ChannelType, HostToPlugin, IncomingMessage, InstanceCapabilities,
        InstanceContribution, PluginToHost, Register, Welcome,
    };
    use crate::host_service::HostService as _;
    use crate::inproc::{InprocIo, InprocPlugin, InprocTransport};
    use crate::supervisor::PluginSupervisor;
    use crate::transport::PluginSpec;
    use echo_defs::plugin_instance::{InstanceManagement as _, PluginInstance as _};
    use serde_json::json;
    use std::future::Future;
    use std::pin::Pin;

    fn spec() -> PluginSpec {
        PluginSpec {
            plugin_id: "adapter-plugin".to_string(),
            config: json!({}),
        }
    }

    fn contribution() -> InstanceContribution {
        InstanceContribution {
            name: "qq".into(),
            platform: "qq".into(),
            display_name: "QQ / OneBot".into(),
            capabilities: InstanceCapabilities {
                send_text: true,
                send_media: false,
                groups: true,
                friends: false,
                gating: true,
                login: true,
            },
            config_schema: None,
            ui: None,
            session_labels: None,
            tools: Vec::new(),
        }
    }

    /// 注册适配器贡献的 loopback 测试插件。
    struct InstancePlugin;

    impl InprocPlugin for InstancePlugin {
        fn run(self: Box<Self>, mut io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(async move {
                loop {
                    let Some(msg) = io.recv().await else {
                        return;
                    };
                    match msg {
                        HostToPlugin::Hello(hello) => {
                            let _ = io
                                .send(PluginToHost::Welcome(Welcome {
                                    protocol: hello.protocol.clone(),
                                    version: hello.version,
                                    plugin_id: hello.plugin_id.clone(),
                                    capabilities: vec![capabilities::ADAPTER.to_string()],
                                }))
                                .await;
                            let _ = io
                                .send(PluginToHost::Register(Register {
                                    contributions: vec![crate::api::Contribution::PluginInstance(
                                        contribution(),
                                    )],
                                }))
                                .await;
                            let _ = io.send(PluginToHost::Ready).await;
                        }
                        HostToPlugin::Invoke(inv) => {
                            // send_text 回一个 message_id；custom 直通 echo 参数。
                            let result = if inv.contribution == "qq:send_text" {
                                json!({"message_id": "msg-1", "success": true})
                            } else if inv.contribution == "qq:custom" {
                                json!({"echoed": inv.payload["payload"].clone()})
                            } else {
                                json!({})
                            };
                            let _ = io
                                .send(PluginToHost::InvokeResult(crate::api::InvokeResult {
                                    call_id: inv.call_id,
                                    outcome: crate::api::InvokeOutcome::Ok {
                                        text: String::new(),
                                        images: vec![],
                                        result,
                                    },
                                }))
                                .await;
                        }
                        HostToPlugin::Drain(_) => return,
                        HostToPlugin::Dispose => return,
                        _ => {}
                    }
                }
            })
        }
    }

    /// RemoteInstance：能力位 fail-closed + 状态仲裁（进程态 ∧ 连接态）。
    #[tokio::test]
    async fn remote_instance_capability_gates_and_state_arbitration() {
        let supervisor = PluginSupervisor::new();
        let handle = supervisor
            .start(
                spec(),
                Box::new(InprocTransport::new(|_| Box::new(InstancePlugin))),
            )
            .await
            .expect("handshake ok");
        let instances: Vec<_> = handle
            .contributions()
            .into_iter()
            .filter_map(|c| match c {
                crate::api::Contribution::PluginInstance(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(instances.len(), 1);
        let instance = RemoteInstance::new(instances[0].clone(), handle.clone());

        // 状态仲裁：插件 Ready 时显示缓存连接态；Pending/Failed 时压住。
        instance.note_connection_state(true, Some("4242".into()));
        let info = instance.status_info();
        assert_eq!(info.name, "qq");
        assert_eq!(info.self_id.as_deref(), Some("4242"));

        // 管理面：invoke_custom 直通 custom op；cached_state 读缓存。
        //
        // **必须用 `instance.management()`**（宿主消费方走的就是这条路径），
        // 而不是自建 `RemoteInstanceManagement::new(instance.clone())`——后者
        // 永远可用，会掩盖「宿主字段里的弱引用没接对」这种致命 bug
        // （历史上 `RemoteInstance::new` 用 `Weak::new()` 占位，导致
        // `management()` 恒失效、全部管理动作返回 "has been destroyed"）。
        let management_ref = instance
            .management()
            .expect("management 必须可用（宿主字段的弱引用指向自身）");
        assert!(
            management_ref.cached_state().is_none(),
            "cached_state 读的是本结构缓存，未写入时应为 None 而不是报错"
        );
        let management = RemoteInstanceManagement::new(instance.clone());
        assert!(management.cached_state().is_none());
        instance.note_management_state(json!({"gate": {"mode": "allowlist"}}));
        assert_eq!(
            management.cached_state().unwrap()["gate"]["mode"],
            json!("allowlist")
        );
        // 同一断言走宿主消费路径（`instance.management()`）：读得到缓存。
        assert_eq!(
            instance
                .management()
                .expect("management 可用")
                .cached_state()
                .expect("缓存已写入")["gate"]["mode"],
            json!("allowlist")
        );
        let out = management
            .invoke_custom("probe", json!({"method": "probe", "payload": {"x": 1}}))
            .await
            .expect("custom ok");
        // 线格式：`qq:custom` 的载荷 = {"method": "probe", "payload": {"x": 1}}，
        // 插件侧 echoed 内层 payload——断言按实际线格式对齐。
        assert_eq!(out["echoed"]["payload"]["x"], json!(1));

        handle
            .drain(std::time::Duration::from_secs(2))
            .await
            .expect("drain ok");
    }

    /// 泛化 Core-1（01a S2）：`RemoteInstance` 的 trait 访问器把贡献里的
    /// `ui` / `session_labels` 原样返回；未声明时为 None。
    #[tokio::test]
    async fn remote_instance_exposes_ui_descriptor_and_session_labels() {
        let supervisor = PluginSupervisor::new();
        let handle = supervisor
            .start(
                spec(),
                Box::new(InprocTransport::new(|_| Box::new(InstancePlugin))),
            )
            .await
            .expect("handshake ok");

        // 带 ui / session_labels 的贡献 → 访问器返回同值。
        let mut with_ui = contribution();
        with_ui.ui = Some(json!({"version": 1, "panels": []}));
        with_ui.session_labels = Some(json!({"dm": "私聊", "group": "群聊"}));
        let instance: Arc<RemoteInstance> = RemoteInstance::new(with_ui, handle.clone());
        assert_eq!(
            instance.ui_descriptor(),
            Some(json!({"version": 1, "panels": []}))
        );
        assert_eq!(
            instance.session_labels(),
            Some(json!({"dm": "私聊", "group": "群聊"}))
        );

        // 未声明的贡献（fixture 默认 None）→ 默认 None。
        let bare: Arc<RemoteInstance> = RemoteInstance::new(contribution(), handle.clone());
        assert_eq!(bare.ui_descriptor(), None);
        assert_eq!(bare.session_labels(), None);

        handle
            .drain(std::time::Duration::from_secs(2))
            .await
            .expect("drain ok");
    }

    /// CoreApiService：submit_input 入队（反压）+ report_* 刷新缓存。
    #[tokio::test]
    async fn core_api_service_inbound_queue_and_reports() {
        let supervisor = PluginSupervisor::new();
        let handle = supervisor
            .start(
                spec(),
                Box::new(InprocTransport::new(|_| Box::new(InstancePlugin))),
            )
            .await
            .expect("handshake ok");
        let instances: Vec<_> = handle
            .contributions()
            .into_iter()
            .filter_map(|c| match c {
                crate::api::Contribution::PluginInstance(a) => Some(a),
                _ => None,
            })
            .collect();
        let instance = RemoteInstance::new(instances[0].clone(), handle.clone());

        let service = CoreApiService::new();
        let mut rx = service.register(instance.clone());

        // submit_input：入队回执 accepted。
        let message = IncomingMessage {
            instance: "qq".into(),
            platform: "qq".into(),
            user_id: "123".into(),
            user_name: Some("Alice".into()),
            channel: ChannelType::Dm,
            group_name: None,
            content: "hello".into(),
            timestamp: 1,
            at_me: false,
            metadata: json!({}),
            images: vec![],
            files: vec![],
        };
        let outcome = service
            .call(
                "submit_input",
                json!({"message": serde_json::to_value(&message).unwrap()}),
            )
            .expect("deliver accepted");
        assert_eq!(outcome["accepted"], json!(true));
        // 队列收到消息。
        let received = rx.recv().await.expect("queue message");
        assert_eq!(received.content, "hello");

        // report_state：连接态刷新 + 订阅者广播。
        let mut states = service.subscribe_states();
        service
            .call(
                "report_state",
                json!({"adapter": "qq", "connected": true, "self_id": "4242"}),
            )
            .expect("report ok");
        let info = instance.status_info();
        assert_eq!(info.self_id.as_deref(), Some("4242"));
        assert_eq!(
            states.try_recv().expect("state broadcast"),
            InstanceStateChange {
                instance: "qq".into(),
                connected: true,
                self_id: Some("4242".into()),
            }
        );

        // report_error：断开语义广播（connected=false）。
        service
            .call("report_error", json!({"adapter": "qq", "message": "boom"}))
            .expect("report error ok");
        assert_eq!(
            states.try_recv().expect("stopped broadcast"),
            InstanceStateChange {
                instance: "qq".into(),
                connected: false,
                self_id: None,
            }
        );

        // report_ui_state：管理面状态缓存刷新 + UI 状态广播（泛化 01a
        // §1.2 S4：订阅者收到 InstanceUiStateChange，组合根据此转 InstanceState）。
        let mut ui_states = service.subscribe_ui_states();
        service
            .call(
                "report_ui_state",
                json!({"adapter": "qq", "state": {"gate": {"mode": "denylist"}}}),
            )
            .expect("ui state ok");
        assert_eq!(
            instance.management_state().unwrap()["gate"]["mode"],
            json!("denylist")
        );
        assert_eq!(
            ui_states.try_recv().expect("ui state broadcast"),
            InstanceUiStateChange {
                instance: "qq".into(),
                state: json!({"gate": {"mode": "denylist"}}),
            }
        );

        // 未注册实例：可读错误（instance_not_found）。
        let err = service
            .call(
                "submit_input",
                json!({"message": serde_json::to_value(&IncomingMessage {
                    instance: "nope".into(),
                    platform: "qq".into(),
                    user_id: "1".into(),
                    user_name: None,
                    channel: ChannelType::Dm,
                    group_name: None,
                    content: "x".into(),
                    timestamp: 1,
                    at_me: false,
                    metadata: json!({}),
                    images: vec![],
                    files: vec![],
                }).unwrap()}),
            )
            .expect_err("unregistered adapter must fail");
        assert_eq!(err.0, "adapter_not_found");

        handle
            .drain(std::time::Duration::from_secs(2))
            .await
            .expect("drain ok");
    }

    /// store_media：bytes_b64 / path 两种形态落盘、内容寻址去重、缺参与超限拒绝。
    #[test]
    fn core_api_store_media() {
        use base64::Engine as _;
        let dir = std::env::temp_dir().join(format!("echo-store-media-{}", std::process::id()));
        let service = CoreApiService::with_media_dir(dir.clone());

        let png = [0x89u8, b'P', b'N', b'G', 1, 2, 3];
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        let by_bytes = service
            .call(
                "store_media",
                json!({"bytes_b64": b64, "mime": "image/png"}),
            )
            .expect("bytes ok");
        let reference = by_bytes["ref"].as_str().unwrap().to_string();
        assert!(
            reference.starts_with("/media/") && reference.ends_with(".png"),
            "{reference}"
        );
        let id = echo_defs::media_store::id_of_ref(&reference).unwrap();
        assert_eq!(std::fs::read(dir.join(id)).unwrap(), png);

        // path 形态：同内容 → 同一引用。
        let src = dir.join("src.bin");
        std::fs::write(&src, png).unwrap();
        let by_path = service
            .call(
                "store_media",
                json!({"path": src.to_str().unwrap(), "mime": "image/png"}),
            )
            .expect("path ok");
        assert_eq!(by_path["ref"], json!(reference));

        let err = service.call("store_media", json!({})).unwrap_err();
        assert_eq!(err.0, "invalid_payload");
        let err = service
            .call(
                "store_media",
                json!({"path": dir.join("missing").to_str().unwrap()}),
            )
            .unwrap_err();
        assert_eq!(err.0, "invalid_payload");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `message_to_incoming`：线协议字段逐项映射（含 files JSON → IncomingFile）。
    #[test]
    fn message_to_incoming_maps_fields() {
        let message = IncomingMessage {
            instance: "qq-two".into(),
            platform: "qq".into(),
            user_id: "123".into(),
            user_name: Some("Alice".into()),
            channel: ChannelType::Group {
                group_id: "456".into(),
            },
            group_name: Some("开发群".into()),
            content: "hello".into(),
            timestamp: 1733814154,
            at_me: true,
            metadata: json!({"message_id": 7}),
            images: vec!["/media/ab12.png".into()],
            files: vec![
                json!({"name": "report.pdf", "path": "/tmp/report.pdf", "size": 42, "error": null}),
                json!({"name": "missing.bin", "path": null, "size": 0, "error": "download failed"}),
                json!({"no_name": true}),
            ],
        };
        let incoming = message_to_incoming(message);
        assert_eq!(incoming.instance_name, "qq-two");
        assert_eq!(incoming.platform, "qq");
        assert_eq!(incoming.user_id, "123");
        assert_eq!(incoming.user_name, "Alice");
        assert!(matches!(
            incoming.channel,
            echo_defs::chat::ChannelType::Group { ref group_id } if group_id == "456"
        ));
        assert_eq!(incoming.group_name.as_deref(), Some("开发群"));
        assert_eq!(incoming.content, "hello");
        assert_eq!(incoming.timestamp, 1733814154);
        assert!(incoming.at_me);
        assert_eq!(incoming.metadata, json!({"message_id": 7}));
        assert_eq!(incoming.images, vec!["/media/ab12.png".to_string()]);
        // 无 name 的条目被跳过（保序）。
        assert_eq!(incoming.files.len(), 2);
        assert_eq!(incoming.files[0].name, "report.pdf");
        assert_eq!(incoming.files[0].path.as_deref(), Some("/tmp/report.pdf"));
        assert_eq!(incoming.files[0].size, 42);
        assert_eq!(incoming.files[1].error.as_deref(), Some("download failed"));

        // DM + 缺省 user_name → 空串（hook 侧按空名兜底）。
        let dm = IncomingMessage {
            instance: "qq".into(),
            platform: "qq".into(),
            user_id: "1".into(),
            user_name: None,
            channel: ChannelType::Dm,
            group_name: None,
            content: "x".into(),
            timestamp: 0,
            at_me: false,
            metadata: Value::Null,
            images: vec![],
            files: vec![],
        };
        let incoming = message_to_incoming(dm);
        assert!(matches!(
            incoming.channel,
            echo_defs::chat::ChannelType::Direct
        ));
        assert_eq!(incoming.user_name, "");
    }

    // ── 泛化 Core-2 S5（01a §1.3）：generation epoch 失效 ──────────────

    /// 崩溃一次插件：握手完成后立刻退出（触发监督自动重启）；重启实例
    /// （第二次 start）保持存活，正常响应 Drain。
    struct CrashOncePlugin {
        crashed: Arc<std::sync::atomic::AtomicBool>,
    }

    impl InprocPlugin for CrashOncePlugin {
        fn run(self: Box<Self>, mut io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(async move {
                let crash_now = !self.crashed.swap(true, std::sync::atomic::Ordering::SeqCst);
                loop {
                    let Some(msg) = io.recv().await else {
                        return;
                    };
                    match msg {
                        HostToPlugin::Hello(hello) => {
                            let _ = io
                                .send(PluginToHost::Welcome(Welcome {
                                    protocol: hello.protocol.clone(),
                                    version: hello.version,
                                    plugin_id: hello.plugin_id.clone(),
                                    capabilities: vec![capabilities::ADAPTER.to_string()],
                                }))
                                .await;
                            let _ = io
                                .send(PluginToHost::Register(Register {
                                    contributions: vec![crate::api::Contribution::PluginInstance(
                                        contribution(),
                                    )],
                                }))
                                .await;
                            let _ = io.send(PluginToHost::Ready).await;
                            if crash_now {
                                // 首实例：就绪后立刻退出 = 崩溃（监督自动重启）。
                                return;
                            }
                        }
                        HostToPlugin::Drain(_) => return,
                        _ => {}
                    }
                }
            })
        }
    }

    /// 从句柄取第一个适配器贡献并构造 `RemoteInstance`。
    fn remote_instance_from(handle: &PluginHandle) -> Arc<RemoteInstance> {
        let instances: Vec<_> = handle
            .contributions()
            .into_iter()
            .filter_map(|c| match c {
                crate::api::Contribution::PluginInstance(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(instances.len(), 1, "fixture 应注册一个适配器贡献");
        RemoteInstance::new(instances[0].clone(), handle.clone())
    }

    /// 泛化 S5：崩溃重启换代（`set_conn(Some(新连接))`）后管理面缓存惰性
    /// 失效（`management_state() == None`）；插件再次推送即以新代际恢复。
    ///
    /// 取舍（提交说明）：`cached_state()` 同步签名不能 await，读路径不做
    /// `invoke_custom("get_state")` 兜底——退化为「等下一次插件推送恢复」
    /// （QQ 插件 on_ready 必推初始快照；`get_state` 拉取路径在
    /// echo-agent 侧，见 `InvokeInstanceOp` 臂）。
    #[tokio::test]
    async fn management_state_epoch_invalidates_across_restart() {
        let crashed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let supervisor =
            PluginSupervisor::new().with_restart_policy(crate::supervisor::RestartPolicy {
                initial_backoff: std::time::Duration::from_millis(10),
                ..Default::default()
            });
        let handle = supervisor
            .start(
                spec(),
                Box::new(InprocTransport::new({
                    let crashed = crashed.clone();
                    move |_| {
                        Box::new(CrashOncePlugin {
                            crashed: crashed.clone(),
                        })
                    }
                })),
            )
            .await
            .expect("handshake ok");
        let instance = remote_instance_from(&handle);
        let service = CoreApiService::new();
        service.register(instance.clone());

        // 首代推送一次 → 缓存就位（初始启动 = 第 1 代）。
        let initial_generation = handle.generation();
        assert!(initial_generation >= 1, "初始启动应已自增 generation");
        service
            .call(
                "report_ui_state",
                json!({"adapter": "qq", "state": {"gate": {"mode": "allowlist"}}}),
            )
            .expect("ui state ok");
        assert_eq!(
            instance.management_state().unwrap()["gate"]["mode"],
            json!("allowlist")
        );

        // 等监督重启完成（新连接 = 新一代，state 回到 Ready）。
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if handle.generation() > initial_generation && handle.state() == PluginState::Ready {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "restart wait timed out; state={:?}, generation={}",
                handle.state(),
                handle.generation()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // 换代后缓存读时比对 generation → 失效（惰性，不清陈旧值）。
        assert_eq!(
            instance.management_state(),
            None,
            "崩溃重启换代后管理面缓存应失效"
        );

        // 插件重新推送一次 → 以新代际恢复。
        service
            .call(
                "report_ui_state",
                json!({"adapter": "qq", "state": {"gate": {"mode": "none"}}}),
            )
            .expect("ui state ok");
        assert_eq!(
            instance.management_state().unwrap()["gate"]["mode"],
            json!("none")
        );

        // `report_stopped`：显式失效（重启 / 停止后缓存不再可信）。
        service
            .call("report_stopped", json!({"adapter": "qq"}))
            .expect("report stopped ok");
        assert_eq!(
            instance.management_state(),
            None,
            "report_stopped 应显式失效管理面缓存"
        );

        handle
            .drain(std::time::Duration::from_secs(2))
            .await
            .expect("drain ok");
    }

    /// 泛化 S5：热替换换代（升级换连接的 `set_conn(Some)` 路径）同样自增
    /// generation 并使管理面缓存失效；重新推送恢复。
    #[tokio::test]
    async fn management_state_epoch_invalidates_after_upgrade() {
        let supervisor = PluginSupervisor::new();
        let handle = supervisor
            .start(
                spec(),
                Box::new(InprocTransport::new(|_| Box::new(InstancePlugin))),
            )
            .await
            .expect("handshake ok");
        let instance = remote_instance_from(&handle);
        let service = CoreApiService::new();
        service.register(instance.clone());

        service
            .call(
                "report_ui_state",
                json!({"adapter": "qq", "state": {"gate": {"mode": "allowlist"}}}),
            )
            .expect("ui state ok");
        assert_eq!(
            instance.management_state().unwrap()["gate"]["mode"],
            json!("allowlist")
        );

        let before = handle.generation();
        handle
            .upgrade(json!({}), std::time::Duration::from_secs(2))
            .await
            .expect("upgrade ok");
        assert!(
            handle.generation() > before,
            "热替换换代应自增 generation（before={before}, after={}）",
            handle.generation()
        );
        assert_eq!(instance.management_state(), None, "换代后缓存应失效");

        service
            .call(
                "report_ui_state",
                json!({"adapter": "qq", "state": {"gate": {"mode": "none"}}}),
            )
            .expect("ui state ok");
        assert_eq!(
            instance.management_state().unwrap()["gate"]["mode"],
            json!("none")
        );

        handle
            .drain(std::time::Duration::from_secs(2))
            .await
            .expect("drain ok");
    }
}
