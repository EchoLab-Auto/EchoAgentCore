//! Frontend command handling (`BackendCommand` → agent actions).
//!
//! Split out of `mod.rs` so the command dispatch — the largest single
//! function in the agent — lives in its own file.

use crate::agent::Agent;
use crate::command::BackendCommand;
use crate::event::BackendEvent;
use crate::session::SessionKey;

impl Agent {
    /// Emit the current trunk context snapshot (`BackendEvent::ContextSnapshot`).
    async fn emit_context_snapshot(&self) {
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
        let blocks = self.context_blocks(&history).await;
        self.emit(BackendEvent::ContextSnapshot {
            messages,
            blocks,
            total_tokens,
            limit_tokens: self.trunk.memory_limit_tokens(),
        });
    }

    pub async fn apply_command(&self, cmd: BackendCommand) {
        match cmd {
            BackendCommand::SwitchModel { model } => {
                self.config.write().await.model = model.clone();
                self.set_model(model.clone()).await;
                self.emit_api_config().await;
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("switched model: {model}"),
                });
                let cfg = self.config.read().await.clone();
                self.persist_config(&cfg).await;
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
                self.persist_system_prompt_plugin(&prompt).await;
                self.emit(BackendEvent::SystemPrompt { text: prompt });
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "system prompt updated".into(),
                });
            }
            BackendCommand::RequestSystemPrompt => {
                let text = self.config.read().await.system_prompt.clone();
                self.emit(BackendEvent::SystemPrompt { text });
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
            BackendCommand::TestApi { name } => {
                self.test_api_config(&name).await;
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
                self.emit_context_snapshot().await;
            }
            BackendCommand::RequestTrunkTimeline => {
                self.emit(BackendEvent::TrunkTimeline {
                    messages: self.trunk.timeline_snapshot(),
                });
            }
            BackendCommand::ClearHistory => {
                self.trunk.clear_history().await;
                // Push the empty projections so every connected frontend drops
                // its local copy immediately (chat view + open context modal).
                self.emit(BackendEvent::TrunkTimeline {
                    messages: Vec::new(),
                });
                self.emit_context_snapshot().await;
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "历史记忆已清理".into(),
                });
            }
            BackendCommand::CancelRequestedWork { session_id, all } => {
                let cancelled = self.cancel_requested_work(&session_id, all).await;
                self.emit(BackendEvent::Error {
                    session_id: Some(session_id.clone()),
                    message: format!("已取消 {cancelled} 个进行中的任务"),
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
            BackendCommand::UpdateQqAllowlist { .. }
            | BackendCommand::UpdateQqDenylist { .. }
            | BackendCommand::SetQqGateMode { .. }
            | BackendCommand::SetQqOwner { .. }
            | BackendCommand::RequestQqOwner
            | BackendCommand::RequestQqFilterConfig
            | BackendCommand::RequestGroupList
            | BackendCommand::RequestFriendList => {
                self.apply_qq_command(cmd).await;
            }
        }
    }
}
