//! 迁移兼容层：旧 QQ 命令变体的处理臂（**N+2 随协议变体删除；grep 验收豁免**）。
//!
//! 泛化 S5（01b §D.1）：10 个旧变体（`UpdateQqAllowlist` / `UpdateQqDenylist` /
//! `RequestQqFilterConfig` / `SetQqGateMode` / `SetQqOwner` / `RequestQqOwner` /
//! `RequestQqLoginStatus` / `RequestQqQrcode` / `RequestGroupList` /
//! `RequestFriendList`）的处理臂从 `commands.rs` 隔离到本文件：
//!
//! - 旧臂保留历史行为与全部用户可见文案（含缺省回退的
//!   `platform() == "qq"` 过滤——旧客户端只寻址 QQ 实例）；
//! - **版本 N+2** 随 echo-protocol 旧变体物理删除（01b §B 删除计划），
//!   届时本文件整体移除，`commands.rs` 仅剩通用 `InvokeAdapterOp` 臂；
//! - 依据 01b §D.5 豁免口径，本文件属「迁移兼容层」，不参与
//!   `grep -riE 'qq|onebot|napcat'` 零残留验收（N+2 去掉 legacy 豁免）。
//!
//! 迁移期旧事件（`QqFilterConfig` / `QqGateMode` / `QqOwner` 等）的单源派生
//! [`derive_legacy_qq_state_events`] 同属本层资产——`commands.rs` 的
//! `emit_adapter_state_events` 与 core 组合根的插件推送路径共用它，
//! N+2 与旧变体一并删除。

use crate::agent::Agent;
use crate::command::BackendCommand;
use crate::event::BackendEvent;

use super::commands::invoke_adapter_method;

