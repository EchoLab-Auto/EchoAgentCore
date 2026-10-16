//! 适配器命令域（平台中立；泛化 S5，2026-10）。
//!
//! - 通用命令 `InvokeAdapterOp` 的分发：经适配器管理面
//!   `AdapterManagement::invoke_custom` 执行、`AdapterOpResult` 回执、
//!   写方法跨实例广播（同 platform + 源实例 gating 声明）、状态下发单源化
//!   （`AdapterState` 单源派生）。
//! - 实例寻址 [`resolve_adapter`]：显式名字优先；缺省按**注册表全体**
//!   回退——0 个报错、恰 1 个选它、≥2 要求显式 `adapter`（平台中立，无
//!   `platform()` 过滤）。
//! - 旧 QQ 命令变体（迁移期）的处理臂在 `legacy.rs`（N+2 随协议变体删除；
//!   见 01b §D.1 / §D.5）。
//!
//! **多实例寻址**（2026-09）：命令携带 `adapter: Option<String>`（实例名）。
//! - `Some(name)` → 精确寻址该实例；
//! - `None` → 按注册表全体计数（单实例部署行为不变），多于一个时报错要求
//!   显式指定。

use crate::agent::Agent;
use crate::command::BackendCommand;
use crate::event::BackendEvent;

impl Agent {
    /// 解析命令要操作的适配器实例：显式名字优先；缺省时按**注册表全体**
    /// 回退（0 个 → 错误；恰 1 个 → 选它；≥2 → 要求显式 `adapter`）。
    /// 平台中立（泛化 S5 决策，见 01b §D.1）。
    fn resolve_adapter(
        &self,
        adapter: &Option<String>,
    ) -> Result<std::sync::Arc<dyn echo_adapter::traits::Adapter>, String> {
        if let Some(name) = adapter.as_deref().filter(|n| !n.is_empty()) {
            return self
                .adapters
                .get(name)
                .ok_or_else(|| format!("适配器实例 {name} 未找到"));
        }
        let all: Vec<std::sync::Arc<dyn echo_adapter::traits::Adapter>> = self
            .adapters
            .names()
            .into_iter()
            .filter_map(|n| self.adapters.get(&n))
            .collect();
        match all.len() {
            0 => Err("未找到任何适配器实例".into()),
            1 => Ok(all[0].clone()),
            n => Err(format!(
                "有 {n} 个适配器实例，请在命令中指定 adapter（实例名）"
            )),
        }
    }

    /// Handle the adapter-domain command variants. Called by `apply_command`
    /// when the command belongs to this domain.
    ///
    /// 迁移兼容（N～N+1）：旧 QQ 命令变体先过 `legacy.rs` 兼容层；其余命令
    /// 进通用 `InvokeAdapterOp` 臂。N+2 兼容层删除后本函数只剩通用臂。
    pub(crate) async fn apply_adapter_command(&self, cmd: BackendCommand) {
        let Some(cmd) = self.apply_legacy_command(cmd).await else {
            return;
        };
        match cmd {
            BackendCommand::InvokeAdapterOp {
                adapter,
                method,
                payload,
            } => {
                // W2.1/W3.3：通用适配器管理命令（唯一新增变体）——经管理面
                // invoke_custom 执行；写类方法对齐其余实例（best-effort），
                // 完成后状态下发单源化（AdapterState 单源派生旧事件）。
                // C1（泛化 02a §7.2）：失败统一回执 AdapterOpResult{error}
                // （替代 Error），实例解析失败同样以命令自带 adapter/method
                // 回显（Panel 按键配对，无需等超时）。
                tracing::info!(%adapter, %method, "Agent: InvokeAdapterOp");
                match self.resolve_adapter(&Some(adapter.clone())) {
                    Ok(ad) => {
                        // 持久化在共享段（gating 平台）的写方法：对齐同
                        // platform 的其余实例（源实例须声明 gating 能力，
                        // 见 `broadcast_adapter_op_to_others`）。
                        const SHARED_WRITE_METHODS: &[&str] = &[
                            "set_gate_mode",
                            "update_allowlist",
                            "update_denylist",
                            "set_owner",
                        ];
                        if SHARED_WRITE_METHODS.contains(&method.as_str()) {
                            self.broadcast_adapter_op_to_others(&adapter, &method, payload.clone())
                                .await;
                        }
                        let succeeded = self
                            .invoke_adapter_op(ad.clone(), &adapter, &method, payload)
                            .await;
                        // 写类方法完成后刷新状态（读类查询不重复下发）。
                        if SHARED_WRITE_METHODS.contains(&method.as_str()) {
                            self.emit_adapter_state_events(ad.clone(), Some(adapter.clone()));
                        }
                        // C2（泛化 02a §7.2 D2）：读路径 `get_state` 成功完成后
                        // 推送状态（Panel 首屏路径；复用单源，其余读方法不加）。
                        if succeeded && method == "get_state" {
                            self.emit_adapter_state_events(ad.clone(), Some(adapter.clone()));
                        }
                    }
                    Err(message) => self.emit(BackendEvent::AdapterOpResult {
                        adapter: adapter.clone(),
                        method: method.clone(),
                        result: serde_json::Value::Null,
                        error: Some(message),
                    }),
                }
            }
            // 其余命令不属于适配器域（apply_command 只对本域变体调用本函数）。
            _ => {}
        }
    }

