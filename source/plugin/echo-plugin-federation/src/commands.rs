//! 联邦命令域：5 条 `*Federation*` / `MigrateSession` 命令。
//!
//! ## 为什么这几条归本插件
//!
//! 它们是**联邦的管理面**：保存对端、生成邀请串、查询链路状态、迁移会话。
//! 此前它们在宿主 `apply_command` 里是一段"问 `ports.command_handler`，
//! 没有就报「联邦未启用」"的内联分支——命令归属关系写在宿主里，而处理者
//! 本来就是联邦自己的出口。
//!
//! ## 现在实现也在插件里（2026-11 S11）
//!
//! 本模块只是**薄桥**：把 `ports.admin`（宿主的状态迁移出口）交出去，
//! 由 [`crate::admin::dispatch`] 做真正的分发。规则（token 继承 / 冲突 /
//! URL 形状 / 邀请地址推导）全在 `admin.rs` 的纯函数里，状态迁移（写配置、
//! 断链、落 peer 表）由宿主的 `FederationAdminHost` 实现。
//!
//! 于是"哪条命令归谁、判什么、失败说什么"完整地在本 crate 内可读、可测；
//! 宿主那边只剩一个 `impl FederationAdminHost`。

use std::sync::Arc;

use echo_protocol::BackendEvent;

use crate::admin::{self, Emit};
use crate::ports::FederationPorts;

/// 本插件处理的命令名（真源在 [`admin::COMMANDS`]）。
pub const COMMANDS: [&str; 6] = admin::COMMANDS;

/// 命令处理器：把命令交给规则层，状态迁移经 [`FederationAdminHost`]。
pub struct FederationCommandBridge {
    ports: Arc<FederationPorts>,
    emit: Emit,
}

