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
                drop(skills);
                if ok {
                    // Persist the choice so it survives restarts.
                    let mut cfg = self.config.write().await;
                    cfg.disabled_skills.retain(|n| n != &name);
                    if !enabled {
                        cfg.disabled_skills.push(name.clone());
                    }
                    let cfg_snapshot = cfg.clone();
                    drop(cfg);
                    self.persist_config(&cfg_snapshot).await;
                    self.emit_skills_list().await;
                }
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
            BackendCommand::ToggleTool { name, enabled } => {
                let ok = self.tools.set_enabled(&name, enabled).await;
                if ok {
                    let mut cfg = self.config.write().await;
                    cfg.disabled_tools.retain(|n| n != &name);
                    if !enabled {
                        cfg.disabled_tools.push(name.clone());
                    }
                    let cfg_snapshot = cfg.clone();
                    drop(cfg);
                    self.persist_config(&cfg_snapshot).await;
                    self.emit_tools_list().await;
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: format!(
                            "tool {name} {}",
                            if enabled { "enabled" } else { "disabled" }
                        ),
                    });
                } else {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: format!("tool {name} not found"),
                    });
                }
            }
            BackendCommand::SaveSkill {
                name,
                description,
                keywords,
                always,
                category,
                content,
            } => {
                let dir = self.current_skills_dir().await;
                let draft = SkillDraft {
                    name: name.clone(),
                    description,
                    keywords,
                    always,
                    category,
                    content,
                };
                match self.save_skill_file(&dir, &draft) {
                    Ok(()) => {
                        if let Err(e) = self.reload_skills(&dir).await {
                            tracing::warn!(error = %e, "skill reload after save failed");
                        }
                        self.emit_skills_list().await;
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("skill {name} saved"),
                        });
                    }
                    Err(e) => {
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("save skill failed: {e}"),
                        });
                    }
                }
            }
            BackendCommand::DeleteSkill { name } => {
                let dir = self.current_skills_dir().await;
                match self.delete_skill_file(&dir, &name) {
                    Ok(()) => {
                        if let Err(e) = self.reload_skills(&dir).await {
                            tracing::warn!(error = %e, "skill reload after delete failed");
                        }
                        self.emit_skills_list().await;
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("skill {name} deleted"),
                        });
                    }
                    Err(e) => {
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("delete skill failed: {e}"),
                        });
                    }
                }
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
            BackendCommand::RequestSkillsList => {
                self.emit_skills_list().await;
            }
            BackendCommand::RequestToolsList => {
                self.emit_tools_list().await;
            }
            BackendCommand::RequestPluginsList => {
                self.emit_plugins_list().await;
            }
            BackendCommand::TogglePlugin { id, enabled } => {
                match self.plugin_host.set_enabled(&id, enabled).await {
                    Ok(()) => {
                        // Persist so the choice survives restarts.
                        let mut cfg = self.config.write().await;
                        cfg.disabled_plugins.retain(|p| p != &id);
                        if !enabled {
                            cfg.disabled_plugins.push(id.clone());
                        }
                        let cfg_snapshot = cfg.clone();
                        drop(cfg);
                        self.persist_config(&cfg_snapshot).await;
                        self.emit_plugins_list().await;
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!(
                                "plugin {id} {}",
                                if enabled { "enabled" } else { "disabled" }
                            ),
                        });
                    }
                    Err(e) => {
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("toggle plugin failed: {e}"),
                        });
                    }
                }
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
                images,
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
                    images: images.clone(),
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
                    "content": content,
                    "images": images
                });
                let backend_input = crate::input_marker::wrap_hook_value("backend", &backend_input);
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

    /// Emit the full skill list (`BackendEvent::SkillsList`).
    pub async fn emit_skills_list(&self) {
        let skills = self.skills.lock().await;
        let mut list: Vec<crate::event::SkillInfo> = skills
            .all()
            .iter()
            .map(|skill| crate::event::SkillInfo {
                name: skill.metadata.name.clone(),
                description: skill.metadata.description.clone(),
                keywords: skill.metadata.keywords.clone(),
                always: skill.metadata.always,
                enabled: skill.metadata.enabled,
                category: skill.metadata.category.clone(),
                content: skill.instructions.clone(),
            })
            .collect();
        list.sort_by_key(|s| s.name.clone());
        drop(skills);
        self.emit(BackendEvent::SkillsList { skills: list });
    }

    /// Emit the full tool list (`BackendEvent::ToolsList`), including
    /// disabled tools with their runtime enable state.
    pub async fn emit_tools_list(&self) {
        let defs = self.tools.full_definitions().await;
        let mut list: Vec<crate::event::ToolInfo> = defs
            .into_iter()
            .map(|(t, category, enabled)| crate::event::ToolInfo {
                name: t.name,
                description: t.description,
                parameters: t.parameters.unwrap_or(serde_json::Value::Null),
                category,
                enabled,
            })
            .collect();
        list.sort_by_key(|t| t.name.clone());
        self.emit(BackendEvent::ToolsList { tools: list });
    }

    /// Emit the full plugin list (`BackendEvent::PluginsList`).
    pub async fn emit_plugins_list(&self) {
        let plugins: Vec<crate::event::PluginInfo> = self
            .plugin_host
            .descriptors()
            .into_iter()
            .map(|d| crate::event::PluginInfo {
                id: d.id,
                name: d.name,
                version: d.version,
                kind: d.kind,
                description: d.description,
                entry: d.entry,
                author: d.author,
                enabled: d.enabled,
                builtin: d.builtin,
            })
            .collect();
        self.emit(BackendEvent::PluginsList { plugins });
    }

    /// Current configured skill directory (resolved from the live config).
    async fn current_skills_dir(&self) -> String {
        self.config.read().await.skills_dir.clone()
    }

    /// Persist a skill to `{skills_dir}/{name}/SKILL.md`.
    ///
    /// The frontmatter is regenerated from the metadata fields; the content
    /// is stored verbatim as the markdown body. Returns an error string on
    /// invalid names or filesystem failures.
    fn save_skill_file(&self, skills_dir: &str, draft: &SkillDraft) -> Result<(), String> {
        validate_skill_name(&draft.name)?;
        let dir = std::path::Path::new(skills_dir);
        let skill_dir = dir.join(&draft.name);
        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| format!("cannot create {}: {e}", skill_dir.display()))?;
        let keywords_str = if draft.keywords.is_empty() {
            String::new()
        } else {
            format!("keywords: [{}]\n", draft.keywords.join(", "))
        };
        let body = format!(
            "---\nname: {}\ndescription: {}\n{keywords_str}metadata:\n  always: {}\n  category: {}\n---\n{}",
            draft.name,
            draft.description,
            draft.always,
            draft.category,
            draft.content
        );
        std::fs::write(skill_dir.join("SKILL.md"), body)
            .map_err(|e| format!("cannot write SKILL.md: {e}"))?;
        Ok(())
    }

    /// Delete `{skills_dir}/{name}` (only when it is a skill directory inside
    /// the configured skills directory).
    fn delete_skill_file(&self, skills_dir: &str, name: &str) -> Result<(), String> {
        validate_skill_name(name)?;
        let root = std::path::Path::new(skills_dir);
        let canonical_root = root
            .canonicalize()
            .map_err(|e| format!("skills dir missing: {e}"))?;
        let skill_dir = root.join(name);
        let canonical_target = skill_dir
            .canonicalize()
            .map_err(|e| format!("skill {name} not found: {e}"))?;
        if !canonical_target.starts_with(&canonical_root) {
            return Err("refusing to delete outside skills directory".into());
        }
        if !canonical_target.join("SKILL.md").exists() {
            return Err(format!("{name} is not a skill directory"));
        }
        std::fs::remove_dir_all(&canonical_target)
            .map_err(|e| format!("cannot delete {name}: {e}"))?;
        Ok(())
    }
}

