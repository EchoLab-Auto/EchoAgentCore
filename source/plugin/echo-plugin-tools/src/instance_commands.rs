//! 插件实例（adapter）命令域：7 条 `*Instance*` 命令 + 实例清单投影。
//!
//! ## 为什么这一段归本插件
//!
//! `instance_*` **工具**（`instance.rs`：启动/停止/状态查询）本来就住在这里，
//! 与 `shell_*` 工具 + Shell 命令域同一理由：模型侧与面板侧是同一件事的两个
//! 入口，分居两个 crate 会让"插件实例"这个概念横跨插件边界。
//!
//! 更关键的是**清单投影**（`InstanceList`）：它逐条读
//! `PluginInstance::management().cached_state()` / `ui_descriptor()` /
//! `session_labels()`，把"这个平台有哪些展示字段"翻成线上结构。那是
//! **适配器能力**的知识（2026-10 去适配器化之后，Core 不再逐键命名
//! `container` / `webui_url` / `onebot_url`）——它不该留在宿主。
//!
//! ## 宿主接缝 = 两项能力
//!
//! | 方法 | 宿主实现 |
//! |---|---|
//! | [`InstanceCommandHost::instances`] | `Agent` 的 `Arc<InstanceRegistry>`（进程级同一份） |
//! | [`InstanceCommandHost::emit`] | `Agent::emit`（team 标注 / 事件总线 / 汇聚点） |
//!
//! `InvokeInstanceOp` 不在本域：它是**通用**的实例操作转发（载荷是自由 JSON，
//! 由实例自己的 `invoke_op` 解释），与具体平台无关，且需要宿主的实例桥接
//! （`instance_ops/bridge.rs`）。它留在宿主直到 S3b 把桥接也搬过来。

use std::sync::Arc;

use echo_defs::plugin_instance::{InstanceConnectionState, InstanceRegistry};
use echo_protocol::{BackendCommand, BackendEvent, InstanceMetadataEntry, InstanceStatus};

/// 本插件处理的命令名。
pub const COMMANDS: [&str; 6] = [
    "StartInstance",
    "StopInstance",
    "RestartInstance",
    "RequestInstanceStatus",
    "StartAllInstances",
    "StopAllInstances",
];

/// 宿主接缝：实例命令域需要的两件事。
pub trait InstanceCommandHost: Send + Sync {
    /// 进程级实例注册表（全部 persona 共享同一份——实例是进程资源）。
    fn instances(&self) -> Arc<InstanceRegistry>;
    /// 事件出口。
    fn emit(&self, event: BackendEvent);
}

/// 命令处理器。
pub struct InstanceCommandHandler {
    host: Arc<dyn InstanceCommandHost>,
}

impl InstanceCommandHandler {
    pub fn new(host: Arc<dyn InstanceCommandHost>) -> Self {
        Self { host }
    }

    /// 处理一条命令；`None` = 不是本域（`InvokeInstanceOp` 也不算）。
    pub async fn handle(
        &self,
        name: &str,
        payload: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        if !COMMANDS.contains(&name) {
            return None;
        }
        let tagged = serde_json::json!({ name: payload });
        let cmd: BackendCommand = match serde_json::from_value(tagged) {
            Ok(cmd) => cmd,
            Err(error) => return Some(Err(format!("instance 命令载荷解析失败（{name}）: {error}"))),
        };
        Some(self.apply(cmd).await.map(|()| serde_json::Value::Null))
    }

    async fn apply(&self, cmd: BackendCommand) -> Result<(), String> {
        let instances = self.host.instances();
        match cmd {
            BackendCommand::StartInstance { name } => {
                self.one(&instances, &name, OneShot::Start).await;
                self.emit_list(&instances);
                Ok(())
            }
            BackendCommand::StopInstance { name } => {
                self.one(&instances, &name, OneShot::Stop).await;
                self.emit_list(&instances);
                Ok(())
            }
            BackendCommand::RestartInstance { name } => {
                self.one(&instances, &name, OneShot::Restart).await;
                self.emit_list(&instances);
                Ok(())
            }
            BackendCommand::RequestInstanceStatus => {
                self.emit_list(&instances);
                Ok(())
            }
            BackendCommand::StartAllInstances => {
                for (name, result) in instances.start_all().await {
                    match result {
                        Ok(()) => self.notice(format!("adapter {name} started")),
                        Err(error) => self.notice(format!("adapter {name}: {error}")),
                    }
                }
                self.emit_list(&instances);
                Ok(())
            }
            BackendCommand::StopAllInstances => {
                instances.stop_all().await;
                self.notice("all adapters stopped");
                self.emit_list(&instances);
                Ok(())
            }
            other => Err(format!(
                "instance 命令域声明了 {} 但处理器没有对应分支",
                other.kind_name()
            )),
        }
    }

    /// 单实例的三个动作（start / stop / restart）。
    ///
    /// 抽成一个函数是因为三条命令的**差异只有动作本身**：找不到实例、失败
    /// 文案、清单广播三件事完全同构。分开写时三处各有一份 `not found` 文案，
    /// 改一处忘两处是时间问题。
    async fn one(&self, instances: &Arc<InstanceRegistry>, name: &str, action: OneShot) {
        let Some(instance) = instances.get(name) else {
            self.notice(format!("adapter {name} not found"));
            return;
        };
        let display = instance.display_name().to_string();
        match action {
            OneShot::Start => match instance.start().await {
                Ok(()) => self.notice(format!("adapter {display} started")),
                Err(error) => self.notice(format!("adapter {display} start failed: {error}")),
            },
            OneShot::Stop => match instance.stop().await {
                Ok(()) => self.notice(format!("adapter {display} stopped")),
                Err(error) => self.notice(format!("adapter {display} stop failed: {error}")),
            },
            OneShot::Restart => match instance.stop().await {
                Ok(()) => match instance.start().await {
                    Ok(()) => self.notice(format!("adapter {display} restarted")),
                    Err(error) => {
                        self.notice(format!("adapter {display} restart (start) failed: {error}"))
                    }
                },
                Err(error) => {
                    self.notice(format!("adapter {display} restart (stop) failed: {error}"))
                }
            },
        }
    }