    // ── 适配器管理通道基础设施（W3.3/W3.4，2026-10）─────────────────────

    /// 经 `AdapterManagement::invoke_custom` 调用一个管理方法并回执
    /// （`AdapterOpResult`；成功 error=None）。失败同样回执
    /// `AdapterOpResult{result: null, error}`（C1，泛化 02a §7.2）——不再
    /// 发 `Error`，Panel 按 (adapter, method) 键配对而非等超时。
    /// 返回是否成功（`get_state` 读路径推送需要）。
    async fn invoke_adapter_op(
        &self,
        ad: std::sync::Arc<dyn echo_adapter::traits::Adapter>,
        adapter_name: &str,
        method: &str,
        payload: serde_json::Value,
    ) -> bool {
        match invoke_adapter_method(ad, method, payload).await {
            Ok(result) => {
                self.emit(BackendEvent::AdapterOpResult {
                    adapter: adapter_name.to_string(),
                    method: method.to_string(),
                    result,
                    error: None,
                });
                true
            }
            Err(message) => {
                self.emit(BackendEvent::AdapterOpResult {
                    adapter: adapter_name.to_string(),
                    method: method.to_string(),
                    result: serde_json::Value::Null,
                    error: Some(message),
                });
                false
            }
        }
    }

    /// 状态下发单源化（W3.4）：从 `AdapterManagement::cached_state` 一次性
    /// 读管理状态，派生新通道事件（`AdapterState`）与全部迁移期旧事件——
    /// 单一事实来源，无双源漂移；能力缺失 / 无状态时静默跳过（调用方
    /// 已给出错误反馈）。旧事件派生在 legacy 兼容层（N+2 只留 AdapterState）。
    pub(crate) fn emit_adapter_state_events(
        &self,
        ad: std::sync::Arc<dyn echo_adapter::traits::Adapter>,
        adapter: Option<String>,
    ) {
        let Some(management) = ad.management() else {
            return;
        };
        let Some(state) = management.cached_state() else {
            return;
        };
        // 新通道：单事件全量状态。
        self.emit(BackendEvent::AdapterState {
            adapter: ad.name().to_string(),
            state: state.clone(),
        });
        // 迁移期：由同一状态派生旧事件（W4.1，deprecated 保留；推送路径
        // 共用同一纯函数——单源派生防双源漂移）。函数在 legacy 兼容层。
        for event in super::legacy::derive_legacy_qq_state_events(&state, adapter) {
            self.emit(event);
        }
    }

    /// 把写类管理方法（gating 平台的共享配置）对齐到同 **platform** 的
    /// 其余实例（多实例广播，best-effort；单实例失败仅记日志）。
    ///
    /// 泛化 D.2 a′（2026-10）：不再硬编码 `"qq"`——
    /// - 目标 = 与**源实例相同 platform** 的其它实例；
    /// - 源实例未声明 `gating` 能力时不广播（每实例独立门控的平台不做
    ///   共享段对齐）。
    ///
    /// 已知限制：面向未来「同 platform 但每实例独立门控」的平台需升级为
    /// 插件显式声明（案 a），见 01b §D.2。
    pub(crate) async fn broadcast_adapter_op_to_others(
        &self,
        skip: &str,
        method: &str,
        payload: serde_json::Value,
    ) {
        let Some(source) = self.adapters.get(skip) else {
            return;
        };
        if !source.gating_capabilities() {
            return;
        }
        let source_platform = source.platform().to_string();
        for name in self.adapters.names() {
            if name == skip {
                continue;
            }
            let Some(other) = self.adapters.get(&name) else {
                continue;
            };
            if other.platform() != source_platform {
                continue;
            }
            if let Err(message) = invoke_adapter_method(other, method, payload.clone()).await {
                tracing::warn!(adapter = %name, %method, %message, "adapter op broadcast failed");
            }
        }
    }
}

