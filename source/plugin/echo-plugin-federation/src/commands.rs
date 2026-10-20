//! 联邦命令域：7 条 `*Federation*` / `SelfUpdate` / `MigrateSession` 命令。
//!
//! ## 为什么这几条归本插件
//!
//! 它们是**联邦与自更新**的管理面：保存对端、生成邀请串、查询链路状态、
//! 迁移会话、触发自更新。此前它们在宿主 `apply_command` 里是一段
//! "问 `ports.command_handler`，没有就报「联邦未启用」"的内联分支——命令
//! 归属关系写在宿主里，而处理者本来就是联邦自己的出口。
//!
//! ## 实现留在组合根，归属关系在这里
//!
//! 真正的处理（`SaveFederationPeer` 要写配置、`RequestFederationInvite` 要
//! 拿本机监听地址与 node id、`MigrateSession` 要走链路）需要 `Federation`
//! 句柄与 `ConfigStore`——那是组合根的所有物。所以本模块**不做实现**，它做
//! 两件事：
//!
//! 1. **声明归属**：这 7 条命令由本插件处理（`Contribution::Command`）；
//! 2. **兜底语义**：未接线的处理者给可读错误，而不是让面板静默转圈。
//!
//! 处理者经 [`FederationPorts::command_handler`] 取得（类型不引用 `Agent`，
//! 见 [`crate::ports`] 的模块文档）。于是"哪条命令归谁"是注册关系，
//! 而不是宿主里的一段 match。

use std::sync::Arc;

use echo_protocol::{BackendCommand, BackendEvent};

use crate::ports::FederationPorts;

/// 本插件处理的命令名。
pub const COMMANDS: [&str; 5] = [
    "SaveFederationPeer",
    "DeleteFederationPeer",
    "RequestFederationStatus",
    "RequestFederationInvite",
    "MigrateSession",
];

/// 命令处理器：转发给装配期注入的出口。
pub struct FederationCommandBridge {
    ports: Arc<FederationPorts>,
    emit: Arc<dyn Fn(BackendEvent) + Send + Sync>,
}

impl FederationCommandBridge {
    /// `emit` 是**进程级**事件出口（装配期捕获）：命令的副作用是发事件，
    /// 而"发给哪个 agent"在去主智能体之后不再是问题——所有 persona 共用
    /// 同一个汇聚点（见 `ports` 模块文档）。
    pub fn new(ports: Arc<FederationPorts>, emit: Arc<dyn Fn(BackendEvent) + Send + Sync>) -> Self {
        Self { ports, emit }
    }

    /// 处理一条命令；`None` = 不是本域。
    pub async fn handle(
        &self,
        name: &str,
        payload: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        if !COMMANDS.contains(&name) {
            return None;
        }
        // 名字 → 强类型（载荷是剥掉外层 tag 的对象；套回去再反序列化）。
        let tagged = serde_json::json!({ name: payload });
        let cmd: BackendCommand = match serde_json::from_value(tagged) {
            Ok(cmd) => cmd,
            Err(error) => {
                return Some(Err(format!("federation 命令载荷解析失败（{name}）: {error}")))
            }
        };
        match &self.ports.command_handler {
            Some(handler) => {
                handler(cmd).await;
                Some(Ok(serde_json::Value::Null))
            }
            None => {
                // 未接线：明确报错（不是静默丢弃）。与宿主此前那句
                // 「联邦未启用（[federation] enabled = false）」同一语义，
                // 但词汇归插件（它知道自己是联邦）。
                (self.emit)(BackendEvent::Error {
                    session_id: None,
                    message: "联邦未启用（[federation] enabled = false）".into(),
                });
                Some(Ok(serde_json::Value::Null))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn bridge(ports: FederationPorts) -> (FederationCommandBridge, Arc<Mutex<Vec<BackendEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let emit: Arc<dyn Fn(BackendEvent) + Send + Sync> =
            Arc::new(move |event| sink.lock().unwrap().push(event));
        (FederationCommandBridge::new(Arc::new(ports), emit), events)
    }

    /// 未接线：7 条命令都回可读错误（面板点了按钮必须有回应）。
    #[tokio::test]
    async fn unwired_handler_reports_a_readable_error() {
        let (bridge, events) = bridge(FederationPorts::unlinked());
        for name in COMMANDS {
            bridge
                .handle(name, serde_json::Value::Null)
                .await
                .unwrap_or_else(|| panic!("{name} 必须被本域认领"))
                // 载荷形状不对也不算协议错误（这里断言的是"认领 + 不 panic"）
                .ok();
        }
        let events = events.lock().unwrap();
        assert!(
            events.iter().all(|event| matches!(
                event,
                BackendEvent::Error { message, .. } if message.contains("联邦未启用")
            )),
            "未接线时必须全部回「联邦未启用」：{events:?}"
        );
    }

    /// 已接线：转发给处理者，**插件自己不发事件**（副作用归处理者）。
    #[tokio::test]
    async fn wired_handler_delegates_without_emitting() {
        let called = Arc::new(Mutex::new(Vec::<String>::new()));
        let record = called.clone();
        let ports = FederationPorts {
            command_handler: Some(Arc::new(move |cmd| {
                record.lock().unwrap().push(cmd.kind_name().to_string());
                Box::pin(async {})
            })),
            ..FederationPorts::unlinked()
        };
        let (bridge, events) = bridge(ports);
        bridge
            .handle(
                "RequestFederationStatus",
                serde_json::Value::Null,
            )
            .await
            .expect("本域命令")
            .expect("转发成功");
        assert_eq!(*called.lock().unwrap(), vec!["RequestFederationStatus"]);
        assert!(events.lock().unwrap().is_empty(), "插件不该自己发事件");
    }

    /// 外部命令不认领（让下一个插件/宿主接手）。
    #[tokio::test]
    async fn foreign_commands_are_not_claimed() {
        let (bridge, _events) = bridge(FederationPorts::unlinked());
        assert!(bridge
            .handle("RequestToolsList", serde_json::Value::Null)
            .await
            .is_none());
    }

    /// 载荷形状不对：认领 + 报解析错误（不静默）。
    #[tokio::test]
    async fn malformed_payload_is_reported() {
        let (bridge, _events) = bridge(FederationPorts::unlinked());
        let outcome = bridge
            .handle("MigrateSession", serde_json::json!(42))
            .await
            .expect("仍是本域命令");
        assert!(outcome.is_err(), "坏载荷必须报错");
    }
}