    /// 广播实例清单（投影见 [`instance_list`]）。
    fn emit_list(&self, instances: &Arc<InstanceRegistry>) {
        self.host
            .emit(BackendEvent::InstanceList {
                instances: instance_list(instances),
            });
    }

    fn notice(&self, message: impl Into<String>) {
        self.host.emit(BackendEvent::Error {
            session_id: None,
            message: message.into(),
        });
    }
}

/// 实例清单投影（`InstanceList` 的线上形状）——**本 crate 是它的唯一作者**。
///
/// 逐条读注册表 + 实例自报的访问器。**平台自有展示字段由插件自己描述**
/// （UI 描述符里的 `bind` 指向 `instance.*`，Panel 通用渲染器据此取值），
/// 所以 `metadata` 恒为空——Core 不再逐键命名平台的展示 schema。
///
/// 抽成自由函数而不是 handler 的私有方法：宿主还有一处需要它
/// （`RequestState` 启动快照里带上实例状态），那处不必为此造一个 handler。
pub fn instance_list(instances: &Arc<InstanceRegistry>) -> Vec<InstanceStatus> {
    instances
        .list_info()
            .into_iter()
            .map(|info| {
                let handle = instances.get(&info.name);
                // 实例附加信息（persona / 容器 / URL / 绑定地址）经管理面
                // `cached_state` 取（W3.4 单源化：与 `InstanceState` 同一份
                // 状态文档；缺管理面 / 无状态时留空，线上字段本就 Option）。
                let instance = handle
                    .as_ref()
                    .and_then(|a| a.management().and_then(|m| m.cached_state()))
                    .map(|state| state["instance"].clone())
                    .unwrap_or_default();
                InstanceStatus {
                    bind_address: instance["bind_address"].as_str().unwrap_or("").to_string(),
                    persona: instance["persona"].as_str().map(str::to_string),
                    connected: matches!(info.status, InstanceConnectionState::Connected),
                    running: !matches!(info.status, InstanceConnectionState::Stopped),
                    self_id: info.self_id,
                    started_at: info.started_at,
                    ui: handle.as_ref().and_then(|a| a.ui_descriptor()),
                    session_labels: handle.as_ref().and_then(|a| a.session_labels()),
                    metadata: Vec::<InstanceMetadataEntry>::new(),
                    display_name: info.display_name,
                    name: info.name,
                    platform: Some(info.platform),
                }
        })
        .collect()
}

/// 单实例动作（三条命令的唯一差异）。
#[derive(Clone, Copy)]
enum OneShot {
    Start,
    Stop,
    Restart,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeHost {
        instances: Arc<InstanceRegistry>,
        events: Mutex<Vec<BackendEvent>>,
    }

    impl InstanceCommandHost for FakeHost {
        fn instances(&self) -> Arc<InstanceRegistry> {
            Arc::clone(&self.instances)
        }
        fn emit(&self, event: BackendEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn handler() -> (InstanceCommandHandler, Arc<FakeHost>) {
        let host = Arc::new(FakeHost {
            instances: Arc::new(InstanceRegistry::new()),
            events: Mutex::new(Vec::new()),
        });
        (InstanceCommandHandler::new(host.clone()), host)
    }

    /// 空注册表：状态查询回**空清单**（面板显示"没有实例"，不是报错）。
    #[tokio::test]
    async fn status_with_no_instances_emits_an_empty_list() {
        let (h, host) = handler();
        h.handle("RequestInstanceStatus", serde_json::Value::Null)
            .await
            .expect("本域命令")
            .expect("空清单不是错误");
        let events = host.events.lock().unwrap();
        assert!(matches!(
            events.as_slice(),
            [BackendEvent::InstanceList { instances }] if instances.is_empty()
        ));
    }

    /// 找不到实例：明确 `not found`（面板点了按钮必须收到回应）。
    #[tokio::test]
    async fn unknown_instance_reports_not_found() {
        let (h, host) = handler();
        h.handle("StartInstance", serde_json::json!({"name": "ghost"}))
            .await
            .expect("本域命令")
            .expect("解析成功");
        let events = host.events.lock().unwrap();
        match events.first() {
            Some(BackendEvent::Error { message, .. }) => {
                assert!(message.contains("ghost") && message.contains("not found"), "{message}")
            }
            other => panic!("期望 not found 错误，得到 {other:?}"),
        }
    }

    /// `InvokeInstanceOp` **不**被认领（通用操作转发留在宿主）。
    #[tokio::test]
    async fn invoke_instance_op_is_not_claimed() {
        let (h, _host) = handler();
        assert!(h
            .handle("InvokeInstanceOp", serde_json::json!({}))
            .await
            .is_none());
    }

    /// 全量启停后一定广播清单（面板据此刷新）。
    #[tokio::test]
    async fn bulk_actions_always_broadcast_the_list() {
        let (h, host) = handler();
        h.handle("StopAllInstances", serde_json::Value::Null)
            .await
            .expect("本域命令")
            .expect("ok");
        let events = host.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BackendEvent::InstanceList { .. })),
            "全量停止后必须广播清单"
        );
    }
}