impl FederationCommandBridge {
    /// `emit` 是**进程级**事件出口（装配期捕获）：命令的副作用是发事件，
    /// 而"发给哪个 agent"在去主智能体之后不再是问题——所有 persona 共用
    /// 同一个汇聚点（见 `ports` 模块文档）。
    pub fn new(ports: Arc<FederationPorts>, emit: Emit) -> Self {
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
        match self.ports.admin.clone() {
            Some(admin) => admin::dispatch(admin.as_ref(), name, payload, &self.emit).await,
            None => {
                // 未接线：明确报错（不是静默丢弃）。词汇归插件（它知道自己是
                // 联邦），宿主的 `apply_command` 不再需要写这条文案。
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
    use crate::admin::{FederationAdminHost, PeerSave, PeerSummary};
    use std::sync::Mutex;

    fn bridge(ports: FederationPorts) -> (FederationCommandBridge, Arc<Mutex<Vec<BackendEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let emit: Emit = Arc::new(move |event| sink.lock().unwrap().push(event));
        (FederationCommandBridge::new(Arc::new(ports), emit), events)
    }

    /// 未接线：5 条命令都回可读错误（面板点了按钮必须有回应）。
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

    /// 非本域命令：`None`（宿主继续找，不抢别人的命令）。
    #[tokio::test]
    async fn foreign_commands_are_not_claimed() {
        let (bridge, _events) = bridge(FederationPorts::unlinked());
        for name in ["SendMessage", "ToggleTool", "RequestPluginsList", ""] {
            assert!(
                bridge
                    .handle(name, serde_json::Value::Null)
                    .await
                    .is_none(),
                "{name} 不该被联邦认领"
            );
        }
    }

    /// 载荷形状不对：回可读错误（而不是 panic / 静默丢弃）。
    #[tokio::test]
    async fn malformed_payload_is_reported() {
        let (unwired, _events) = bridge(FederationPorts::unlinked());
        // 未接线时先走"联邦未启用"分支（不解析载荷）：不许 panic。
        unwired
            .handle("SaveFederationPeer", serde_json::json!({"peer": {"bogus": 1}}))
            .await
            .expect("本域命令")
            .ok();
        // 接线后才会走到载荷解析：缺字段必须报可读错误，文案带命令名。
        let mut ports = FederationPorts::unlinked();
        ports.admin = Some(Arc::new(NoopAdmin));
        let (wired, _events) = bridge(ports);
        let error = wired
            .handle("SaveFederationPeer", serde_json::json!({"peer": {"bogus": 1}}))
            .await
            .expect("本域命令")
            .expect_err("载荷非法必须报错");
        assert!(error.contains("SaveFederationPeer"), "{error}");
    }

    /// 什么都不做的 admin（载荷解析测试用）。
    struct NoopAdmin;

    #[async_trait::async_trait]
    impl FederationAdminHost for NoopAdmin {
        async fn peers(&self) -> Vec<PeerSummary> {
            Vec::new()
        }
        fn merged_management_token(&self) -> Option<String> {
            None
        }
        fn listen(&self) -> String {
            String::new()
        }
        fn advertise(&self) -> String {
            String::new()
        }
        fn observed_host(&self) -> Option<String> {
            None
        }
        fn node_name(&self) -> Option<String> {
            None
        }
        async fn status_event(&self) -> BackendEvent {
            BackendEvent::Error {
                session_id: None,
                message: "status".into(),
            }
        }
        async fn directory_event(&self) -> BackendEvent {
            BackendEvent::Error {
                session_id: None,
                message: "directory".into(),
            }
        }
        async fn apply_peer(&self, _peer: PeerSave) {}
        async fn remove_peer(&self, _name: &str) {}
        async fn register_invite(&self, _placeholder: &str, _token: &str) -> Vec<String> {
            Vec::new()
        }
        async fn migrate_session(
            &self,
            _session_id: String,
            _team_id: Option<String>,
            _target_peer: String,
            _emit: Emit,
        ) {
        }
    }

    /// 已接线：规则层接管，**未接线的错误文案不再出现**。
    #[tokio::test]
    async fn wired_handler_runs_the_rule_layer() {
        struct ProbeHost {
            saved: Mutex<Vec<PeerSave>>,
        }

        #[async_trait::async_trait]
        impl FederationAdminHost for ProbeHost {
            async fn peers(&self) -> Vec<PeerSummary> {
                Vec::new()
            }
            fn merged_management_token(&self) -> Option<String> {
                None
            }
            fn listen(&self) -> String {
                "10.0.0.1:3133".into()
            }
            fn advertise(&self) -> String {
                String::new()
            }
            fn observed_host(&self) -> Option<String> {
                None
            }
            fn node_name(&self) -> Option<String> {
                None
            }
            async fn status_event(&self) -> BackendEvent {
                BackendEvent::Error {
                    session_id: None,
                    message: "status".into(),
                }
            }
            async fn directory_event(&self) -> BackendEvent {
                BackendEvent::Error {
                    session_id: None,
                    message: "directory".into(),
                }
            }
            async fn apply_peer(&self, peer: PeerSave) {
                self.saved.lock().unwrap().push(peer);
            }
            async fn remove_peer(&self, _name: &str) {}
            async fn register_invite(&self, _placeholder: &str, _token: &str) -> Vec<String> {
                Vec::new()
            }
            async fn migrate_session(
                &self,
                _session_id: String,
                _team_id: Option<String>,
                _target_peer: String,
                _emit: Emit,
            ) {
            }
        }

        let host = Arc::new(ProbeHost {
            saved: Mutex::new(Vec::new()),
        });
        let mut ports = FederationPorts::unlinked();
        ports.admin = Some(host.clone());
        let (bridge, events) = bridge(ports);

        // 缺 token 的新建 → 规则拒绝（**插件内**判的，不是宿主）。
        let outcome = bridge
            .handle(
                "SaveFederationPeer",
                serde_json::json!({
                    "peer": {"name": "peer-b", "url": "ws://10.0.0.2:3133", "token": ""}
                }),
            )
            .await
            .expect("本域命令");
        outcome.expect("协议层成功（业务错误走事件）");
        assert!(
            events.lock().unwrap().iter().any(|event| matches!(
                event,
                BackendEvent::Error { message, .. } if message.contains("token 不能为空")
            )),
            "规则层必须拒绝空 token：{:?}",
            events.lock().unwrap()
        );
        assert!(host.saved.lock().unwrap().is_empty(), "拒绝时不得落状态");

        // 给了 token → 落到宿主。
        bridge
            .handle(
                "SaveFederationPeer",
                serde_json::json!({
                    "peer": {"name": "peer-b", "url": "ws://10.0.0.2:3133", "token": "s3cret"}
                }),
            )
            .await
            .expect("本域命令")
            .expect("ok");
        let saved = host.saved.lock().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "peer-b");
        assert_eq!(saved[0].token, "s3cret");
    }

    /// 邀请：从 `listen` 推导地址，产出一条 `FederationInvite` 事件。
    #[tokio::test]
    async fn invite_is_built_from_the_host_listen_address() {
        struct InviteHost {
            registered: Mutex<Vec<(String, String)>>,
        }

        #[async_trait::async_trait]
        impl FederationAdminHost for InviteHost {
            async fn peers(&self) -> Vec<PeerSummary> {
                Vec::new()
            }
            fn merged_management_token(&self) -> Option<String> {
                None
            }
            fn listen(&self) -> String {
                "10.0.0.1:3133".into()
            }
            fn advertise(&self) -> String {
                String::new()
            }
            fn observed_host(&self) -> Option<String> {
                None
            }
            fn node_name(&self) -> Option<String> {
                Some("alpha".into())
            }
            async fn status_event(&self) -> BackendEvent {
                BackendEvent::Error {
                    session_id: None,
                    message: "status".into(),
                }
            }
            async fn directory_event(&self) -> BackendEvent {
                BackendEvent::Error {
                    session_id: None,
                    message: "directory".into(),
                }
            }
            async fn apply_peer(&self, _peer: PeerSave) {}
            async fn remove_peer(&self, _name: &str) {}
            async fn register_invite(&self, placeholder: &str, token: &str) -> Vec<String> {
                self.registered
                    .lock()
                    .unwrap()
                    .push((placeholder.to_string(), token.to_string()));
                Vec::new()
            }
            async fn migrate_session(
                &self,
                _session_id: String,
                _team_id: Option<String>,
                _target_peer: String,
                _emit: Emit,
            ) {
            }
        }

        let host = Arc::new(InviteHost {
            registered: Mutex::new(Vec::new()),
        });
        let mut ports = FederationPorts::unlinked();
        ports.admin = Some(host.clone());
        let (bridge, events) = bridge(ports);
        bridge
            .handle("RequestFederationInvite", serde_json::Value::Null)
            .await
            .expect("本域命令")
            .expect("ok");

        let events = events.lock().unwrap();
        let invite = events
            .iter()
            .find_map(|event| match event {
                BackendEvent::FederationInvite { invite } => Some(invite.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("必须发出邀请串事件：{events:?}"));
        assert!(invite.starts_with("echofed://"), "{invite}");
        assert!(invite.contains("10.0.0.1%3A3133") || invite.contains("10.0.0.1:3133"), "{invite}");
        let registered = host.registered.lock().unwrap();
        assert_eq!(registered.len(), 1, "必须登记占位");
        assert!(registered[0].0.starts_with("invite-"), "{registered:?}");
    }
}
