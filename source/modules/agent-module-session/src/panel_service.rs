//! 会话模块对面板开放的服务（`echo-agent.sessions`）。
//!
//! ## 为什么要有它
//!
//! 面板的「会话」按钮此前**只能靠广播**：`SessionUpdated` 一条条累积成
//! `state.sessions`。那有两个洞，都是"没有主动入口"造成的：
//!
//! 1. **首屏要等事件**。进程刚起来磁盘上有会话，但没有任何变更发生——
//!    一条 `SessionUpdated` 都不会来，侧边栏空着，直到用户手动动了一下工作区
//!    触发一次广播（2026-11 的现场）。
//! 2. **拼不出来就问不到**。面板想"重新数一遍"时没有命令可发，只能等。
//!
//! 现在提供一个读方法：面板发 `InvokeService { service: "echo-agent.sessions",
//! method: "list" }`，一次往返拿到完整快照。广播仍然保留——它负责**增量**
//! （某个会话有新消息 → `last_active` 变），本服务负责**基线**。
//!
//! ## 为什么服务实现在模块里而不是宿主
//!
//! 列表是会话模块的领域数据（它拥有 `TrunkStore` 与 `Session::info()` 的
//! 映射规则）。宿主只提供模块做不了的两件事：跨人格遍历，以及
//! `NodeIdentity`（联邦归属，装配期注入）。这两件事走
//! [`SessionCommandHost::sessions_snapshot`]。
//!
//! 归属跟着挂载走：模块被禁用 → 服务一起消失 → 面板拿到"服务不存在"而不是
//! 空列表（空列表会被误读成"这个部署没有会话"，那是错的）。

use std::sync::Arc;

use async_trait::async_trait;
use echo_module::ServiceSlot;

use crate::commands::SessionCommandHost;

/// 服务名（语义名，稳定）。
///
/// 用复数（`sessions`）表示"会话这个集合"，方法名另外一级——**不要**写成
/// `echo-agent.session.list`：那会把"服务"和"方法"两级压成一级，加第二个
/// 方法时无路可走。
pub const SERVICE_NAME: &str = "echo-agent.sessions";

/// `list` 支持的方法。
pub const METHODS: &[&str] = &["list"];

/// 会话服务：面板按服务名请求会话列表。
pub struct SessionCatalog {
    /// 宿主接缝（mount 时绑定、调用时取用）。
    ///
    /// 与命令域共用同一个 `ServiceSlot`：两者要的是同一份宿主能力，分开放
    /// 会出现"命令能跑、服务说未挂载"的错位。
    host: ServiceSlot<Arc<dyn SessionCommandHost>>,
}

impl SessionCatalog {
    /// 用命令域同一个接缝槽构造。
    pub fn new(host: ServiceSlot<Arc<dyn SessionCommandHost>>) -> Self {
        Self { host }
    }
}

