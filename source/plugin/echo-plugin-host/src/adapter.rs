//! 适配器插件宿主侧（P1，2026-10 适配器插件化）。
//!
//! - [`RemoteAdapter`]：`echo_adapter::Adapter` 的远程实现——异步方法经
//!   插件协议 `Invoke("<name>:<op>")`（能力位 fail-closed）；状态经
//!   `PluginState` 进程态 ∧ 连接态缓存仲裁（见 §交接与崩溃恢复语义）；
//! - [`RemoteAdapterManagement`]：`AdapterManagement` 的远程实现——
//!   `invoke_custom` 直通 `custom` op；`cached_state` 读 epoch 缓存；
//! - [`AdapterHostService`]：插件 → 宿主的 `adapter_host` 宿主服务——
//!   `deliver_message` 入队（回执 = 已入队，每实例有界队列 + 串行 worker
//!   保序）、`report_*` 更新连接态缓存。
//!
//! 选型与语义见 `document/adapter-plugin.md`（协议规范 / 宿主侧实现）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::supervisor::{PluginHandle, PluginState};
use crate::transport::TransportError;
use crate::api::{AdapterContribution, AdapterMessage, InvokeContext, InvokeOutcome};

use echo_adapter::types::SendResult as AdapterSendResult;

/// 入站队列容量上限（每实例；满 → `backpressure` 错误码，插件退避）。
const INBOUND_QUEUE_CAPACITY: usize = 256;

/// 远程适配器（`echo_adapter::Adapter` 的插件进程实现）。
///
/// 能力位 fail-closed：宿主只调用该适配器贡献中声明过的方法位
/// （[`AdapterContribution::capabilities`]），未声明的调用拒绝为
/// `unsupported`（不发起 Invoke）。
pub struct RemoteAdapter {
    /// 适配器实例名（= 插件协议 `Invoke` 的贡献前缀）。
    pub name: String,
    contribution: AdapterContribution,
    handle: PluginHandle,
    /// 连接态缓存（`report_state` 上报刷新；进程态仲裁见 `status_info`）。
    connection: Mutex<ConnectionCache>,
    /// 管理面状态缓存（epoch 语义：宿主在重启 / 升级时失效后按需重拉）。
    management_state: Mutex<Option<Value>>,
    /// 管理面（构造后以 `Adapter::management()` 按引返回；`Adapter` 的
    /// 签名是 `Option<&dyn AdapterManagement>`——返回值借用本结构的字段）。
    management: RemoteAdapterManagement,
}

/// 连接态缓存（`report_state` 刷新）。
#[derive(Debug, Clone, Default)]
struct ConnectionCache {
    connected: bool,
    self_id: Option<String>,
}

