//! QQ-specific frontend command handling.
//!
//! The QQ command variants (`UpdateQqAllowlist`/`UpdateQqDenylist`/
//! `SetQqGateMode`/`RequestQqFilterConfig`/`RequestGroupList`/
//! `RequestFriendList`/登录查询) are dispatched here, out of the main
//! `apply_command` match.
//!
//! **多实例寻址**（2026-09）：命令携带 `adapter: Option<String>`（QQ 实例名）。
//! - `Some(name)` → 精确寻址该实例；
//! - `None` → 若注册表里恰好只有一个 QQ 实例则用它（单实例部署行为不变），
//!   多于一个时报错要求显式指定。

use crate::agent::Agent;
use crate::command::BackendCommand;
use crate::event::BackendEvent;

impl Agent {
    /// 解析命令要操作的 QQ 实例：显式名字优先；缺省时唯一实例回退。
    fn resolve_qq_adapter(
        &self,
        adapter: &Option<String>,
    ) -> Result<std::sync::Arc<dyn echo_adapter::traits::Adapter>, String> {
        if let Some(name) = adapter.as_deref().filter(|n| !n.is_empty()) {
            return self
                .adapters
                .get(name)
                .ok_or_else(|| format!("QQ 实例 {name} 未找到"));
        }
        let qq: Vec<std::sync::Arc<dyn echo_adapter::traits::Adapter>> = self
            .adapters
            .names()
            .into_iter()
            .filter_map(|n| self.adapters.get(&n))
            .filter(|a| a.platform() == "qq")
            .collect();
        match qq.len() {
            0 => Err("QQ 适配器未找到".into()),
            1 => Ok(qq[0].clone()),
            n => Err(format!(
                "有 {n} 个 QQ 实例，请在命令中指定 adapter（实例名）"
            )),
        }
    }