/// Draft payload for creating/updating a skill (`BackendCommand::SaveSkill`).
#[derive(Debug, Clone)]
pub(crate) struct SkillDraft {
    pub name: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub always: bool,
    pub category: String,
    pub content: String,
}

/// Skill name/directory key validation: Unicode letters/digits (covers Chinese),
/// `-`, `_` and spaces; no path separators, no leading dots.
fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("skill name is empty".into());
    }
    // 目录 key 校验：Unicode 字母/数字（支持中文名）+ '-' + '_'；
    // 拒绝路径分隔符、点开头（防路径穿越）与空白/控制字符。
    if name.starts_with('.')
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == ' ')
    {
        return Err(format!(
            "invalid skill name {name:?}: only Unicode letters/digits, '-', '_' allowed"
        ));
    }
    // 路径分隔符显式禁止（is_alphanumeric 不含它们，这里兜底明确语义）。
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("skill name must be a single directory key".into());
    }
    Ok(())
}

#[cfg(test)]
mod skill_name_tests {
    use super::validate_skill_name;

    #[test]
    fn accepts_unicode_chinese_names() {
        assert!(validate_skill_name("信息检索").is_ok());
        assert!(validate_skill_name("web-search").is_ok());
        assert!(validate_skill_name("my_skill-2").is_ok());
    }

    #[test]
    fn rejects_path_traversal_and_separators() {
        assert!(validate_skill_name("").is_err());
        assert!(validate_skill_name(".hidden").is_err());
        assert!(validate_skill_name("a/b").is_err());
        assert!(validate_skill_name("a\\b").is_err());
        assert!(validate_skill_name("..").is_err());
        assert!(validate_skill_name("../evil").is_err());
    }
}