/// 经 `AdapterManagement::invoke_custom` 调用一个管理方法（适配器管理
/// 通道的通用入口；W3.3，2026-10）。适配器不存在 / 缺管理面 / 方法失败
/// 均以可读错误返回。
pub(crate) async fn invoke_adapter_method(
    ad: std::sync::Arc<dyn echo_adapter::traits::Adapter>,
    method: &str,
    payload: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let Some(management) = ad.management() else {
        return Err(format!("适配器 {} 缺少管理面实现", ad.name()));
    };
    management
        .invoke_custom(method, payload)
        .await
        .map_err(|error| format!("{error}"))
}

#[cfg(test)]
mod tests {
    use crate::command::BackendCommand;
    use crate::event::BackendEvent;
    use crate::packages::adapter_ops::test_support::{
        agent_with_instances, agent_with_two_instances, drain_events, owner_of,
    };

    /// 写方法广播泛化（01b §D.2 a′）：**同 platform** 的其它实例收到广播，
    /// 异 platform 不收；源实例未声明 gating 能力时不广播（每实例独立门控
    /// 的平台不做共享段对齐）。
    #[tokio::test]
    async fn invoke_adapter_op_write_method_broadcasts_same_platform_only() {
        let (agent, events) = agent_with_instances(&[
            ("mock-a", "mock", true),
            ("mock-b", "mock", true),
            ("other-c", "other", true),
            ("closed-x", "closed", false),
            ("closed-y", "closed", false),
        ]);

        // 源 = mock-a（同 platform 只有 mock-b；other-c 异 platform）。
        agent
            .apply_adapter_command(BackendCommand::InvokeAdapterOp {
                adapter: "mock-a".into(),
                method: "set_owner".into(),
                payload: serde_json::json!({"owner_qq": 9527}),
            })
            .await;
        let emitted = drain_events(&events);
        let result = emitted.iter().find_map(|e| match e {
            BackendEvent::AdapterOpResult {
                method,
                result,
                error,
                ..
            } => Some((method.clone(), result.clone(), error.clone())),
            _ => None,
        });
        let (method, result, error) = result.expect("应有 AdapterOpResult");
        assert_eq!(method, "set_owner");
        assert_eq!(result["owner_qq"], serde_json::json!(9527));
        assert!(error.is_none());
        // 同 platform：对齐。
        assert_eq!(
            owner_of(&agent.adapters.get("mock-b").expect("mock-b")).await,
            9527
        );
        // 异 platform：不收广播。
        assert_eq!(
            owner_of(&agent.adapters.get("other-c").expect("other-c")).await,
            0
        );

        // 源实例未声明 gating（每实例独立门控）：同 platform 也不广播。
        agent
            .apply_adapter_command(BackendCommand::InvokeAdapterOp {
                adapter: "closed-x".into(),
                method: "set_owner".into(),
                payload: serde_json::json!({"owner_qq": 1111}),
            })
            .await;
        assert_eq!(
            owner_of(&agent.adapters.get("closed-x").expect("closed-x")).await,
            1111
        );
        assert_eq!(
            owner_of(&agent.adapters.get("closed-y").expect("closed-y")).await,
            0,
            "未声明 gating 的源实例不应触发跨实例广播"
        );
    }