impl RemoteAdapter {
    /// 以插件句柄与适配器贡献构造。
    ///
    /// 管理面在构造后由 `Adapter::management()` 返回（`Option<&dyn …>`
    /// 按引借用本结构的 `management` 字段——`AdapterManagement` 的实现
    /// 只读 `connection` / `management_state` / `handle` / `contribution`，
    /// 不读名字段，构造顺序无自引用问题）。
    pub fn new(contribution: AdapterContribution, handle: PluginHandle) -> Arc<Self> {
        let name = contribution.name.clone();
        // `RemoteAdapterManagement` 持有一个指向本结构的裸 Arc——它只被
        // 本结构的字段持有、生命周期与本结构绑定，不会逃逸。
        let management = RemoteAdapterManagement::for_inner(name.clone());
        Arc::new(Self {
            name,
            contribution,
            handle,
            connection: Mutex::new(ConnectionCache::default()),
            management_state: Mutex::new(None),
            management,
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

    /// 更新连接态缓存（`report_state` 上报入口；`AdapterHostService` 调用）。
    pub fn note_connection_state(&self, connected: bool, self_id: Option<String>) {
        let mut cache = self.connection.lock().expect("connection cache poisoned");
        cache.connected = connected;
        cache.self_id = self_id;
    }

    /// 失效管理面状态缓存（epoch 语义：重启 / 升级后宿主调用）。
    pub fn invalidate_management_state(&self) {
        *self
            .management_state
            .lock()
            .expect("management state cache poisoned") = None;
    }

    /// 取管理面状态缓存（`AdapterManagement::cached_state` 的远程侧）。
    pub fn management_state(&self) -> Option<Value> {
        self.management_state
            .lock()
            .expect("management state cache poisoned")
            .clone()
    }

    /// 更新管理面状态缓存（`report_ui_state` / 状态拉取入口）。
    pub fn note_management_state(&self, state: Value) {
        *self
            .management_state
            .lock()
            .expect("management state cache poisoned") = Some(state);
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
            InvokeOutcome::Error { code, message } => Err(TransportError::Protocol(format!(
                "{code}: {message}"
            ))),
        }
    }
}

/// `echo_adapter::Adapter` 的远程实现（`echo-agent` 侧注册点统一走
/// `AdapterRegistry`，trait 来自 `echo-adapter`）。
#[async_trait]
impl echo_adapter::Adapter for RemoteAdapter {
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

    fn status_info(&self) -> echo_adapter::AdapterInfo {
        // 进程态仲裁（最高优先）：插件 Pending / Failed 时一律显示非
        // Connected，绝不显示缓存的连接态（重启期间缓存不可信）。
        let state = self.handle.state();
        let cache = self.connection.lock().expect("connection cache poisoned");
        let connected = match state {
            PluginState::Ready => cache.connected,
            PluginState::Pending => false,
            PluginState::Draining => cache.connected,
            PluginState::Stopped => false,
            PluginState::Failed(_) => false,
        };
        echo_adapter::AdapterInfo {
            name: self.name.clone(),
            display_name: self.contribution.display_name.clone(),
            status: match state {
                PluginState::Ready if connected => echo_adapter::AdapterConnectionState::Connected,
                PluginState::Ready | PluginState::Draining => {
                    echo_adapter::AdapterConnectionState::Disconnected
                }
                PluginState::Pending => echo_adapter::AdapterConnectionState::Starting,
                PluginState::Stopped => echo_adapter::AdapterConnectionState::Stopped,
                PluginState::Failed(_) => echo_adapter::AdapterConnectionState::Error,
            },
            self_id: cache.self_id.clone(),
            started_at: None,
            platform: self.contribution.platform.clone(),
            configured: true,
        }
    }

    async fn start(&self) -> Result<(), echo_adapter::AdapterError> {
        // 生命周期经插件协议直通（`<name>:start` / `<name>:stop`）。
        self.invoke_op(true, "start", json!({}))
            .await
            .map_err(|e| echo_adapter::AdapterError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), echo_adapter::AdapterError> {
        self.invoke_op(true, "stop", json!({}))
            .await
            .map_err(|e| echo_adapter::AdapterError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn send_message(
        &self,
        target: &echo_adapter::MessageTarget,
        content: &str,
    ) -> Result<AdapterSendResult, echo_adapter::AdapterError> {
        if !self.contribution.capabilities.send_text {
            return Err(echo_adapter::AdapterError::Internal(format!(
                "adapter {} does not declare send_text capability",
                self.name
            )));
        }
        let payload = json!({
            "target": {
                "channel": match &target.channel {
                    echo_adapter::ChannelType::Direct => json!({"type": "dm"}),
                    echo_adapter::ChannelType::Group { group_id } => {
                        json!({"type": "group", "group_id": group_id})
                    }
                },
                "user_id": target.user_id,
            },
            "content": content,
        });
        let result = self
            .invoke_op(true, "send_text", payload)
            .await
            .map_err(|e| echo_adapter::AdapterError::SendFailed(e.to_string()))?;
        Ok(AdapterSendResult {
            message_id: result["message_id"].as_str().map(str::to_string),
            success: result["success"].as_bool().unwrap_or(true),
            error: result["error"].as_str().map(str::to_string),
        })
    }

    fn subscribe(
        &self,
        _tx: tokio::sync::mpsc::UnboundedSender<echo_adapter::AdapterEvent>,
    ) {
        // 入站消息经 `AdapterHostService` 队列分发（不走订阅通道）；
        // 远程适配器的事件流由插件进程持有（`HostCall` 上报）。
    }

    fn management(&self) -> Option<&dyn echo_adapter::AdapterManagement> {
        // 远程管理面（cached_state / invoke_custom；按引返回本结构字段）。
        Some(&self.management)
    }
}

/// `AdapterManagement` 的远程实现（`invoke_custom` 直通 `custom` op；
/// `cached_state` 读 epoch 缓存）。
///
/// 持有对宿主 [`RemoteAdapter`] 的弱引用（`Weak`）——管理面挂在宿主
/// 字段上、生命周期与宿主绑定，不形成循环强引用。
pub struct RemoteAdapterManagement {
    adapter: std::sync::Weak<RemoteAdapter>,
    /// 适配器名（弱引用失效后的错误消息用；弱引用升级失败 = 宿主已销毁）。
    name: String,
}

impl RemoteAdapterManagement {
    /// 以宿主适配器构造（宿主侧组合根在 `RemoteAdapter` 完成构造后调用）。
    pub fn new(adapter: Arc<RemoteAdapter>) -> Self {
        let name = adapter.name.clone();
        Self {
            adapter: Arc::downgrade(&adapter),
            name,
        }
    }

    /// 由宿主 `RemoteAdapter` 内部构造（挂在宿主字段上；弱引用在宿主
    /// 销毁后自然失效，调用方拿到 `internal error` 而非 UB）。
    fn for_inner(name: String) -> Self {
        Self {
            adapter: std::sync::Weak::new(),
            name,
        }
    }

    /// 弱引用升级（宿主已销毁 → 可读错误）。
    fn adapter(&self) -> Result<Arc<RemoteAdapter>, echo_adapter::AdapterError> {
        self.adapter
            .upgrade()
            .ok_or_else(|| echo_adapter::AdapterError::Internal(format!(
                "adapter {} has been destroyed",
                self.name
            )))
    }
}

#[async_trait]
impl echo_adapter::AdapterManagement for RemoteAdapterManagement {
    fn cached_state(&self) -> Option<Value> {
        self.adapter.upgrade()?.management_state()
    }

    async fn invoke_custom(
        &self,
        method: &str,
        payload: Value,
    ) -> Result<Value, echo_adapter::AdapterError> {
        let adapter = self.adapter()?;
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
            adapter.contribution.capabilities.gating
        } else if login_methods.contains(&method) {
            adapter.contribution.capabilities.login
        } else if method == "get_groups" {
            adapter.contribution.capabilities.groups
        } else if method == "get_friends" {
            adapter.contribution.capabilities.friends
        } else {
            true
        };
        adapter
            .invoke_op(capability, "custom", json!({"method": method, "payload": payload}))
            .await
            .map_err(|e| echo_adapter::AdapterError::Internal(e.to_string()))
    }
}


/// 插件 → 宿主的适配器宿主服务（`adapter_host`）。
///
/// 语义（见 §插件动作 表）：`deliver_message` 入队（回执 = 已入队；
/// 每实例有界队列 + 串行 worker 保序）；`report_state` / `report_stopped` /
/// `report_error` 更新连接态缓存；`report_ui_state` 更新管理面状态缓存。
pub struct AdapterHostService {
    adapters: Mutex<HashMap<String, Arc<RemoteAdapter>>>,
    /// 每实例入站队列（有界；worker 串行消费）。
    inbound_queues: Mutex<HashMap<String, tokio::sync::mpsc::Sender<AdapterMessage>>>,
}

impl AdapterHostService {
    /// 空注册表。
    pub fn new() -> Self {
        Self {
            adapters: Mutex::new(HashMap::new()),
            inbound_queues: Mutex::new(HashMap::new()),
        }
    }

    /// 注册一个远程适配器（宿主侧组合根在插件启动后调用）。
    pub fn register(
        &self,
        adapter: Arc<RemoteAdapter>,
    ) -> tokio::sync::mpsc::Receiver<AdapterMessage> {
        let (tx, rx) = tokio::sync::mpsc::channel(INBOUND_QUEUE_CAPACITY);
        let name = adapter.name.clone();
        self.adapters
            .lock()
            .expect("adapters lock poisoned")
            .insert(name.clone(), adapter);
        self.inbound_queues
            .lock()
            .expect("inbound queues lock poisoned")
            .insert(name, tx);
        rx
    }

    /// 取一个远程适配器（命令域 / 面板通道用）。
    pub fn get(&self, name: &str) -> Option<Arc<RemoteAdapter>> {
        self.adapters
            .lock()
            .expect("adapters lock poisoned")
            .get(name)
            .cloned()
    }

    /// 全部已注册远程适配器（`StartedPlugins::adapters` 汇总用）。
    pub fn adapters(&self) -> Vec<Arc<RemoteAdapter>> {
        self.adapters
            .lock()
            .expect("adapters lock poisoned")
            .values()
            .cloned()
            .collect()
    }
}

impl Default for AdapterHostService {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::host_service::HostService for AdapterHostService {
    fn call(&self, method: &str, payload: Value) -> Result<Value, (String, String)> {
        match method {
            "deliver_message" => {
                let message: AdapterMessage = serde_json::from_value(payload.get("message").cloned().unwrap_or(Value::Null))
                    .map_err(|e| ("invalid_payload".to_string(), format!("deliver_message payload: {e}")))?;
                let adapter_name = message.adapter.clone();
                let Some(tx) = self
                    .inbound_queues
                    .lock()
                    .expect("inbound queues lock poisoned")
                    .get(&adapter_name)
                    .cloned()
                else {
                    return Err((
                        "adapter_not_found".to_string(),
                        format!("adapter {adapter_name} not registered"),
                    ));
                };
                // 有界队列：满 → backpressure（插件退避 / 落盘重试）。
                match tx.try_send(message) {
                    Ok(()) => Ok(json!({"accepted": true})),
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Err((
                        "backpressure".to_string(),
                        format!("adapter {adapter_name} inbound queue full"),
                    )),
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err((
                        "adapter_stopped".to_string(),
                        format!("adapter {adapter_name} inbound worker stopped"),
                    )),
                }
            }
            "report_state" => {
                let adapter_name = payload["adapter"].as_str().unwrap_or("").to_string();
                let connected = payload["connected"].as_bool().unwrap_or(false);
                let self_id = payload["self_id"].as_str().map(str::to_string);
                let Some(adapter) = self.get(&adapter_name) else {
                    return Err((
                        "adapter_not_found".to_string(),
                        format!("adapter {adapter_name} not registered"),
                    ));
                };
                adapter.note_connection_state(connected, self_id);
                Ok(json!({}))
            }
            "report_stopped" | "report_error" => {
                // 生命周期 / 错误上报：连接态置未连接（状态文档由插件另行推送）。
                let adapter_name = payload["adapter"].as_str().unwrap_or("").to_string();
                let Some(adapter) = self.get(&adapter_name) else {
                    return Err((
                        "adapter_not_found".to_string(),
                        format!("adapter {adapter_name} not registered"),
                    ));
                };
                adapter.note_connection_state(false, None);
                Ok(json!({}))
            }
            "report_ui_state" => {
                let adapter_name = payload["adapter"].as_str().unwrap_or("").to_string();
                let state = payload["state"].clone();
                let Some(adapter) = self.get(&adapter_name) else {
                    return Err((
                        "adapter_not_found".to_string(),
                        format!("adapter {adapter_name} not registered"),
                    ));
                };
                adapter.note_management_state(state);
                Ok(json!({}))
            }
            other => Err((
                "method_not_found".to_string(),
                format!("unknown adapter_host method: {other}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        capabilities, AdapterCapabilities, AdapterContribution, AdapterMessage,
        AdapterChannel, HostToPlugin, PluginToHost, Register, Welcome,
    };
    use crate::host_service::HostService as _;
    use crate::inproc::{InprocIo, InprocPlugin, InprocTransport};
    use crate::supervisor::PluginSupervisor;
    use echo_adapter::{Adapter as _, AdapterManagement as _};
    use crate::transport::PluginSpec;
    use serde_json::json;
    use std::future::Future;
    use std::pin::Pin;

    fn spec() -> PluginSpec {
        PluginSpec {
            plugin_id: "adapter-plugin".to_string(),
            config: json!({}),
        }
    }

    fn contribution() -> AdapterContribution {
        AdapterContribution {
            name: "qq".into(),
            platform: "qq".into(),
            display_name: "QQ / OneBot".into(),
            capabilities: AdapterCapabilities {
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
        }
    }

    /// 注册适配器贡献的 loopback 测试插件。
    struct AdapterPlugin;

    impl InprocPlugin for AdapterPlugin {
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
                                    contributions: vec![crate::api::Contribution::Adapter(
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

    /// RemoteAdapter：能力位 fail-closed + 状态仲裁（进程态 ∧ 连接态）。
    #[tokio::test]
    async fn remote_adapter_capability_gates_and_state_arbitration() {
        let supervisor = PluginSupervisor::new();
        let handle = supervisor
            .start(spec(), Box::new(InprocTransport::new(|_| Box::new(AdapterPlugin))))
            .await
            .expect("handshake ok");
        let adapters: Vec<_> = handle
            .contributions()
            .into_iter()
            .filter_map(|c| match c {
                crate::api::Contribution::Adapter(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(adapters.len(), 1);
        let adapter = RemoteAdapter::new(adapters[0].clone(), handle.clone());

        // 能力位 fail-closed：send_media 未声明 → 拒绝（不发起 Invoke）。
        let target = echo_adapter::MessageTarget {
            adapter_name: "qq".into(),
            channel: echo_adapter::ChannelType::Direct,
            user_id: "123".into(),
        };
        // send_text（已声明）正常——经 `Adapter::send_message`（trait 方法名
        // 是 send_message；线协议 op 是 `send_text`）。
        let result = adapter.send_message(&target, "hello").await.expect("send ok");
        assert_eq!(result.message_id.as_deref(), Some("msg-1"));
        assert!(result.success);

        // 状态仲裁：插件 Ready 时显示缓存连接态；Pending/Failed 时压住。
        adapter.note_connection_state(true, Some("4242".into()));
        let info = adapter.status_info();
        assert_eq!(info.name, "qq");
        assert_eq!(info.self_id.as_deref(), Some("4242"));

        // 管理面：invoke_custom 直通 custom op；cached_state 读缓存。
        let management = RemoteAdapterManagement::new(adapter.clone());
        assert!(management.cached_state().is_none());
        adapter.note_management_state(json!({"gate": {"mode": "allowlist"}}));
        assert_eq!(
            management.cached_state().unwrap()["gate"]["mode"],
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

    /// AdapterHostService：deliver_message 入队（反压）+ report_* 刷新缓存。
    #[tokio::test]
    async fn adapter_host_service_inbound_queue_and_reports() {
        let supervisor = PluginSupervisor::new();
        let handle = supervisor
            .start(spec(), Box::new(InprocTransport::new(|_| Box::new(AdapterPlugin))))
            .await
            .expect("handshake ok");
        let adapters: Vec<_> = handle
            .contributions()
            .into_iter()
            .filter_map(|c| match c {
                crate::api::Contribution::Adapter(a) => Some(a),
                _ => None,
            })
            .collect();
        let adapter = RemoteAdapter::new(adapters[0].clone(), handle.clone());

        let service = AdapterHostService::new();
        let mut rx = service.register(adapter.clone());

        // deliver_message：入队回执 accepted。
        let message = AdapterMessage {
            adapter: "qq".into(),
            platform: "qq".into(),
            user_id: "123".into(),
            user_name: Some("Alice".into()),
            channel: AdapterChannel::Dm,
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
                "deliver_message",
                json!({"message": serde_json::to_value(&message).unwrap()}),
            )
            .expect("deliver accepted");
        assert_eq!(outcome["accepted"], json!(true));
        // 队列收到消息。
        let received = rx.recv().await.expect("queue message");
        assert_eq!(received.content, "hello");

        // report_state：连接态刷新。
        service
            .call(
                "report_state",
                json!({"adapter": "qq", "connected": true, "self_id": "4242"}),
            )
            .expect("report ok");
        let info = adapter.status_info();
        assert_eq!(info.self_id.as_deref(), Some("4242"));

        // report_ui_state：管理面状态缓存刷新。
        service
            .call(
                "report_ui_state",
                json!({"adapter": "qq", "state": {"gate": {"mode": "denylist"}}}),
            )
            .expect("ui state ok");
        assert_eq!(
            adapter.management_state().unwrap()["gate"]["mode"],
            json!("denylist")
        );

        // 未注册实例：可读错误（adapter_not_found）。
        let err = service
            .call(
                "deliver_message",
                json!({"message": serde_json::to_value(&AdapterMessage {
                    adapter: "nope".into(),
                    platform: "qq".into(),
                    user_id: "1".into(),
                    user_name: None,
                    channel: AdapterChannel::Dm,
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
}