    /// Handle the QQ-specific command variants. Called by `apply_command`
    /// when the command is one of the QQ domain.
    pub(crate) async fn apply_qq_command(&self, cmd: BackendCommand) {
        match cmd {
            BackendCommand::UpdateQqAllowlist {
                user_ids,
                group_ids,
                adapter,
            } => {
                tracing::info!(users = ?user_ids, groups = ?group_ids, adapter = ?adapter, "Agent: UpdateQqAllowlist");
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => {
                        let payload = serde_json::json!({
                            "user_ids": user_ids,
                            "group_ids": group_ids,
                        });
                        if let Err(message) =
                            invoke_adapter_method(ad.clone(), "update_allowlist", payload.clone())
                                .await
                        {
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message,
                            });
                            return;
                        }
                        self.broadcast_adapter_op_to_others(ad.name(), "update_allowlist", payload)
                            .await;
                        // 状态下发单源化（W3.4）：AdapterState 单源派生。
                        self.emit_adapter_state_events(ad.clone(), adapter.clone());
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: "QQ 白名单已更新".into(),
                        });
                    }
                    Err(message) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    }),
                }
            }
            BackendCommand::UpdateQqDenylist {
                user_ids,
                group_ids,
                adapter,
            } => {
                tracing::info!(users = ?user_ids, groups = ?group_ids, adapter = ?adapter, "Agent: UpdateQqDenylist");
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => {
                        let payload = serde_json::json!({
                            "user_ids": user_ids,
                            "group_ids": group_ids,
                        });
                        if let Err(message) =
                            invoke_adapter_method(ad.clone(), "update_denylist", payload.clone())
                                .await
                        {
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message,
                            });
                            return;
                        }
                        self.broadcast_adapter_op_to_others(ad.name(), "update_denylist", payload)
                            .await;
                        self.emit_adapter_state_events(ad.clone(), adapter.clone());
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: "QQ 黑名单已更新".into(),
                        });
                    }
                    Err(message) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    }),
                }
            }
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
                match self.resolve_qq_adapter(&Some(adapter.clone())) {
                    Ok(ad) => {
                        // 持久化在共享段的写方法：对齐其余实例。
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
            BackendCommand::SetQqGateMode { mode, adapter } => {
                // W3.3/W3.4 参考实现：旧命令经适配器管理通道执行（管理面
                // invoke_custom）+ 状态下发单源化（AdapterState 单源派生）。
                tracing::info!(mode = %mode.as_str(), adapter = ?adapter, "Agent: SetQqGateMode");
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => {
                        let mode_str = match mode {
                            echo_adapter::GateMode::Allowlist => "allowlist",
                            echo_adapter::GateMode::Denylist => "denylist",
                            echo_adapter::GateMode::None => "none",
                        };
                        if let Err(message) = invoke_adapter_method(
                            ad.clone(),
                            "set_gate_mode",
                            serde_json::json!({"mode": mode_str}),
                        )
                        .await
                        {
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message,
                            });
                            return;
                        }
                        // 多实例对齐：模式持久化在共享 `[adapters.qq]` 段，
                        // 只应用单实例会让其余实例内存值与磁盘分叉到重启
                        // （名单已广播，模式此前漏网——2026-10 巡检 P1）。
                        self.broadcast_adapter_op_to_others(
                            ad.name(),
                            "set_gate_mode",
                            serde_json::json!({"mode": mode_str}),
                        )
                        .await;
                        // 状态下发单源化（W3.4）：AdapterState + 派生旧事件。
                        self.emit_adapter_state_events(ad.clone(), adapter.clone());
                        let label = match mode {
                            echo_adapter::GateMode::Allowlist => "白名单模式",
                            echo_adapter::GateMode::Denylist => "黑名单模式",
                            echo_adapter::GateMode::None => "无约束模式",
                        };
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("QQ 门控模式已切换为: {label}"),
                        });
                    }
                    Err(message) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    }),
                }
            }
            BackendCommand::SetQqOwner { owner_qq, adapter } => {
                tracing::info!(owner_qq = %owner_qq, adapter = ?adapter, "Agent: SetQqOwner");
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => {
                        // W2.4：字符串透传（插件 `parse_id` 接受 str）；
                        // `""` = 清除 / 无 owner（替代旧 `0`）。
                        let payload = serde_json::json!({"owner_qq": owner_qq.clone()});
                        if let Err(message) =
                            invoke_adapter_method(ad.clone(), "set_owner", payload.clone()).await
                        {
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message,
                            });
                            return;
                        }
                        // 同模式/名单：owner 持久化在共享段，需向其余实例对齐
                        // （管理员绕过读 gate，各实例行为必须一致）。
                        self.broadcast_adapter_op_to_others(ad.name(), "set_owner", payload)
                            .await;
                        self.emit_adapter_state_events(ad.clone(), adapter.clone());
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: if owner_qq.is_empty() {
                                "QQ 管理员已清除".into()
                            } else {
                                format!("QQ 管理员已设置为: {owner_qq}")
                            },
                        });
                    }
                    Err(message) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    }),
                }
            }
            BackendCommand::RequestQqOwner { adapter } => match self.resolve_qq_adapter(&adapter) {
                Ok(ad) => {
                    // 读路径（W3.4）：owner 由 cached_state 单源投影（与
                    // emit_adapter_state_events 同一状态文档）。
                    let Some(state) = ad.management().and_then(|m| m.cached_state()) else {
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("适配器 {} 缺少管理面实现", ad.name()),
                        });
                        return;
                    };
                    self.emit(BackendEvent::QqOwner {
                        owner_qq: state_owner(&state),
                        adapter: adapter.clone(),
                    });
                }
                Err(message) => self.emit(BackendEvent::Error {
                    session_id: None,
                    message,
                }),
            },
            BackendCommand::RequestQqFilterConfig { adapter } => {
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => {
                        // 读路径（W3.4）：状态下发单源化——AdapterState +
                        // 派生旧事件（mode/filter/owner 由同一状态产出）。
                        self.emit_adapter_state_events(ad.clone(), adapter.clone());
                    }
                    Err(message) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    }),
                }
            }
            BackendCommand::RequestGroupList { adapter } => {
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => {
                        match invoke_adapter_method(ad.clone(), "get_groups", serde_json::json!({}))
                            .await
                        {
                            Ok(result) => {
                                let groups = id_name_list(&result, "groups");
                                self.emit(BackendEvent::GroupList {
                                    groups: groups
                                        .into_iter()
                                        .map(|(id, name)| crate::event::GroupInfo {
                                            group_id: id,
                                            group_name: name,
                                        })
                                        .collect(),
                                    adapter: adapter.clone(),
                                })
                            }
                            Err(error) => self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("获取群列表失败: {error}"),
                            }),
                        }
                    }
                    Err(message) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    }),
                }
            }
            BackendCommand::RequestFriendList { adapter } => {
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => {
                        match invoke_adapter_method(
                            ad.clone(),
                            "get_friends",
                            serde_json::json!({}),
                        )
                        .await
                        {
                            Ok(result) => {
                                let friends = id_name_list(&result, "friends");
                                self.emit(BackendEvent::FriendList {
                                    friends: friends
                                        .into_iter()
                                        .map(|(id, name)| crate::event::FriendInfo {
                                            user_id: id,
                                            nickname: name,
                                        })
                                        .collect(),
                                    adapter: adapter.clone(),
                                })
                            }
                            Err(error) => self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("获取好友列表失败: {error}"),
                            }),
                        }
                    }
                    Err(message) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    }),
                }
            }
            BackendCommand::RequestQqLoginStatus { adapter } => {
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => {
                        let info = ad.status_info();
                        let online =
                            matches!(info.status, echo_adapter::AdapterConnectionState::Connected);
                        self.emit(BackendEvent::QqLoginStatus {
                            adapter: info.name.clone(),
                            online,
                            user_id: info.self_id.clone(),
                            nickname: None,
                        });
                    }
                    Err(message) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    }),
                }
            }
            BackendCommand::RequestQqQrcode { adapter } => {
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => {
                        // Core 代理取二维码（适配器管理通道；管理面内置 base64 编码）。
                        let name = ad.name().to_string();
                        match invoke_adapter_method(
                            ad.clone(),
                            "login_qrcode",
                            serde_json::json!({}),
                        )
                        .await
                        {
                            Ok(result) => {
                                let encoded =
                                    result["png_base64"].as_str().unwrap_or("").to_string();
                                self.emit(BackendEvent::QqQrcode {
                                    adapter: name,
                                    png_base64: encoded,
                                    error: None,
                                });
                            }
                            Err(error) => self.emit(BackendEvent::QqQrcode {
                                adapter: name,
                                png_base64: String::new(),
                                error: Some(error),
                            }),
                        }
                    }
                    Err(message) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    }),
                }
            }
            // 其余命令不属于 QQ 域（apply_command 只对本域变体调用本函数）。
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
    /// 读管理状态，派生新通道事件（`AdapterState`）与全部旧 QQ 事件——
    /// 单一事实来源，无双源漂移；能力缺失 / 无状态时静默跳过（调用方
    /// 已给出错误反馈）。
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
        // 迁移期：由同一状态派生旧 QQ 事件（W4.1，deprecated 保留；
        // 推送路径共用同一纯函数——单源派生防双源漂移）。
        for event in derive_legacy_qq_state_events(&state, adapter) {
            self.emit(event);
        }
    }

    /// 把写类管理方法（持久化在共享 `[adapters.qq]` 段）对齐到其余实例
    /// （多实例广播，best-effort；单实例失败仅记日志）。
    async fn broadcast_adapter_op_to_others(
        &self,
        skip: &str,
        method: &str,
        payload: serde_json::Value,
    ) {
        for name in self.adapters.names() {
            if name == skip {
                continue;
            }
            let Some(other) = self.adapters.get(&name) else {
                continue;
            };
            if other.platform() != "qq" {
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

/// 从管理方法结果取 `[{id, name}]` 列表（W2.4：id 支持数字/字符串两种
/// 形态，统一为 `String`（number → `to_string()`）；未知形态跳过）。
fn id_name_list(result: &serde_json::Value, key: &str) -> Vec<(String, String)> {
    result[key]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = match &item["id"] {
                        serde_json::Value::Number(number) => number.to_string(),
                        serde_json::Value::String(text) => text.clone(),
                        _ => return None,
                    };
                    let name = item["name"].as_str().unwrap_or("").to_string();
                    Some((id, name))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// owner 状态宽容读取（W2.4 去号码化）：number → `to_string()`（`0` = 未
/// 设置 → 空串）、string 原样、缺省/其它 → 空串。`""` = 无 owner（新语义，
/// 替代旧 `0`；插件 cached_state 现阶段仍发 number，Core 宽容读取）。
fn state_owner(state: &serde_json::Value) -> String {
    match &state["owner"] {
        serde_json::Value::Number(number) => number
            .as_i64()
            .filter(|value| *value != 0)
            .map(|value| value.to_string())
            .unwrap_or_default(),
        serde_json::Value::String(text) => text.clone(),
        _ => String::new(),
    }
}

/// 由管理面状态文档派生旧 QQ 事件（W4.1 迁移期，deprecated；泛化 Core-2
/// S4 抽出的可复用纯函数——`emit_adapter_state_events`（命令路径）与
/// `core` 组合根的插件推送消费者（`report_ui_state` → `AdapterState`）
/// 共用同一份派生逻辑，避免旧事件双源）。
///
/// `adapter`：旧命令自带的实例名（回显；可为 None）；推送路径传实例名。
/// W2.4：id 一律字符串输出（对 `cached_state` 宽容读取 number|string）。
/// 随 Panel 阶段 1 结束与新通道稳定一并删除（见 02a §1-D3 阶段 4）。
pub fn derive_legacy_qq_state_events(
    state: &serde_json::Value,
    adapter: Option<String>,
) -> Vec<BackendEvent> {
    let gate = &state["gate"];
    let parse_ids = |key: &str| -> Vec<String> {
        gate[key]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| match item {
                        serde_json::Value::Number(number) => Some(number.to_string()),
                        serde_json::Value::String(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let parse_mode = |text: &str| match text {
        "allowlist" => echo_adapter::GateMode::Allowlist,
        "denylist" => echo_adapter::GateMode::Denylist,
        _ => echo_adapter::GateMode::None,
    };
    vec![
        BackendEvent::QqFilterConfig {
            allowlist_users: parse_ids("allowlist_users"),
            allowlist_groups: parse_ids("allowlist_groups"),
            denylist_users: parse_ids("denylist_users"),
            denylist_groups: parse_ids("denylist_groups"),
            adapter: adapter.clone(),
        },
        BackendEvent::QqGateMode {
            mode: parse_mode(gate["mode"].as_str().unwrap_or("none")),
            adapter: adapter.clone(),
        },
        BackendEvent::QqOwner {
            owner_qq: state_owner(state),
            adapter,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{Agent, BackendCommand, BackendEvent};
    use crate::config::AgentConfig;
    use crate::skill::SkillRegistry;
    use crate::tool::ToolRegistry;
    use echo_adapter::registry::AdapterRegistry;
    use std::sync::{Arc, Mutex};

    // ── W4.2 双跑参考（适配器管理通道）──────────────────────────────────

    /// 无操作 provider（管理命令不经 LLM，仅为满足 Agent 构造）。
    struct NullProvider;

    #[async_trait::async_trait]
    impl crate::llm::LlmProvider for NullProvider {
        fn name(&self) -> &str {
            "null"
        }
        fn default_model(&self) -> &str {
            "null"
        }
        async fn chat(
            &self,
            _request: &crate::llm::ChatRequest,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Config("null provider".into()))
        }
        async fn chat_stream(
            &self,
            _request: &crate::llm::ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<crate::llm::ChatChunk>,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Config("null provider".into()))
        }
    }

    /// 测试适配器：实现 `Adapter` + `AdapterManagement`（真实 QqAdapter 在
    /// 独立 crate、未列为依赖——mock 复刻其管理面行为用于通道双跑验证；
    /// 生产实现由 `echo-adapter-qq` 的契约测试守护，见 W3.2 测试）。
    struct MockQqAdapter {
        name: String,
        mode: Mutex<String>,
        owner: Mutex<i64>,
        allow_users: Mutex<Vec<String>>,
        allow_groups: Mutex<Vec<String>>,
        deny_users: Mutex<Vec<String>>,
        deny_groups: Mutex<Vec<String>>,
    }

    impl MockQqAdapter {
        fn new(name: &str) -> Self {
            Self {
                name: name.into(),
                mode: Mutex::new("none".into()),
                owner: Mutex::new(0),
                allow_users: Mutex::new(Vec::new()),
                allow_groups: Mutex::new(Vec::new()),
                deny_users: Mutex::new(Vec::new()),
                deny_groups: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl echo_adapter::Adapter for MockQqAdapter {
        fn name(&self) -> &str {
            &self.name
        }
        fn display_name(&self) -> &str {
            &self.name
        }
        fn platform(&self) -> &str {
            "qq"
        }
        fn is_configured(&self) -> bool {
            true
        }
        fn status_info(&self) -> echo_adapter::AdapterInfo {
            echo_adapter::AdapterInfo {
                name: self.name.clone(),
                display_name: self.name.clone(),
                status: echo_adapter::AdapterConnectionState::Connected,
                self_id: Some("123".into()),
                started_at: Some(1),
                platform: "qq".into(),
                configured: true,
            }
        }
        async fn start(&self) -> Result<(), echo_adapter::AdapterError> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), echo_adapter::AdapterError> {
            Ok(())
        }
        async fn send_message(
            &self,
            _target: &echo_adapter::MessageTarget,
            _content: &str,
        ) -> Result<echo_adapter::SendResult, echo_adapter::AdapterError> {
            Ok(echo_adapter::SendResult {
                message_id: None,
                success: true,
                error: None,
            })
        }
        fn subscribe(&self, _tx: tokio::sync::mpsc::UnboundedSender<echo_adapter::AdapterEvent>) {}
        fn management(&self) -> Option<&dyn echo_adapter::AdapterManagement> {
            Some(self)
        }
    }

    #[async_trait::async_trait]
    impl echo_adapter::AdapterManagement for MockQqAdapter {
        fn cached_state(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({
                "gate": {
                    "mode": *self.mode.lock().unwrap(),
                    "allowlist_users": self.allow_users.lock().unwrap().clone(),
                    "allowlist_groups": self.allow_groups.lock().unwrap().clone(),
                    "denylist_users": self.deny_users.lock().unwrap().clone(),
                    "denylist_groups": self.deny_groups.lock().unwrap().clone(),
                },
                "owner": *self.owner.lock().unwrap(),
            }))
        }

        async fn invoke_custom(
            &self,
            method: &str,
            payload: serde_json::Value,
        ) -> Result<serde_json::Value, echo_adapter::AdapterError> {
            match method {
                "set_gate_mode" => {
                    let mode = payload["mode"].as_str().unwrap_or("none");
                    *self.mode.lock().unwrap() = mode.to_string();
                    Ok(serde_json::json!({"mode": mode}))
                }
                "get_gate_mode" => Ok(serde_json::json!({
                    "mode": self.mode.lock().unwrap().clone()
                })),
                "set_owner" => {
                    // W2.4：插件 `parse_id` 接受 number|string（mock 复刻）。
                    let owner = payload["owner_qq"]
                        .as_i64()
                        .or_else(|| payload["owner_qq"].as_str().and_then(|s| s.parse().ok()))
                        .unwrap_or(0);
                    *self.owner.lock().unwrap() = owner;
                    Ok(serde_json::json!({"owner_qq": owner}))
                }
                "get_owner" => Ok(serde_json::json!({
                    "owner_qq": *self.owner.lock().unwrap()
                })),
                // 01a §1.3 / 02a §7.2 C3：插件 `get_state` 返回完整
                // cached_state（mock 复刻插件行为；真实实现由
                // echo-adapter-qq 契约测试守护）。
                "get_state" => self.cached_state().ok_or_else(|| {
                    echo_adapter::AdapterError::Internal("no cached state available".into())
                }),
                "update_allowlist" => {
                    *self.allow_users.lock().unwrap() = parse_list(&payload, "user_ids");
                    *self.allow_groups.lock().unwrap() = parse_list(&payload, "group_ids");
                    Ok(serde_json::json!({}))
                }
                "update_denylist" => {
                    *self.deny_users.lock().unwrap() = parse_list(&payload, "user_ids");
                    *self.deny_groups.lock().unwrap() = parse_list(&payload, "group_ids");
                    Ok(serde_json::json!({}))
                }
                "get_filter_config" => Ok(serde_json::json!({
                    "allowlist_users": self.allow_users.lock().unwrap().clone(),
                    "allowlist_groups": self.allow_groups.lock().unwrap().clone(),
                    "denylist_users": self.deny_users.lock().unwrap().clone(),
                    "denylist_groups": self.deny_groups.lock().unwrap().clone(),
                })),
                other => Err(echo_adapter::AdapterError::Internal(format!(
                    "unknown adapter method: {other}"
                ))),
            }
        }
    }

    fn parse_list(payload: &serde_json::Value, key: &str) -> Vec<String> {
        payload[key]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| {
                        v.as_i64()
                            .map(|n| n.to_string())
                            .or_else(|| v.as_str().map(|s| s.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 组装一个挂两个实例（`qq-a` / `qq-b`）的 Agent，返回（agent, 事件缓冲）。
    fn agent_with_qq_instances() -> (Arc<Agent>, Arc<Mutex<Vec<BackendEvent>>>) {
        let registry = AdapterRegistry::new();
        for name in ["qq-a", "qq-b"] {
            registry.register(Arc::new(MockQqAdapter::new(name)));
        }
        let agent = Arc::new(Agent::new(
            Arc::new(NullProvider),
            AgentConfig::default(),
            SkillRegistry::new(),
            ToolRegistry::new(),
            Arc::new(registry),
        ));
        let events: Arc<Mutex<Vec<BackendEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        agent.attach_event_sink(Arc::new(move |event: BackendEvent| {
            sink_events.lock().expect("events poisoned").push(event);
        }));
        (agent, events)
    }

    fn drain_events(events: &Arc<Mutex<Vec<BackendEvent>>>) -> Vec<BackendEvent> {
        std::mem::take(&mut *events.lock().expect("events poisoned"))
    }

    /// 双跑：旧命令（`SetQqGateMode`）与新命令（`InvokeAdapterOp`）产生等价
    /// 状态——两侧都走 `AdapterManagement::invoke_custom`，单源下发。
    #[tokio::test]
    async fn dual_run_old_and_new_gate_commands_produce_equivalent_state() {
        let (agent, events) = agent_with_qq_instances();

        // 旧路径：SetQqGateMode
        agent
            .apply_qq_command(BackendCommand::SetQqGateMode {
                mode: echo_adapter::GateMode::Allowlist,
                adapter: Some("qq-a".into()),
            })
            .await;
        // 广播应把另一实例也对齐（共享段语义）。
        let ad_b = agent.adapters.get("qq-b").expect("qq-b");
        assert_eq!(
            ad_b.management()
                .unwrap()
                .invoke_custom("get_gate_mode", serde_json::json!({}))
                .await
                .unwrap()["mode"],
            serde_json::json!("allowlist")
        );

        let emitted = drain_events(&events);
        // AdapterState（新通道）与派生旧事件都由同一状态单源产出。
        let states: Vec<_> = emitted
            .iter()
            .filter_map(|e| match e {
                BackendEvent::AdapterState { adapter, state } => Some((adapter, state)),
                _ => None,
            })
            .collect();
        assert_eq!(
            states.len(),
            1,
            "应恰好一条 AdapterState；实际 = {emitted:?}"
        );
        assert_eq!(states[0].0, "qq-a");
        assert_eq!(states[0].1["gate"]["mode"], serde_json::json!("allowlist"));
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
        assert_eq!(legacy.len(), 3, "派生旧事件（mode/filter/owner）应齐备");
        // W2.4：派生旧事件 id 一律字符串（owner 未设置 = ""，替代旧 0）。
        let owner = emitted
            .iter()
            .find_map(|e| match e {
                BackendEvent::QqOwner { owner_qq, .. } => Some(owner_qq.clone()),
                _ => None,
            })
            .expect("QqOwner event");
        assert_eq!(owner, "");

        // 新路径：InvokeAdapterOp 改回 none——与旧命令同一执行面（invoke_custom）。
        agent
            .apply_qq_command(BackendCommand::InvokeAdapterOp {
                adapter: "qq-a".into(),
                method: "set_gate_mode".into(),
                payload: serde_json::json!({"mode": "none"}),
            })
            .await;
        let emitted = drain_events(&events);
        let results: Vec<_> = emitted
            .iter()
            .filter_map(|e| match e {
                BackendEvent::AdapterOpResult {
                    method,
                    result,
                    error,
                    ..
                } => Some((method, result, error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            results.len(),
            1,
            "应恰好一条 AdapterOpResult；实际 = {emitted:?}"
        );
        assert_eq!(results[0].0, "set_gate_mode");
        assert!(results[0].2.is_none(), "新命令应成功");
        // 状态等价（新命令产生与旧命令同形态的状态文档）。
        let states: Vec<_> = emitted
            .iter()
            .filter_map(|e| match e {
                BackendEvent::AdapterState { state, .. } => Some(state),
                _ => None,
            })
            .collect();
        assert_eq!(states[0]["gate"]["mode"], serde_json::json!("none"));
        // 两实例保持对齐。
        let ad_a = agent.adapters.get("qq-a").unwrap();
        assert_eq!(
            ad_a.management()
                .unwrap()
                .invoke_custom("get_gate_mode", serde_json::json!({}))
                .await
                .unwrap()["mode"],
            serde_json::json!("none")
        );
    }

    /// InvokeAdapterOp：写类方法（owner）对齐其余实例并回执成功。
    #[tokio::test]
    async fn invoke_adapter_op_write_method_broadcasts_and_acks() {
        let (agent, events) = agent_with_qq_instances();
        agent
            .apply_qq_command(BackendCommand::InvokeAdapterOp {
                adapter: "qq-a".into(),
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
        // 其余实例已对齐（best-effort 广播）。
        let ad_b = agent.adapters.get("qq-b").unwrap();
        assert_eq!(
            ad_b.management()
                .unwrap()
                .invoke_custom("get_owner", serde_json::json!({}))
                .await
                .unwrap()["owner_qq"],
            serde_json::json!(9527)
        );
    }

    /// W2.4：SetQqOwner 字符串载荷透传（number 双读已由协议层归一）；
    /// 空串 = 清除语义（消息文案区分），派生 QqOwner 输出字符串 id。
    #[tokio::test]
    async fn set_qq_owner_string_payload_and_clear_semantics() {
        let (agent, events) = agent_with_qq_instances();

        agent
            .apply_qq_command(BackendCommand::SetQqOwner {
                owner_qq: "9527".into(),
                adapter: Some("qq-a".into()),
            })
            .await;
        // 实例侧（mock 复刻插件 parse_id）：字符串 id 可解析并广播对齐。
        let ad_b = agent.adapters.get("qq-b").expect("qq-b");
        assert_eq!(
            ad_b.management()
                .unwrap()
                .invoke_custom("get_owner", serde_json::json!({}))
                .await
                .unwrap()["owner_qq"],
            serde_json::json!(9527)
        );
        let emitted = drain_events(&events);
        let owner_ev = emitted
            .iter()
            .find_map(|e| match e {
                BackendEvent::QqOwner { owner_qq, .. } => Some(owner_qq.clone()),
                _ => None,
            })
            .expect("QqOwner event");
        assert_eq!(owner_ev, "9527", "owner 事件输出字符串 id");
        let messages: Vec<&String> = emitted
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Error { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        assert!(
            messages.iter().any(|m| m.contains("已设置为")),
            "got {messages:?}"
        );

        // 空串 = 清除（替代旧 0 语义）。
        agent
            .apply_qq_command(BackendCommand::SetQqOwner {
                owner_qq: String::new(),
                adapter: Some("qq-a".into()),
            })
            .await;
        let emitted = drain_events(&events);
        let owner_ev = emitted
            .iter()
            .find_map(|e| match e {
                BackendEvent::QqOwner { owner_qq, .. } => Some(owner_qq.clone()),
                _ => None,
            })
            .expect("QqOwner event");
        assert_eq!(owner_ev, "", "清除后 owner 输出空串");
        let messages: Vec<&String> = emitted
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Error { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        assert!(
            messages.iter().any(|m| m.contains("已清除")),
            "got {messages:?}"
        );
    }

    /// C1（泛化 02a §7.2）：`InvokeAdapterOp` 失败路径回执
    /// `AdapterOpResult{result: null, error: 非空}`（不再发 `Error`）；
    /// 方法执行失败与实例解析失败同理（后者用命令自带 adapter/method 回显）。
    #[tokio::test]
    async fn invoke_adapter_op_failure_acks_with_error_result() {
        let (agent, events) = agent_with_qq_instances();

        // 方法执行失败（mock 对未知方法返回 Err）。
        agent
            .apply_qq_command(BackendCommand::InvokeAdapterOp {
                adapter: "qq-a".into(),
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
        assert_eq!(adapter, "qq-a");
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
            .apply_qq_command(BackendCommand::InvokeAdapterOp {
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

        // 约束：旧 QQ 9 命令臂失败行为保持不动（仍 Error，无 AdapterOpResult）。
        agent
            .apply_qq_command(BackendCommand::SetQqGateMode {
                mode: echo_adapter::GateMode::None,
                adapter: Some("missing".into()),
            })
            .await;
        let emitted = drain_events(&events);
        assert!(
            emitted
                .iter()
                .any(|e| matches!(e, BackendEvent::Error { .. })),
            "旧命令失败仍发 Error；实际 = {emitted:?}"
        );
        assert!(
            !emitted
                .iter()
                .any(|e| matches!(e, BackendEvent::AdapterOpResult { .. })),
            "旧命令失败不发 AdapterOpResult；实际 = {emitted:?}"
        );
    }

    /// C2（泛化 02a §7.2 D2）：读路径 `get_state` 成功完成后推送
    /// `AdapterState`（+ 派生旧事件，单源）；其它读方法不推。
    #[tokio::test]
    async fn invoke_adapter_op_get_state_pushes_adapter_state() {
        let (agent, events) = agent_with_qq_instances();

        agent
            .apply_qq_command(BackendCommand::InvokeAdapterOp {
                adapter: "qq-a".into(),
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
        assert_eq!(states[0].0, "qq-a");
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
            "旧事件应与状态同源派生；实际 = {emitted:?}"
        );

        // 另一读方法（get_filter_config）成功但**不**推 AdapterState。
        agent
            .apply_qq_command(BackendCommand::InvokeAdapterOp {
                adapter: "qq-a".into(),
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

    /// 纯函数 `derive_legacy_qq_state_events`：number|string 宽容读取、
    /// 缺省门控 → none/空表、owner 0 → 空串（W2.4 语义）。
    #[test]
    fn derive_legacy_state_events_shapes() {
        let events = super::derive_legacy_qq_state_events(
            &serde_json::json!({
                "gate": {
                    "mode": "denylist",
                    "allowlist_users": [123, "456"],
                    "denylist_users": ["789"],
                },
                "owner": 0,
            }),
            Some("qq-a".into()),
        );
        assert_eq!(events.len(), 3);
        match &events[0] {
            BackendEvent::QqFilterConfig {
                allowlist_users,
                denylist_users,
                adapter,
                ..
            } => {
                assert_eq!(allowlist_users, &vec!["123".to_string(), "456".to_string()]);
                assert_eq!(denylist_users, &vec!["789".to_string()]);
                assert_eq!(adapter.as_deref(), Some("qq-a"));
            }
            other => panic!("QqFilterConfig expected, got {other:?}"),
        }
        match &events[1] {
            BackendEvent::QqGateMode { mode, .. } => {
                assert_eq!(*mode, echo_adapter::GateMode::Denylist);
            }
            other => panic!("QqGateMode expected, got {other:?}"),
        }
        match &events[2] {
            BackendEvent::QqOwner { owner_qq, .. } => assert_eq!(owner_qq, ""),
            other => panic!("QqOwner expected, got {other:?}"),
        }
    }
}
