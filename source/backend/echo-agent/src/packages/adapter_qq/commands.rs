//! QQ-specific frontend command handling.
//!
//! The QQ command variants (`UpdateQqAllowlist`/`UpdateQqDenylist`/
//! `SetQqGateMode`/`RequestQqFilterConfig`/`RequestGroupList`/
//! `RequestFriendList`/登录查询) are dispatched here, out of the main
//! `apply_command` match.
//!
//! **多实例寻址**（2026-09）：命令携带 `adapter: Option<String>`（QQ 实例名）。
//! - `Some(name)` → 精确寻址该实例；
//! - `None` → 若注册表里恰好只有一个 QQ 实例则用它（旧 Panel 行为不变），
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
                .cloned()
                .ok_or_else(|| format!("QQ 实例 {name} 未找到"));
        }
        let qq: Vec<std::sync::Arc<dyn echo_adapter::traits::Adapter>> = self
            .adapters
            .names()
            .into_iter()
            .filter_map(|n| self.adapters.get(&n).cloned())
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
                        let user_strs: Vec<String> =
                            user_ids.iter().map(|id| id.to_string()).collect();
                        let group_strs: Vec<String> =
                            group_ids.iter().map(|id| id.to_string()).collect();
                        ad.update_allowlist(user_strs, group_strs);
                        self.emit_qq_filter_config(&ad, adapter.clone());
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
                        let user_strs: Vec<String> =
                            user_ids.iter().map(|id| id.to_string()).collect();
                        let group_strs: Vec<String> =
                            group_ids.iter().map(|id| id.to_string()).collect();
                        ad.update_denylist(user_strs, group_strs);
                        self.emit_qq_filter_config(&ad, adapter.clone());
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
            BackendCommand::SetQqGateMode { mode, adapter } => {
                tracing::info!(mode = %mode.as_str(), adapter = ?adapter, "Agent: SetQqGateMode");
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => {
                        ad.set_gate_mode(mode);
                        self.emit(BackendEvent::QqGateMode {
                            mode: ad.get_gate_mode(),
                            adapter: adapter.clone(),
                        });
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
                tracing::info!(owner_qq, adapter = ?adapter, "Agent: SetQqOwner");
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => {
                        ad.set_owner_qq(owner_qq);
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: if owner_qq > 0 {
                                format!("QQ 管理员已设置为: {owner_qq}")
                            } else {
                                "QQ 管理员已清除".into()
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
                    let owner_qq = ad.get_owner_qq();
                    self.emit(BackendEvent::QqOwner {
                        owner_qq,
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
                        self.emit_qq_filter_config(&ad, adapter.clone());
                        self.emit(BackendEvent::QqGateMode {
                            mode: ad.get_gate_mode(),
                            adapter: adapter.clone(),
                        });
                    }
                    Err(message) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    }),
                }
            }
            BackendCommand::RequestGroupList { adapter } => {
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => match ad.get_all_groups().await {
                        Ok(groups) => self.emit(BackendEvent::GroupList {
                            groups: groups
                                .into_iter()
                                .map(|(id, name)| crate::event::GroupInfo {
                                    group_id: id,
                                    group_name: name,
                                })
                                .collect(),
                            adapter: adapter.clone(),
                        }),
                        Err(error) => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("获取群列表失败: {error}"),
                        }),
                    },
                    Err(message) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    }),
                }
            }
            BackendCommand::RequestFriendList { adapter } => {
                match self.resolve_qq_adapter(&adapter) {
                    Ok(ad) => match ad.get_all_friends().await {
                        Ok(friends) => self.emit(BackendEvent::FriendList {
                            friends: friends
                                .into_iter()
                                .map(|(id, name)| crate::event::FriendInfo {
                                    user_id: id,
                                    nickname: name,
                                })
                                .collect(),
                            adapter: adapter.clone(),
                        }),
                        Err(error) => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("获取好友列表失败: {error}"),
                        }),
                    },
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
                        // Core 代理取二维码：Adapter trait 提供默认 no-op 实现，
                        // QQ 适配器覆盖（docker exec 取 PNG）。
                        let name = ad.name().to_string();
                        match ad.login_qrcode_png().await {
                            Ok(png) => {
                                let encoded =
                                    crate::packages::adapter_qq::commands::encode_base64(&png);
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

    /// Emit the current QQ filter configuration from the adapter's live state.
    fn emit_qq_filter_config(
        &self,
        adapter: &std::sync::Arc<dyn echo_adapter::traits::Adapter>,
        instance: Option<String>,
    ) {
        let (au, ag, du, dg) = adapter.get_filter_info();
        self.emit(BackendEvent::QqFilterConfig {
            allowlist_users: au.iter().filter_map(|s| s.parse().ok()).collect(),
            allowlist_groups: ag.iter().filter_map(|s| s.parse().ok()).collect(),
            denylist_users: du.iter().filter_map(|s| s.parse().ok()).collect(),
            denylist_groups: dg.iter().filter_map(|s| s.parse().ok()).collect(),
            adapter: instance,
        });
    }
}

/// 极简 base64 编码（仅用于二维码 PNG 转发，避免给 echo-agent 增加依赖）。
pub(crate) fn encode_base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::encode_base64;

    #[test]
    fn base64_encodes_known_vectors() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
        assert_eq!(encode_base64(b"hello"), "aGVsbG8=");
    }
}