/// `list` 的载荷形状（模块与面板自行约定，注册表不解释）。
///
/// ```json
/// { "team_id": "alix" }   // 可选：只取该人格；缺省 = 跨全部人格
/// ```
///
/// 回执 `data` 形状：
///
/// ```json
/// { "sessions": [ /* SessionInfo */ ], "team_id": "alix" | null }
/// ```
#[async_trait]
impl echo_defs::panel_service::PanelService for SessionCatalog {
    fn name(&self) -> &'static str {
        SERVICE_NAME
    }

    fn methods(&self) -> &'static [&'static str] {
        METHODS
    }

    async fn invoke(
        &self,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if !METHODS.contains(&method) {
            // 注册表已按 `methods()` 过滤过一次；这里是第二道保险（单测与
            // 未来的直接调用方都会走同一条路径）。
            return Err(format!("会话服务未声明方法 {method}"));
        }
        let Some(host) = self.host.get() else {
            return Err(
                "会话服务暂不可用（模块尚未挂载：会话命令域接缝未绑定）".to_string()
            );
        };
        let team_id = payload
            .get("team_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let sessions = host.sessions_snapshot(team_id.clone()).await?;
        Ok(serde_json::json!({
            "team_id": team_id,
            "sessions": sessions,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_module_management::protocol::SessionInfo;
    use crate::SESSION_COMMAND_HOST_SERVICE;

    /// 只验服务面本身（载荷形状 / 方法过滤 / 接缝未绑定时的错误），
    /// 宿主遍历逻辑由 loop 侧的测试覆盖。
    struct StubHost {
        sessions: Vec<SessionInfo>,
        fail: Option<&'static str>,
    }

    impl SessionCommandHost for StubHost {
        fn bind_target(
            self: Arc<Self>,
            _team_id: Option<&str>,
        ) -> Result<Arc<dyn SessionCommandHost>, String> {
            Ok(self)
        }
        fn self_team_id(&self) -> Option<String> {
            None
        }
        fn resolve_trunk(&self, _team_id: &str) -> Option<Arc<crate::TrunkStore>> {
            None
        }
        fn trunk(&self) -> Arc<crate::TrunkStore> {
            Arc::new(crate::TrunkStore::new(1000))
        }
        fn forget_session_provider(&self, _session_id: &str) {}
        fn quiesce_session<'a>(
            &'a self,
            _team_id: Option<&'a str>,
            _session_id: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Box<dyn Send>, String>> + Send + 'a>,
        > {
            Box::pin(async { Ok(Box::new(()) as Box<dyn Send>) })
        }
        fn emit(&self, _event: agent_module_management::protocol::BackendEvent) {}
        fn emit_context_snapshot<'a>(
            &'a self,
            _session_id: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }
        fn compact<'a>(
            &'a self,
            _team_id: Option<String>,
            _session_id: &'a str,
            _keep_recent: usize,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>,
        > {
            Box::pin(async { Ok(String::new()) })
        }
        fn sessions_snapshot<'a>(
            &'a self,
            _team_id: Option<String>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<SessionInfo>, String>> + Send + 'a>,
        > {
            let sessions = self.sessions.clone();
            let fail = self.fail;
            Box::pin(async move {
                if let Some(reason) = fail {
                    return Err(reason.to_string());
                }
                Ok(sessions)
            })
        }
    }

    fn info(id: &str) -> SessionInfo {
        SessionInfo {
            id: id.to_string(),
            node_id: None,
            region_name: None,
            team_id: Some("alix".into()),
            platform: "local".into(),
            scope: "workspace".into(),
            user_id: "local_user".into(),
            nickname: "测试".into(),
            group_name: None,
            last_active: 1,
            last_message: String::new(),
        }
    }

    /// 走真实路径绑定宿主替身（`ServiceSlot` 只认注册进 `Ctx` 的服务）。
    fn bound(host: Arc<dyn SessionCommandHost>) -> SessionCatalog {
        let ctx = Arc::new(echo_context::Ctx::default());
        let keep = ctx
            .provide(
                &echo_context::ServiceKey::<Arc<dyn SessionCommandHost>>::new(
                    SESSION_COMMAND_HOST_SERVICE,
                ),
                host,
            )
            .expect("替身注册");
        let mctx = echo_module::MountContext::new().with_ctx(ctx);
        let slot = ServiceSlot::new();
        let keep_bind = slot
            .bind(
                &mctx,
                &echo_context::ServiceKey::<Arc<dyn SessionCommandHost>>::new(
                    SESSION_COMMAND_HOST_SERVICE,
                ),
            )
            .expect("接缝绑定");
        // 两个 `Disposer` 都必须活到调用结束：落下即注销（RAII）。`ctx.provide`
        // 那个撤服务，`bind` 那个撤槽位绑定——忘掉任一个都会让服务/接缝消失，
        // 而表现是"接缝未绑定"这种容易误判成挂载顺序问题的错误。
        std::mem::forget(keep);
        std::mem::forget(keep_bind);
        SessionCatalog::new(slot)
    }

    /// 接缝槽在 mount 前是空的：必须回明确错误，而不是空列表。
    #[tokio::test]
    async fn unbound_seam_is_an_explicit_error() {
        use echo_defs::panel_service::PanelService;
        let catalog = SessionCatalog::new(ServiceSlot::new());
        let error = catalog
            .invoke("list", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(error.contains("尚未挂载"), "{error}");
    }

    /// `list` 的载荷：`team_id` 透传给宿主，回执把它回显出来。
    #[tokio::test]
    async fn list_returns_sessions_and_echoes_the_scope() {
        use echo_defs::panel_service::PanelService;
        let catalog = bound(Arc::new(StubHost {
            sessions: vec![info("a"), info("b")],
            fail: None,
        }));
        let value = catalog
            .invoke("list", serde_json::json!({ "team_id": "alix" }))
            .await
            .unwrap();
        assert_eq!(value["team_id"], "alix");
        assert_eq!(value["sessions"].as_array().unwrap().len(), 2);
    }

    /// 空 `team_id` 与缺失等价（面板清空选择时不带身份 = 跨全部人格）。
    #[tokio::test]
    async fn empty_team_id_is_treated_as_absent() {
        use echo_defs::panel_service::PanelService;
        let catalog = bound(Arc::new(StubHost {
            sessions: vec![info("a")],
            fail: None,
        }));
        let value = catalog
            .invoke("list", serde_json::json!({ "team_id": "" }))
            .await
            .unwrap();
        assert!(value["team_id"].is_null(), "{value}");
    }

    /// 宿主失败：原因原样透传（服务层不加工领域错误）。
    #[tokio::test]
    async fn host_failure_is_passed_through() {
        use echo_defs::panel_service::PanelService;
        let catalog = bound(Arc::new(StubHost {
            sessions: Vec::new(),
            fail: Some("trunk 加载失败"),
        }));
        let error = catalog
            .invoke("list", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert_eq!(error, "trunk 加载失败");
    }

    /// 未声明的方法：实现侧也有第二道拒绝（注册表之外）。
    #[tokio::test]
    async fn undeclared_method_is_rejected() {
        use echo_defs::panel_service::PanelService;
        let catalog = SessionCatalog::new(ServiceSlot::new());
        let error = catalog
            .invoke("all", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(error.contains("all"), "{error}");
    }

    /// 服务名与方法名是跨仓契约（面板硬编码它们）：改名等于面板失联。
    #[test]
    fn service_contract_is_frozen() {
        assert_eq!(SERVICE_NAME, "echo-agent.sessions");
        assert_eq!(METHODS, &["list"]);
    }
}
