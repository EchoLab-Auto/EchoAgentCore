//! Frontend command handling (`BackendCommand` → agent actions).
//!
//! Split out of `mod.rs` so the command dispatch — the largest single
//! function in the agent — lives in its own file.

use crate::agent::Agent;
use crate::command::BackendCommand;
use crate::event::BackendEvent;
use crate::session::SessionKey;

impl Agent {
    pub async fn apply_command(&self, cmd: BackendCommand) {
        match cmd {
            BackendCommand::SwitchModel { model } => {
                self.set_model(model.clone()).await;
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("switched model: {model}"),
                });
            }
            BackendCommand::SwitchProvider { provider } => {
                let mut config = self.config.write().await;
                let old_provider = config.provider.clone();
                config.provider = provider.clone();
                let cfg = config.clone();
                drop(config);
                if self.rebuild_provider(&cfg).await {
                    let model = cfg.model.clone();
                    let base_url = cfg.effective_base_url();
                    let key_set = !cfg.effective_api_key().is_empty();
                    self.set_model(model).await;
                    self.emit_api_config().await;
                    self.persist_config(&cfg).await;
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: format!(
                            "switched provider: {provider} ({base_url}, key: {})",
                            if key_set { "***" } else { "not set" }
                        ),
                    });
                } else {
                    let mut config = self.config.write().await;
                    config.provider = old_provider;
                }
            }
            BackendCommand::SetSystemPrompt { prompt } => {
                self.config.write().await.system_prompt = prompt.clone();
                *self.system_prompt_cache.write().await = None;
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "system prompt updated".into(),
                });
            }
            BackendCommand::ToggleSkill { name, enabled } => {
                let mut skills = self.skills.lock().await;
                let ok = skills.set_enabled(&name, enabled);
                *self.system_prompt_cache.write().await = None;
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: if ok {
                        format!(
                            "skill {name} {}",
                            if enabled { "enabled" } else { "disabled" }
                        )
                    } else {
                        format!("skill {name} not found")
                    },
                });
            }
            BackendCommand::UpdateApiConfig {
                name,
                provider,
                model,
                base_url,
                api_key,
                thinking,
                reasoning_effort,
            } => {
                self.update_api_config(
                    name,
                    provider,
                    model,
                    base_url,
                    api_key,
                    thinking,
                    reasoning_effort,
                )
                .await;
            }
            BackendCommand::SwitchApi { name } => {
                self.switch_api(&name).await;
            }
            BackendCommand::DeleteApi { name } => {
                self.delete_api(&name).await;
            }
            BackendCommand::RequestState => {
                self.emit_api_config().await;
                for s in self.trunk.all() {
                    let last = s
                        .history
                        .lock()
                        .await
                        .last()
                        .cloned()
                        .map(|m| m.content)
                        .unwrap_or_default();
                    self.emit(BackendEvent::SessionUpdated {
                        session: s.info(last),
                    });
                }
                // Also emit adapter status and QQ gate/filter state so the
                // TUI starts up with the correct persisted values.
                self.emit_adapter_list();
                if let Some(adapter) = self.adapters.get("qq") {
                    let (au, ag, du, dg) = adapter.get_filter_info();
                    self.emit(BackendEvent::QqFilterConfig {
                        allowlist_users: au.iter().filter_map(|s| s.parse().ok()).collect(),
                        allowlist_groups: ag.iter().filter_map(|s| s.parse().ok()).collect(),
                        denylist_users: du.iter().filter_map(|s| s.parse().ok()).collect(),
                        denylist_groups: dg.iter().filter_map(|s| s.parse().ok()).collect(),
                    });
                    self.emit(BackendEvent::QqGateMode {
                        mode: adapter.get_gate_mode(),
                    });
                }
            }
            BackendCommand::RequestContext => {
                let history = self.trunk.snapshot().await;
                let total_tokens = crate::llm::estimate_history_tokens(&history);
                let messages = history
                    .iter()
                    .map(|message| crate::event::ContextMessageInfo {
                        role: match message.role {
                            crate::llm::ChatRole::System => "system",
                            crate::llm::ChatRole::User => "user",
                            crate::llm::ChatRole::Assistant => "assistant",
                            crate::llm::ChatRole::Tool => "tool",
                        }
                        .into(),
                        content: message.content.clone(),
                        tokens: crate::llm::estimate_message_tokens(message),
                        sequence: crate::agent::structured_message_sequence(&message.content),
                    })
                    .collect();
                self.emit(BackendEvent::ContextSnapshot {
                    messages,
                    total_tokens,
                    limit_tokens: self.trunk.memory_limit_tokens(),
                });
            }
            BackendCommand::RequestTrunkTimeline => {
                self.emit(BackendEvent::TrunkTimeline {
                    messages: self.trunk.timeline_snapshot(),
                });
            }
            BackendCommand::SendMessage {
                session_id,
                content,
            } => {
                // Parse or fall back to local TUI session key.
                let key = SessionKey::parse(&session_id).unwrap_or_else(SessionKey::local_tui);
                let session = self.trunk.get_or_create(&key, "local user".into(), None);
                let sid = session.id.clone();
                let received_at_ms = chrono::Utc::now().timestamp_millis();
                let message_sequence = self.next_message_sequence();
                // Notify TUI that a session exists so it can set active_session_id.
                self.emit(BackendEvent::SessionUpdated {
                    session: session.info(String::new()),
                });
                self.emit(BackendEvent::MessageReceived {
                    session_id: sid.clone(),
                    adapter_name: "local".into(),
                    platform: key.platform.clone(),
                    user_id: key.user_id.clone(),
                    user_name: "local user".into(),
                    channel: match &*key.scope {
                        "group" => format!("group:{}", key.scope_id),
                        _ => "direct".into(),
                    },
                    group_name: None,
                    content: content.clone(),
                    timestamp: received_at_ms / 1000,
                    received_at_ms,
                    message_sequence,
                });
                let backend_input = serde_json::json!({
                    "event": "backend_message",
                    "message_sequence": message_sequence,
                    "received_at_ms": received_at_ms,
                    "session": {
                        "id": sid,
                        "platform": key.platform,
                        "scope": key.scope,
                        "scope_id": key.scope_id,
                        "user_id": key.user_id
                    },
                    "content": content
                });
                let backend_input =
                    format!("<backend_message_hook>{backend_input}</backend_message_hook>");
                tracing::info!(
                    message_sequence,
                    session = %session.id,
                    received_at_ms,
                    "backend message received"
                );
                match self.process_message(&session, &backend_input).await {
                    Ok(reply) => {
                        self.emit(BackendEvent::AgentOutput {
                            session_id: sid,
                            content: reply,
                            branch_id: None,
                        });
                    }
                    Err(e) => {
                        self.emit(BackendEvent::Error {
                            session_id: Some(sid),
                            message: e.to_string(),
                        });
                    }
                }
            }
            // ---- Adapter management ----
            BackendCommand::StartAdapter { name } => match self.adapters.get(&name) {
                Some(adapter) => {
                    match adapter.start().await {
                        Ok(()) => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("adapter {} started", adapter.display_name()),
                        }),
                        Err(e) => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!(
                                "adapter {} start failed: {e}",
                                adapter.display_name()
                            ),
                        }),
                    }
                    self.emit_adapter_list();
                }
                None => self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("adapter {name} not found"),
                }),
            },
            BackendCommand::StopAdapter { name } => match self.adapters.get(&name) {
                Some(adapter) => {
                    match adapter.stop().await {
                        Ok(()) => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("adapter {} stopped", adapter.display_name()),
                        }),
                        Err(e) => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("adapter {} stop failed: {e}", adapter.display_name()),
                        }),
                    }
                    self.emit_adapter_list();
                }
                None => self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("adapter {name} not found"),
                }),
            },
            BackendCommand::RestartAdapter { name } => match self.adapters.get(&name) {
                Some(adapter) => {
                    match adapter.stop().await {
                        Ok(()) => match adapter.start().await {
                            Ok(()) => self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("adapter {} restarted", adapter.display_name()),
                            }),
                            Err(e) => self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!(
                                    "adapter {} restart (start) failed: {e}",
                                    adapter.display_name()
                                ),
                            }),
                        },
                        Err(e) => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!(
                                "adapter {} restart (stop) failed: {e}",
                                adapter.display_name()
                            ),
                        }),
                    }
                    self.emit_adapter_list();
                }
                None => self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("adapter {name} not found"),
                }),
            },
            BackendCommand::RequestAdapterStatus => {
                self.emit_adapter_list();
            }
            BackendCommand::StartAllAdapters => {
                let results = self.adapters.start_all().await;
                for (name, result) in &results {
                    match result {
                        Ok(()) => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("adapter {name} started"),
                        }),
                        Err(e) => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("adapter {name}: {e}"),
                        }),
                    }
                }
                self.emit_adapter_list();
            }
            BackendCommand::StopAllAdapters => {
                self.adapters.stop_all().await;
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "all adapters stopped".into(),
                });
                self.emit_adapter_list();
            }
            BackendCommand::UpdateQqAllowlist {
                user_ids,
                group_ids,
            } => {
                tracing::info!(users = ?user_ids, groups = ?group_ids, "Agent: UpdateQqAllowlist");
                let user_strs: Vec<String> = user_ids.iter().map(|id| id.to_string()).collect();
                let group_strs: Vec<String> = group_ids.iter().map(|id| id.to_string()).collect();
                if let Some(adapter) = self.adapters.get("qq") {
                    adapter.update_allowlist(user_strs, group_strs);
                    let (au, ag, du, dg) = adapter.get_filter_info();
                    self.emit(BackendEvent::QqFilterConfig {
                        allowlist_users: au.iter().filter_map(|s| s.parse().ok()).collect(),
                        allowlist_groups: ag.iter().filter_map(|s| s.parse().ok()).collect(),
                        denylist_users: du.iter().filter_map(|s| s.parse().ok()).collect(),
                        denylist_groups: dg.iter().filter_map(|s| s.parse().ok()).collect(),
                    });
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
                    let (au, ag, du, dg) = adapter.get_filter_info();
                    self.emit(BackendEvent::QqFilterConfig {
                        allowlist_users: au.iter().filter_map(|s| s.parse().ok()).collect(),
                        allowlist_groups: ag.iter().filter_map(|s| s.parse().ok()).collect(),
                        denylist_users: du.iter().filter_map(|s| s.parse().ok()).collect(),
                        denylist_groups: dg.iter().filter_map(|s| s.parse().ok()).collect(),
                    });
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
            BackendCommand::RequestQqFilterConfig => {
                if let Some(adapter) = self.adapters.get("qq") {
                    let (au, ag, du, dg) = adapter.get_filter_info();
                    self.emit(BackendEvent::QqFilterConfig {
                        allowlist_users: au.iter().filter_map(|s| s.parse().ok()).collect(),
                        allowlist_groups: ag.iter().filter_map(|s| s.parse().ok()).collect(),
                        denylist_users: du.iter().filter_map(|s| s.parse().ok()).collect(),
                        denylist_groups: dg.iter().filter_map(|s| s.parse().ok()).collect(),
                    });
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
        }
    }
}
