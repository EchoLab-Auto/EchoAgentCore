//! QQ-specific frontend command handling.
//!
//! The QQ command variants (`UpdateQqAllowlist`/`UpdateQqDenylist`/
//! `SetQqGateMode`/`RequestQqFilterConfig`/`RequestGroupList`/
//! `RequestFriendList`) are dispatched here, out of the main `apply_command`
//! match. A new platform's commands get their own handler module; the main
//! dispatcher stays a thin delegator.

use crate::agent::Agent;
use crate::command::BackendCommand;
use crate::event::BackendEvent;

impl Agent {
    /// Handle the QQ-specific command variants. Called by `apply_command`
    /// when the command is one of the QQ domain.
    pub(crate) async fn apply_qq_command(&self, cmd: BackendCommand) {
        match cmd {
            BackendCommand::UpdateQqAllowlist {
                user_ids,
                group_ids,
            } => {
                tracing::info!(users = ?user_ids, groups = ?group_ids, "Agent: UpdateQqAllowlist");
                let user_strs: Vec<String> = user_ids.iter().map(|id| id.to_string()).collect();
                let group_strs: Vec<String> = group_ids.iter().map(|id| id.to_string()).collect();
                if let Some(adapter) = self.adapters.get("qq") {
                    adapter.update_allowlist(user_strs, group_strs);
                    self.emit_qq_filter_config(adapter);
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: "QQ 白名单已更新".into(),
                    });
                } else {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: "QQ 适配器未找到".into(),
                    });
                }
            }
            BackendCommand::UpdateQqDenylist {
                user_ids,
                group_ids,
            } => {
                tracing::info!(users = ?user_ids, groups = ?group_ids, "Agent: UpdateQqDenylist");
                let user_strs: Vec<String> = user_ids.iter().map(|id| id.to_string()).collect();
                let group_strs: Vec<String> = group_ids.iter().map(|id| id.to_string()).collect();
                if let Some(adapter) = self.adapters.get("qq") {
                    adapter.update_denylist(user_strs, group_strs);
                    self.emit_qq_filter_config(adapter);
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: "QQ 黑名单已更新".into(),
                    });
                } else {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: "QQ 适配器未找到".into(),
                    });
                }
            }
            BackendCommand::SetQqGateMode { mode } => {
                tracing::info!(mode = %mode.as_str(), "Agent: SetQqGateMode");
                if let Some(adapter) = self.adapters.get("qq") {
                    adapter.set_gate_mode(mode);
                    self.emit(BackendEvent::QqGateMode {
                        mode: adapter.get_gate_mode(),
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
            }
            BackendCommand::SetQqOwner { owner_qq } => {
                tracing::info!(owner_qq, "Agent: SetQqOwner");
                if let Some(adapter) = self.adapters.get("qq") {
                    adapter.set_owner_qq(owner_qq);
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: if owner_qq > 0 {
                            format!("QQ 管理员已设置为: {owner_qq}")
                        } else {
                            "QQ 管理员已清除".into()
                        },
                    });
                } else {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: "QQ 适配器未找到".into(),
                    });
                }
            }
            BackendCommand::RequestQqOwner => {
                if let Some(adapter) = self.adapters.get("qq") {
                    let owner_qq = adapter.get_owner_qq();
                    self.emit(BackendEvent::QqOwner { owner_qq });
                } else {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: "QQ 适配器未找到".into(),
                    });
                }
            }
            BackendCommand::RequestQqFilterConfig => {
                if let Some(adapter) = self.adapters.get("qq") {
                    self.emit_qq_filter_config(adapter);
                    self.emit(BackendEvent::QqGateMode {
                        mode: adapter.get_gate_mode(),
                    });
                }
            }
            BackendCommand::RequestGroupList => {
                let groups = match self.adapters.get("qq") {
                    Some(adapter) => match adapter.get_all_groups().await {
                        // ^^^ privileged: bypasses gate filtering so the TUI can
                        //     always show all groups for allowlist/denylist management.
                        Ok(groups) => {
                            let result: Vec<crate::event::GroupInfo> = groups
                                .into_iter()
                                .map(|(gid, name)| crate::event::GroupInfo {
                                    group_id: gid,
                                    group_name: name,
                                })
                                .collect();
                            result
                        }
                        Err(e) => {
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("failed to get group list: {e}"),
                            });
                            Vec::new()
                        }
                    },
                    None => {
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: "QQ adapter not found".into(),
                        });
                        Vec::new()
                    }
                };
                // Always emit the event — even an empty list tells the
                // TUI to replace "(正在获取…)" with "(无)".
                self.emit(BackendEvent::GroupList { groups });
            }
            BackendCommand::RequestFriendList => {
                if let Some(adapter) = self.adapters.get("qq") {
                    match adapter.get_all_friends().await {
                        // ^^^ privileged: returns all friends unfiltered for TUI picker.
                        Ok(friends) => {
                            let result: Vec<crate::event::FriendInfo> = friends
                                .into_iter()
                                .map(|(uid, name)| crate::event::FriendInfo {
                                    user_id: uid,
                                    nickname: name,
                                })
                                .collect();
                            // Always emit FriendList event, even if empty, to mark as loaded
                            self.emit(BackendEvent::FriendList { friends: result });
                        }
                        Err(e) => {
                            // Emit empty friend list to mark as attempted and avoid infinite loading
                            self.emit(BackendEvent::FriendList { friends: vec![] });
                            // Show user-friendly error message
                            let error_msg = if e.to_string().contains("no QQ connection active") {
                                "QQ未连接，请先使用 /qq login 登录".to_string()
                            } else {
                                format!("获取好友列表失败: {e}")
                            };
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message: error_msg,
                            });
                        }
                    }
                }
            }
            other => {
                tracing::warn!(command = ?other, "QQ handler received a non-QQ command");
            }
        }
    }

    /// Emit the current QQ filter configuration from the adapter's live state.
    fn emit_qq_filter_config(&self, adapter: &std::sync::Arc<dyn echo_adapter::traits::Adapter>) {
        let (au, ag, du, dg) = adapter.get_filter_info();
        self.emit(BackendEvent::QqFilterConfig {
            allowlist_users: au.iter().filter_map(|s| s.parse().ok()).collect(),
            allowlist_groups: ag.iter().filter_map(|s| s.parse().ok()).collect(),
            denylist_users: du.iter().filter_map(|s| s.parse().ok()).collect(),
            denylist_groups: dg.iter().filter_map(|s| s.parse().ok()).collect(),
        });
    }
}