    /// C1（泛化 02a §7.2）：`InvokeAdapterOp` 失败路径回执
    /// `AdapterOpResult{result: null, error: 非空}`（不再发 `Error`）；
    /// 方法执行失败与实例解析失败同理（后者用命令自带 adapter/method 回显）。
    #[tokio::test]
    async fn invoke_adapter_op_failure_acks_with_error_result() {
        let (agent, events) = agent_with_two_instances();

        // 方法执行失败（mock 对未知方法返回 Err）。
        agent
            .apply_adapter_command(BackendCommand::InvokeAdapterOp {
                adapter: "mock-a".into(),
                method: "no_such_method".into(),
                payload: serde_json::json!({}),
            })
            .await;
        let emitted = drain_events(&events);
        let (adapter, method, result, error) = emitted
            .iter()
            .find_map(|e| match e {
                BackendEvent::AdapterOpResult {
                    adapter,
                    method,
                    result,
                    error,
                } => Some((
                    adapter.clone(),
                    method.clone(),
                    result.clone(),
                    error.clone(),
                )),
                _ => None,
            })
            .expect("失败应回执 AdapterOpResult");
        assert_eq!(adapter, "mock-a");
        assert_eq!(method, "no_such_method");
        assert!(result.is_null(), "失败 result 应为 null；实际 = {result:?}");
        assert!(
            error.as_deref().is_some_and(|m| !m.is_empty()),
            "error 非空；实际 = {error:?}"
        );
        assert!(
            !emitted
                .iter()
                .any(|e| matches!(e, BackendEvent::Error { .. })),
            "C1 失败不再发 Error；实际 = {emitted:?}"
        );

        // 实例解析失败：同样以命令自带 adapter/method 回显。
        agent
            .apply_adapter_command(BackendCommand::InvokeAdapterOp {
                adapter: "missing".into(),
                method: "get_state".into(),
                payload: serde_json::json!({}),
            })
            .await;
        let emitted = drain_events(&events);
        let found = emitted.iter().find_map(|e| match e {
            BackendEvent::AdapterOpResult {
                adapter,
                method,
                result,
                error,
            } => Some((
                adapter.clone(),
                method.clone(),
                result.clone(),
                error.clone(),
            )),
            _ => None,
        });
        let (adapter, method, result, error) = found.expect("解析失败同样回执 AdapterOpResult");
        assert_eq!(adapter, "missing");
        assert_eq!(method, "get_state");
        assert!(result.is_null());
        assert!(
            error.as_deref().is_some_and(|m| !m.is_empty()),
            "error 非空；实际 = {error:?}"
        );
    }

    /// C2（泛化 02a §7.2 D2）：读路径 `get_state` 成功完成后推送
    /// `AdapterState`（+ 派生旧事件，单源）；其它读方法不推。
    #[tokio::test]
    async fn invoke_adapter_op_get_state_pushes_adapter_state() {
        let (agent, events) = agent_with_two_instances();

        agent
            .apply_adapter_command(BackendCommand::InvokeAdapterOp {
                adapter: "mock-a".into(),
                method: "get_state".into(),
                payload: serde_json::json!({}),
            })
            .await;
        let emitted = drain_events(&events);
        // 回执成功。
        assert!(
            emitted.iter().any(|e| matches!(
                e,
                BackendEvent::AdapterOpResult { method, error: None, .. } if method == "get_state"
            )),
            "get_state 应回执成功；实际 = {emitted:?}"
        );
        // 状态推送：AdapterState（目标实例）+ 派生旧事件同源。
        let states: Vec<_> = emitted
            .iter()
            .filter_map(|e| match e {
                BackendEvent::AdapterState { adapter, state } => {
                    Some((adapter.clone(), state.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            states.len(),
            1,
            "get_state 成功应恰好推一条 AdapterState；实际 = {emitted:?}"
        );
        assert_eq!(states[0].0, "mock-a");
        assert_eq!(states[0].1["gate"]["mode"], serde_json::json!("none"));
        let legacy: Vec<_> = emitted
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    BackendEvent::QqGateMode { .. }
                        | BackendEvent::QqFilterConfig { .. }
                        | BackendEvent::QqOwner { .. }
                )
            })
            .collect();
        assert_eq!(
            legacy.len(),
            3,
            "派生旧事件应与状态同源派生；实际 = {emitted:?}"
        );

        // 另一读方法（get_filter_config）成功但**不**推 AdapterState。
        agent
            .apply_adapter_command(BackendCommand::InvokeAdapterOp {
                adapter: "mock-a".into(),
                method: "get_filter_config".into(),
                payload: serde_json::json!({}),
            })
            .await;
        let emitted = drain_events(&events);
        assert!(
            !emitted
                .iter()
                .any(|e| matches!(e, BackendEvent::AdapterState { .. })),
            "非 get_state 读方法不推 AdapterState；实际 = {emitted:?}"
        );
    }
}