impl Agent {
    /// 解析旧 QQ 命令要操作的实例：显式名字优先；缺省时唯一 QQ 实例回退
    /// （历史行为：只计 `platform() == "qq"` 的实例；兼容层豁免）。
    fn resolve_legacy_qq_adapter(
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

    /// 处理旧 QQ 域命令；非本域命令原样返回（交 `commands.rs` 通用分发）。
    ///
    /// 返回值约定：`None` = 本域命令已处理（含错误事件已发）；`Some(cmd)`
    /// = 非旧域命令，调用方继续匹配。
    pub(crate) async fn apply_legacy_command(
        &self,
        cmd: BackendCommand,
    ) -> Option<BackendCommand> {
        match cmd {
            BackendCommand::UpdateQqAllowlist {
                user_ids,
                group_ids,
                adapter,
            } => {
                tracing::info!(users = ?user_ids, groups = ?group_ids, adapter = ?adapter, "Agent: UpdateQqAllowlist");
                match self.resolve_legacy_qq_adapter(&adapter) {
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
                            return None;
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
                None
            }
            BackendCommand::UpdateQqDenylist {
                user_ids,
                group_ids,
                adapter,
            } => {
                tracing::info!(users = ?user_ids, groups = ?group_ids, adapter = ?adapter, "Agent: UpdateQqDenylist");
                match self.resolve_legacy_qq_adapter(&adapter) {
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
                            return None;
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
                None
            }
            BackendCommand::SetQqGateMode { mode, adapter } => {
                // W3.3/W3.4 参考实现：旧命令经适配器管理通道执行（管理面
                // invoke_custom）+ 状态下发单源化（AdapterState 单源派生）。
                tracing::info!(mode = %mode.as_str(), adapter = ?adapter, "Agent: SetQqGateMode");
                match self.resolve_legacy_qq_adapter(&adapter) {
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
                            return None;
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
                None
            }
            BackendCommand::SetQqOwner { owner_qq, adapter } => {
                tracing::info!(owner_qq = %owner_qq, adapter = ?adapter, "Agent: SetQqOwner");
                match self.resolve_legacy_qq_adapter(&adapter) {
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
                            return None;
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
                None
            }
            BackendCommand::RequestQqOwner { adapter } => {
                match self.resolve_legacy_qq_adapter(&adapter) {
                    Ok(ad) => {
                        // 读路径（W3.4）：owner 由 cached_state 单源投影（与
                        // emit_adapter_state_events 同一状态文档）。
                        let Some(state) = ad.management().and_then(|m| m.cached_state()) else {
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("适配器 {} 缺少管理面实现", ad.name()),
                            });
                            return None;
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
                }
                None
            }
            BackendCommand::RequestQqFilterConfig { adapter } => {
                match self.resolve_legacy_qq_adapter(&adapter) {
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
                None
            }
            BackendCommand::RequestGroupList { adapter } => {
                match self.resolve_legacy_qq_adapter(&adapter) {
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
                None
            }
            BackendCommand::RequestFriendList { adapter } => {
                match self.resolve_legacy_qq_adapter(&adapter) {
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
                None
            }
            BackendCommand::RequestQqLoginStatus { adapter } => {
                match self.resolve_legacy_qq_adapter(&adapter) {
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
                None
            }
            BackendCommand::RequestQqQrcode { adapter } => {
                match self.resolve_legacy_qq_adapter(&adapter) {
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
                None
            }
            // 其余命令不属于旧 QQ 域：原样交回（commands.rs 通用分发）。
            other => Some(other),
        }
    }
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
/// 随 Panel 阶段 1 结束与新通道稳定一并删除（见 02a §1-D3 阶段 4）；
/// 本函数属迁移兼容层（01b §D.5 豁免）。
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
    use crate::command::BackendCommand;
    use crate::event::BackendEvent;
    use crate::packages::adapter_ops::test_support::{agent_with_two_instances, drain_events};

    /// 双跑：旧命令（`SetQqGateMode`）与新命令（`InvokeAdapterOp`）产生等价
    /// 状态——两侧都走 `AdapterManagement::invoke_custom`，单源下发。
    #[tokio::test]
    async fn dual_run_old_and_new_gate_commands_produce_equivalent_state() {
        let (agent, events) = agent_with_two_instances();

        // 旧路径：SetQqGateMode
        agent
            .apply_adapter_command(BackendCommand::SetQqGateMode {
                mode: echo_adapter::GateMode::Allowlist,
                adapter: Some("mock-a".into()),
            })
            .await;
        // 广播应把另一实例也对齐（共享段语义）。
        let ad_b = agent.adapters.get("mock-b").expect("mock-b");
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
        assert_eq!(states[0].0, "mock-a");
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
            .apply_adapter_command(BackendCommand::InvokeAdapterOp {
                adapter: "mock-a".into(),
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
        let ad_a = agent.adapters.get("mock-a").unwrap();
        assert_eq!(
            ad_a.management()
                .unwrap()
                .invoke_custom("get_gate_mode", serde_json::json!({}))
                .await
                .unwrap()["mode"],
            serde_json::json!("none")
        );
    }

    /// W2.4：SetQqOwner 字符串载荷透传（number 双读已由协议层归一）；
    /// 空串 = 清除语义（消息文案区分），派生 QqOwner 输出字符串 id。
    #[tokio::test]
    async fn legacy_owner_string_payload_and_clear_semantics() {
        let (agent, events) = agent_with_two_instances();

        agent
            .apply_adapter_command(BackendCommand::SetQqOwner {
                owner_qq: "9527".into(),
                adapter: Some("mock-a".into()),
            })
            .await;
        // 实例侧（mock 复刻插件 parse_id）：字符串 id 可解析并广播对齐。
        let ad_b = agent.adapters.get("mock-b").expect("mock-b");
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
            .apply_adapter_command(BackendCommand::SetQqOwner {
                owner_qq: String::new(),
                adapter: Some("mock-a".into()),
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

    /// 约束：旧 QQ 命令臂失败行为保持不动（仍 `Error`，无 `AdapterOpResult`）。
    #[tokio::test]
    async fn legacy_command_failure_emits_error_without_op_result() {
        let (agent, events) = agent_with_two_instances();
        agent
            .apply_adapter_command(BackendCommand::SetQqGateMode {
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
            Some("mock-a".into()),
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
                assert_eq!(adapter.as_deref(), Some("mock-a"));
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
