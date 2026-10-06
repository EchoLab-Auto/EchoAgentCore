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
    /// Public wrapper: forward a context snapshot request to this agent
    /// (used by the management agent for team-routed requests).
    /// 发出上下文快照（默认会话口径：本地 TUI；不存在时取任一已有会话）。
    pub async fn emit_context_snapshot_for(&self) {
        self.emit_context_snapshot(None).await;
    }

    /// 按会话发出上下文快照（多会话，2026-09）。
    /// `session_id` 缺省时回退本地 TUI 会话，再回退该智能体的任一已有会话。
    async fn emit_context_snapshot(&self, session_id: Option<&str>) {
        let resolved = match session_id {
            Some(id) if self.trunk.history_for(id).is_some() => Some(id.to_string()),
            Some(id) => Some(id.to_string()),
            None => {
                let local = crate::session::SessionKey::local_tui().to_session_id();
                if self.trunk.history_for(&local).is_some() {
                    Some(local)
                } else {
                    self.trunk.history_sessions().into_iter().next()
                }
            }
        };
        let history = match &resolved {
            Some(id) => self.trunk.snapshot_for(id).await,
            None => Vec::new(),
        };
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
            session_id: resolved,
            messages,
            blocks,
            total_tokens,
            limit_tokens: self.trunk.memory_limit_tokens(),
        });
    }

    /// 命令的目标 team_id（会话类命令；None = 未指定）。
    fn command_team_id(cmd: &BackendCommand) -> Option<&str> {
        match cmd {
            BackendCommand::SendMessage { team_id, .. }
            | BackendCommand::CancelRequestedWork { team_id, .. }
            | BackendCommand::RequestTrunkTimeline { team_id, .. }
            | BackendCommand::RequestContext { team_id, .. }
            | BackendCommand::ClearHistory { team_id }
            | BackendCommand::ArchiveHistory { team_id }
            | BackendCommand::CompactHistory { team_id, .. }
            | BackendCommand::RequestWorkspaceSessions { team_id }
            | BackendCommand::SaveWorkspaceSession { team_id, .. }
            | BackendCommand::DeleteWorkspaceSession { team_id, .. }
            | BackendCommand::ActivateWorkspaceSession { team_id, .. }
            | BackendCommand::RequestWorkspaceGitStatus { team_id, .. }
            | BackendCommand::RequestWorkspaceFiles { team_id, .. } => team_id.as_deref(),
            _ => None,
        }
    }

    /// 会话类命令是否必须显式指定 team_id（去主智能体后没有"默认人格"）。
    fn requires_explicit_team(cmd: &BackendCommand) -> bool {
        matches!(
            cmd,
            BackendCommand::SendMessage { .. }
                | BackendCommand::CancelRequestedWork { .. }
                | BackendCommand::RequestTrunkTimeline { .. }
                | BackendCommand::RequestContext { .. }
                | BackendCommand::ClearHistory { .. }
                | BackendCommand::ArchiveHistory { .. }
                | BackendCommand::CompactHistory { .. }
                | BackendCommand::RequestWorkspaceSessions { .. }
                | BackendCommand::SaveWorkspaceSession { .. }
                | BackendCommand::DeleteWorkspaceSession { .. }
                | BackendCommand::ActivateWorkspaceSession { .. }
                | BackendCommand::RequestWorkspaceGitStatus { .. }
                | BackendCommand::RequestWorkspaceFiles { .. }
        )
    }

    pub async fn apply_command(&self, cmd: BackendCommand) {
        // ---- 去主智能体：会话类命令按显式 team_id 路由 ----
        // 每个智能体一律平等；没有"默认人格"兜底，缺失 team_id 直接报错。
        if Self::requires_explicit_team(&cmd) {
            match Self::command_team_id(&cmd) {
                None => {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: "该命令必须指定 team_id（没有默认智能体；请在前端选择智能体）"
                            .into(),
                    });
                    return;
                }
                Some(target_id) => {
                    let current = self.team_id();
                    if current.as_deref() != Some(target_id) {
                        // 转发给目标人格（未知 team_id → 明确报错，不再回退默认）。
                        match crate::agent_manager::global_manager()
                            .and_then(|m| m.resolve(Some(target_id)))
                        {
                            Some(agent) => {
                                Box::pin(agent.apply_command(cmd)).await;
                                return;
                            }
                            None => {
                                self.emit(BackendEvent::Error {
                                    session_id: None,
                                    message: format!("智能体 {target_id} 不存在"),
                                });
                                return;
                            }
                        }
                    }
                }
            }
        }
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
                let ok = {
                    let mut skills = self.skills.lock().await;
                    skills.set_enabled(&name, enabled)
                };
                if ok {
                    *self.system_prompt_cache.write().await = None;
                    // Persist the choice so it survives restarts.
                    let mut cfg = self.config.write().await;
                    cfg.disabled_skills.retain(|n| n != &name);
                    if !enabled {
                        cfg.disabled_skills.push(name.clone());
                    }
                    let cfg_snapshot = cfg.clone();
                    drop(cfg);
                    self.persist_config(&cfg_snapshot).await;
                    // 全局策略（进程级）：所有 persona 重算时读同一份事实。
                    if let Some(policy) = crate::agent::global_policy() {
                        policy.set_skill_enabled(&name, enabled);
                    }
                    // 全局生效：最终状态 = 全局启停 ∧ 各 persona 名单，
                    // 逐 persona 重算（此前只作用于管理面，其他人格照旧）。
                    if let Some(mgr) = crate::agent_manager::global_manager() {
                        for running in mgr.all() {
                            running.agent.reapply_skill_gating(&name, enabled).await;
                        }
                    }
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
                    // 全局策略（进程级）：所有 persona 重算时读同一份事实。
                    if let Some(policy) = crate::agent::global_policy() {
                        policy.set_tool_enabled(&name, enabled);
                    }
                    // 全局生效：最终状态 = 全局启停 ∧ 各 persona 名单，
                    // 逐 persona 重算（此前只作用于管理面，其他人格照旧）。
                    if let Some(mgr) = crate::agent_manager::global_manager() {
                        for running in mgr.all() {
                            running.agent.reapply_tool_gating(&name, enabled).await;
                        }
                    }
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
                system,
            } => {
                let dir = self.current_skills_dir().await;
                let draft = SkillDraft {
                    name: name.clone(),
                    description,
                    keywords,
                    always,
                    category,
                    content,
                    system,
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
            BackendCommand::RequestRemoteApiProfiles { peer } => {
                // 分布式供应商共享（2026-10）：经联邦拉远端**脱敏**供应商
                // 池回传前端；peer 离线/旧版不识查询时 error 带原因。
                let result = match crate::federation::remote_querier() {
                    Some(querier) => {
                        querier(
                            peer.clone(),
                            echo_federation::QueryKind::ApiProfiles,
                            String::new(),
                        )
                        .await
                    }
                    None => Err("联邦未接线：远程供应商查询不可用".into()),
                };
                match result {
                    Ok(payload) => {
                        self.emit(BackendEvent::RemoteApiProfiles {
                            peer,
                            active: payload.get("active").cloned(),
                            profiles: payload
                                .get("profiles")
                                .and_then(|v| serde_json::from_value(v.clone()).ok())
                                .unwrap_or_default(),
                            error: None,
                        });
                    }
                    Err(message) => {
                        self.emit(BackendEvent::RemoteApiProfiles {
                            peer,
                            active: None,
                            profiles: Vec::new(),
                            error: Some(message),
                        });
                    }
                }
            }
            BackendCommand::TestApi { name } => {
                self.test_api_config(&name).await;
            }
            BackendCommand::QueryApiBalance { name } => {
                self.query_api_balance(&name).await;
            }
            BackendCommand::RequestSkillsList => {
                self.emit_skills_list().await;
            }
            BackendCommand::ReloadSkills => {
                let dir = self.current_skills_dir().await;
                // 技能目录是进程级共享的：重载必须覆盖**所有运行中人格**。
                // 此臂由管理代理（__core）执行，此前只重载了它自己的注册表——
                // 面板「重载技能」改完 SKILL.md 后，真正干活的人格仍停留在
                // 启动时的技能内容（实测：EchoCode 的上下文块不含新增技能）。
                let running = crate::agent_manager::global_manager()
                    .map(|manager| manager.all())
                    .unwrap_or_default();
                let mut targets: Vec<(&str, &Agent)> = running
                    .iter()
                    .map(|r| (r.id.as_str(), r.agent.as_ref()))
                    .collect();
                targets.push(("<core>", self));
                let (updated, checked, failures) = reload_skills_into(&dir, targets).await;
                self.emit_skills_list().await;
                let message = if !failures.is_empty() {
                    format!(
                        "技能重载部分失败（{updated}/{checked} 个更新）：{}",
                        failures.join("；")
                    )
                } else if updated == 0 {
                    "技能无变化".into()
                } else {
                    format!("技能已重新加载（{updated}/{checked} 个智能体更新）")
                };
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message,
                });
            }
            BackendCommand::RequestToolsList => {
                self.emit_tools_list().await;
            }
            BackendCommand::RequestPluginsList => {
                self.emit_plugins_list().await;
            }
            BackendCommand::TogglePlugin { id, enabled } => {
                // 防自锁：管理面插件承载 management WS，经命令禁用它 = Panel 立即
                // 断连且无法经 UI 恢复。拒绝经 TogglePlugin 禁用——确需禁用只能
                // 编辑 core.toml 的 disabled_plugins 后重启 Core。
                if !enabled && id == crate::plugins::MANAGEMENT_PANEL_PLUGIN_ID {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: format!(
                            "refuse to disable {id}: 管理面插件禁用即关闭 management 通道（Panel 自锁），如需禁用请编辑 core.toml 的 disabled_plugins 后重启 Core"
                        ),
                    });
                    return;
                }
                match self.plugin_host().set_enabled(&id, enabled).await {
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
            BackendCommand::RequestShellSessions { team_id } => {
                let sessions = crate::shell::shell_manager_global()
                    .map(|m| m.list(team_id.as_deref()))
                    .unwrap_or_default();
                self.emit(BackendEvent::ShellSessionsList { sessions, team_id });
            }
            BackendCommand::ShellStart { workdir } => match crate::shell::shell_manager_global() {
                Some(m) => {
                    let emit = crate::shell::shell_emit_for_self(self);
                    match m.start(workdir, self.team_id(), &emit).await {
                        Ok(info) => {
                            self.emit(BackendEvent::ShellSessionStarted {
                                session: info.clone(),
                            });
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("shell session started: {}", info.session_id),
                            });
                        }
                        Err(e) => {
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("shell start failed: {e}"),
                            });
                        }
                    }
                }
                None => {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: "shell manager unavailable".into(),
                    });
                }
            },
            BackendCommand::ShellExec {
                session_id,
                command,
                timeout_secs,
            } => match crate::shell::shell_manager_global() {
                Some(m) => {
                    let emit = crate::shell::shell_emit_for_self(self);
                    match m.exec(&session_id, &command, timeout_secs, &emit).await {
                        Ok((output, _success, _timed_out)) => {
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!(
                                    "shell exec done ({session_id}): {} chars",
                                    output.chars().count()
                                ),
                            });
                        }
                        Err(e) => {
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("shell exec failed: {e}"),
                            });
                        }
                    }
                }
                None => {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: "shell manager unavailable".into(),
                    });
                }
            },
            BackendCommand::ShellStop { session_id } => {
                match crate::shell::shell_manager_global() {
                    Some(m) => {
                        let emit = crate::shell::shell_emit_for_self(self);
                        match m.stop(&session_id, &emit).await {
                            Ok(()) => {
                                self.emit(BackendEvent::Error {
                                    session_id: None,
                                    message: format!("shell session {session_id} stopped"),
                                });
                            }
                            Err(e) => {
                                self.emit(BackendEvent::Error {
                                    session_id: None,
                                    message: format!("shell stop failed: {e}"),
                                });
                            }
                        }
                    }
                    None => {
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: "shell manager unavailable".into(),
                        });
                    }
                }
            }
            BackendCommand::InstallSkillFromGit { url, name, branch } => {
                let dir = self.current_skills_dir().await;
                match crate::skill_install::install(
                    std::path::Path::new(&dir),
                    &url,
                    name.as_deref(),
                    branch.as_deref(),
                ) {
                    Ok(_) => {
                        if let Err(e) = self.reload_skills(&dir).await {
                            tracing::warn!(error = %e, "skill reload after install failed");
                        }
                        self.emit_skills_list().await;
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("skill source installed: {url}"),
                        });
                    }
                    Err(e) => {
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("install skill source failed: {e}"),
                        });
                    }
                }
            }
            BackendCommand::UpdateSkillFromGit { name } => {
                let dir = self.current_skills_dir().await;
                match crate::skill_install::update(std::path::Path::new(&dir), &name) {
                    Ok(src) => {
                        if let Err(e) = self.reload_skills(&dir).await {
                            tracing::warn!(error = %e, "skill reload after update failed");
                        }
                        self.emit_skills_list().await;
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("skill {name} updated to {}", src.rev),
                        });
                    }
                    Err(e) => {
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("update skill source failed: {e}"),
                        });
                    }
                }
            }
            BackendCommand::RemoveSkillSource { name } => {
                let dir = self.current_skills_dir().await;
                match crate::skill_install::remove_source(std::path::Path::new(&dir), &name) {
                    Ok(()) => {
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("skill source record removed: {name}"),
                        });
                    }
                    Err(e) => {
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("remove skill source failed: {e}"),
                        });
                    }
                }
            }
            BackendCommand::RequestState => {
                self.emit_api_config().await;
                // 会话列表要覆盖**所有人格**：每个会话由事件自带 team_id
                //（SessionInfo.team_id / emit 的 annotate_team），Panel 按
                // 当前 agent 过滤展示。只遍历默认 agent 会让其他人格的会话
                // 缺失（或混入主 agent 会话，刷新后侧边栏串显）。
                let mut emitted_sessions = 0usize;
                async fn emit_sessions_of(agent: &Agent, count: &mut usize) {
                    for s in agent.trunk.all() {
                        let last = s
                            .history
                            .lock()
                            .await
                            .last()
                            .cloned()
                            .map(|m| m.content)
                            .unwrap_or_default();
                        agent.emit(BackendEvent::SessionUpdated {
                            session: s.info(last),
                        });
                        *count += 1;
                    }
                }
                // 当前（默认）agent 的会话。
                emit_sessions_of(self, &mut emitted_sessions).await;
                // 其余 persona 的会话（经 event_bus 镜像到默认 handle，各自
                // annotate 自己的 team_id）。
                if let Some(mgr) = crate::agent_manager::global_manager() {
                    for running in mgr.all() {
                        if running.agent.team_id() == self.team_id() {
                            continue; // 已由上方覆盖
                        }
                        emit_sessions_of(&running.agent, &mut emitted_sessions).await;
                    }
                }
                tracing::info!(
                    sessions = emitted_sessions,
                    "state sessions emitted for all personas"
                );
                // Also emit adapter status and QQ gate/filter state so the
                // TUI starts up with the correct persisted values.
                self.emit_adapter_list();
                // QQ 状态：逐个实例下发（多实例；单实例时前端行为不变）。
                for name in self.adapters.names() {
                    let Some(adapter) = self.adapters.get(&name) else {
                        continue;
                    };
                    if adapter.platform() != "qq" {
                        continue;
                    }
                    let instance = Some(name.clone());
                    let (au, ag, du, dg) = adapter.get_filter_info();
                    self.emit(BackendEvent::QqFilterConfig {
                        allowlist_users: au.iter().filter_map(|s| s.parse().ok()).collect(),
                        allowlist_groups: ag.iter().filter_map(|s| s.parse().ok()).collect(),
                        denylist_users: du.iter().filter_map(|s| s.parse().ok()).collect(),
                        denylist_groups: dg.iter().filter_map(|s| s.parse().ok()).collect(),
                        adapter: instance.clone(),
                    });
                    self.emit(BackendEvent::QqGateMode {
                        mode: adapter.get_gate_mode(),
                        adapter: instance,
                    });
                }
            }
            BackendCommand::RequestContext {
                team_id,
                session_id,
            } => {
                // 会话隔离：team 指定时返回该成员自己的上下文快照；
                // session_id 指定时按会话（多会话上下文，2026-09）。
                if let Some(id) = team_id {
                    let agent =
                        crate::agent_manager::global_manager().and_then(|m| m.resolve(Some(&id)));
                    if let Some(t) = agent {
                        t.emit_context_snapshot(session_id.as_deref()).await;
                        return;
                    }
                }
                self.emit_context_snapshot(session_id.as_deref()).await;
            }
            BackendCommand::RequestTrunkTimeline { team_id, since_seq } => {
                // 指定人格时返回该 persona 的独立 timeline（不同记忆）。
                // since_seq > 0 时走增量快照；增量窗口不完整（None）回退全量。
                // full 标志告知前端本次是替换还是追加/修补，前端不再凭 seq
                // 大小猜测（全量误当增量会整段重复，空增量误当全量会清空聊天）。
                // 快照降级语义（2026-10）：timeline 锁被周期保存短暂
                // 持有时 timeline_snapshot 短重试后可能回 None——此时
                // **跳过本次同步**（不发空全量清前端），下轮请求自然重试。
                let snapshot_or_skip = |trunk: &crate::session::TrunkStore,
                                        since: u64|
                 -> Option<(
                    Vec<crate::event::TimelineMessage>,
                    u64,
                    bool,
                )> {
                    if since > 0 {
                        match trunk.timeline_snapshot_since(since) {
                            Some((m, s)) => Some((m, s, false)),
                            None => trunk
                                .timeline_snapshot()
                                .map(|m| (m, trunk.timeline_seq(), true)),
                        }
                    } else {
                        trunk
                            .timeline_snapshot()
                            .map(|m| (m, trunk.timeline_seq(), true))
                    }
                };
                let (messages, seq, full) = if let Some(ref id) = team_id {
                    match crate::agent_manager::global_manager() {
                        Some(mgr) => match mgr.resolve(Some(id)) {
                            Some(agent) => {
                                let Some(snap) = snapshot_or_skip(&agent.trunk, since_seq) else {
                                    tracing::warn!(
                                        "timeline snapshot degraded; skip this sync round"
                                    );
                                    return;
                                };
                                snap
                            }
                            None => {
                                let Some(snap) = snapshot_or_skip(&self.trunk, since_seq) else {
                                    tracing::warn!(
                                        "timeline snapshot degraded; skip this sync round"
                                    );
                                    return;
                                };
                                snap
                            }
                        },
                        None => {
                            let Some(snap) = snapshot_or_skip(&self.trunk, since_seq) else {
                                tracing::warn!("timeline snapshot degraded; skip this sync round");
                                return;
                            };
                            snap
                        }
                    }
                } else {
                    let Some(snap) = snapshot_or_skip(&self.trunk, since_seq) else {
                        tracing::warn!("timeline snapshot degraded; skip this sync round");
                        return;
                    };
                    snap
                };
                // 快照瘦身（2026-09 刷新加速）：推理正文占板载快照约七成，
                // 只给最近若干条保留全文（旧条目截断 + 标注），持久化不改。
                let messages = crate::timeline::elide_reasoning_for_wire(messages);
                self.emit(BackendEvent::TrunkTimeline {
                    messages,
                    seq,
                    team_id: team_id.clone(),
                    full,
                });
                // 活跃 turn 快照（刷新恢复，2026-10）：运行状态本是瞬时事件
                // 流，前端刷新重连后仅靠 TrunkTimeline 看不到正在运行的会话
                // （活动浮条/取消按钮丢失）。timeline 之后立刻补发——增量与
                // 全量都发，前端按快照重建 running 集合。
                let session_ids = match team_id.as_deref() {
                    Some(id) => crate::agent_manager::global_manager()
                        .and_then(|m| m.resolve(Some(id)))
                        .map(|a| a.active_turn_session_ids())
                        .unwrap_or_default(),
                    None => self.active_turn_session_ids(),
                };
                self.emit(BackendEvent::ActiveTurnsSnapshot {
                    session_ids,
                    team_id: team_id.clone(),
                });
            }
            BackendCommand::ClearHistory { team_id } => {
                if let Some(id) = team_id {
                    let target =
                        crate::agent_manager::global_manager().and_then(|m| m.resolve(Some(&id)));
                    match target {
                        Some(agent) => {
                            agent.trunk.clear_history().await;
                            agent.emit(BackendEvent::TrunkTimeline {
                                messages: Vec::new(),
                                seq: agent.trunk.timeline_seq(),
                                team_id: Some(id),
                                full: true,
                            });
                            agent.emit_context_snapshot_for().await;
                            agent.emit(BackendEvent::Error {
                                session_id: None,
                                message: "历史记忆已清理".into(),
                            });
                        }
                        None => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("team {id} 不存在"),
                        }),
                    }
                } else {
                    self.trunk.clear_history().await;
                    self.emit(BackendEvent::TrunkTimeline {
                        messages: Vec::new(),
                        seq: self.trunk.timeline_seq(),
                        team_id: None,
                        full: true,
                    });
                    self.emit_context_snapshot(None).await;
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: "历史记忆已清理".into(),
                    });
                }
            }
            BackendCommand::CancelRequestedWork {
                session_id,
                all,
                team_id: _,
            } => {
                let cancelled = self.cancel_requested_work(&session_id, all).await;
                self.emit(BackendEvent::Error {
                    session_id: Some(session_id.clone()),
                    message: format!("已取消 {cancelled} 个进行中的任务"),
                });
            }
            BackendCommand::RequestTeamsList => {
                self.emit_teams_list().await;
            }
            BackendCommand::ArchiveHistory { team_id } => {
                if let Some(id) = team_id {
                    match crate::agent_manager::global_manager().and_then(|m| m.resolve(Some(&id)))
                    {
                        Some(agent) => match agent.trunk.archive_history().await {
                            Ok(path) => self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("历史已归档：{path}"),
                            }),
                            Err(e) => self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("归档失败：{e}"),
                            }),
                        },
                        None => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("team {id} 不存在"),
                        }),
                    }
                } else {
                    match self.trunk.archive_history().await {
                        Ok(path) => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("历史已归档：{path}"),
                        }),
                        Err(e) => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("归档失败：{e}"),
                        }),
                    }
                }
            }
            BackendCommand::CompactHistory {
                team_id,
                keep_recent,
            } => {
                let keep = keep_recent.unwrap_or(40).clamp(10, 500);
                let result = if let Some(id) = team_id {
                    match crate::agent_manager::global_manager().and_then(|m| m.resolve(Some(&id)))
                    {
                        Some(agent) => agent.compact_history(keep).await,
                        None => Err(format!("team {id} 不存在")),
                    }
                } else {
                    self.compact_history(keep).await
                };
                match result {
                    Ok(msg) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message: msg,
                    }),
                    Err(e) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message: format!("压缩失败：{e}"),
                    }),
                }
            }
            BackendCommand::SaveTeam {
                id,
                name,
                description,
                system_prompt,
                enabled,
                system_skills,
                disabled_tools,
                disabled_skills,
                enabled_plugins,
                enabled_tools,
                enabled_skills,
                memory_limit_tokens,
                context_window_tokens,
                api_profile,
            } => match crate::agent_manager::global_manager() {
                Some(mgr) => {
                    let profile = crate::config::AgentProfile {
                        system_skills,
                        name,
                        description,
                        system_prompt,
                        enabled,
                        disabled_tools,
                        disabled_skills,
                        enabled_plugins,
                        enabled_tools,
                        enabled_skills,
                        memory_limit_tokens,
                        context_window_tokens,
                        api_profile,
                        disabled_plugins: Vec::new(),
                    };
                    match mgr.save_profile(&id, profile.clone(), enabled) {
                        Ok(()) => {
                            // 勾选变化立即生效：对运行中的目标 agent 重新发现技能
                            // 文件（覆盖用户手动新增/修改的 SKILL.md），再应用新的
                            // 启用/禁用白名单，最后推送技能列表。
                            if let Some(target) = mgr.resolve(Some(&id)) {
                                let dir = target.config.read().await.skills_dir.clone();
                                if let Err(e) = target.reload_skills(&dir).await {
                                    tracing::warn!(error = %e, agent = %id, "skill reload after team save failed");
                                }
                                target.apply_capabilities(&profile).await;
                                target.emit_skills_list().await;
                                // Persona 级 API 引用变更：立即重建该 persona 的
                                // provider（以管理面本 agent 的全局配置为池基准）。
                                target.set_persona_api(profile.api_profile.clone()).await;
                                let global_cfg = self.config.read().await.clone();
                                target.apply_persona_api(&global_cfg).await;
                                self.emit_teams_list().await;
                            }
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("agent {id} saved"),
                            });
                        }
                        Err(e) => {
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("save agent failed: {e}"),
                            });
                        }
                    }
                }
                None => {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: "agent manager unavailable".into(),
                    });
                }
            },
            BackendCommand::DeleteTeam { id } => match crate::agent_manager::global_manager() {
                Some(mgr) => match mgr.delete_profile(&id) {
                    Ok(()) => {
                        self.emit_teams_list().await;
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("agent {id} deleted"),
                        });
                    }
                    Err(e) => {
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("delete agent failed: {e}"),
                        });
                    }
                },
                None => {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: "agent manager unavailable".into(),
                    });
                }
            },
            BackendCommand::ToggleTeam { id, enabled } => {
                // 组合根 AgentManager 处理；这里转发回进程级管理器。
                match crate::agent_manager::global_manager() {
                    None => {}
                    Some(mgr) => {
                        let factory = crate::agent_manager::agent_factory_for_toggle();
                        let f = move |i: String, p: crate::config::AgentProfile| factory.call(i, p);
                        match mgr.set_enabled(&id, enabled, f) {
                            Ok(()) => {
                                // 重新启用的人格：补应用启动期门控（名单 +
                                // 全局禁用），否则新实例以"全开"状态运行到
                                // 下次保存/重启。
                                if enabled {
                                    if let Some(running) = mgr.get(&id) {
                                        running.agent.apply_capabilities(&running.profile).await;
                                    }
                                }
                                self.emit_teams_list().await;
                                self.emit(BackendEvent::Error {
                                    session_id: None,
                                    message: format!(
                                        "agent {id} {}",
                                        if enabled { "enabled" } else { "disabled" }
                                    ),
                                });
                            }
                            Err(e) => {
                                self.emit(BackendEvent::Error {
                                    session_id: None,
                                    message: format!("toggle agent failed: {e}"),
                                });
                            }
                        }
                    }
                }
            }
            BackendCommand::SendMessage {
                session_id,
                content,
                images,
                team_id: _,
            } => {
                // Parse or fall back to local TUI session key.
                let key = SessionKey::parse(&session_id).unwrap_or_else(SessionKey::local_tui);
                // 工作区通道（激活 = 进入项目对话）：昵称解析为工作区名，重建
                // 不退化（正常路径由激活 ensure 注册，这里是兜底）。
                let nickname = if key.scope == "workspace" {
                    self.workspace_store()
                        .and_then(|store| store.get(&key.scope_id))
                        .map(|ws| ws.name)
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| key.scope_id.clone())
                } else {
                    "local user".into()
                };
                let session = self.trunk.get_or_create(&key, nickname, None);
                let sid = session.id.clone();
                let received_at_ms = chrono::Utc::now().timestamp_millis();
                let message_sequence = self.next_message_sequence();
                // 面板上传的图片是 data URI（base64 内嵌）：在这里落盘并改
                // 写为 `/media/<id>` 引用，后续链路（事件/日志/时间线）只带
                // 引用；发往 LLM 前由投影出口还原为 data URI（2026-09-24）。
                let images: Vec<String> = images
                    .iter()
                    .map(|image| echo_defs::media_store::spill_or_keep(image))
                    .collect();
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
                    team_id: None,
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
                            team_id: None,
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
            | BackendCommand::RequestQqOwner { .. }
            | BackendCommand::RequestQqFilterConfig { .. }
            | BackendCommand::RequestGroupList { .. }
            | BackendCommand::RequestFriendList { .. }
            | BackendCommand::RequestQqLoginStatus { .. }
            | BackendCommand::RequestQqQrcode { .. } => {
                self.apply_qq_command(cmd).await;
            }
            BackendCommand::RequestWorkspaceSessions { .. }
            | BackendCommand::SaveWorkspaceSession { .. }
            | BackendCommand::DeleteWorkspaceSession { .. }
            | BackendCommand::ActivateWorkspaceSession { .. }
            | BackendCommand::RequestWorkspaceGitStatus { .. }
            | BackendCommand::RequestWorkspaceFiles { .. } => {
                self.apply_workspace_command(cmd).await;
            }
            // 联邦管理（Phase 4）：处理函数注册在组合根（需要 Federation
            // 句柄与 ConfigStore），经进程级注册表分发；未接线（联邦关闭
            // 或旧 Core）时明确报错而非静默吞掉。
            BackendCommand::SaveFederationPeer { .. }
            | BackendCommand::DeleteFederationPeer { .. }
            | BackendCommand::RequestFederationStatus
            | BackendCommand::RequestFederationInvite
            | BackendCommand::MigrateSession { .. }
            | BackendCommand::RequestSelfUpdate
            | BackendCommand::RequestSelfUpdateStatus => {
                let handler = crate::agent::federation_command_handler();
                match handler {
                    Some(h) => h(self, cmd).await,
                    None => {
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: "联邦未启用（[federation] enabled = false）".into(),
                        });
                    }
                }
            }
        }
    }

    /// Emit the full skill list (`BackendEvent::SkillsList`).
    pub async fn emit_skills_list(&self) {
        let skills = self.skills.lock().await;
        // 注入外部 Git 来源信息（skills/.sources.json）
        let sources = crate::skill_install::load_sources(std::path::Path::new(
            &self.config.read().await.skills_dir,
        ));
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
                package: skill.metadata.package.clone(),
                content: skill.instructions.clone(),
                system: skill.metadata.system,
                source: sources.get(&skill.metadata.name).map(|src| {
                    crate::event::SkillSourceInfo {
                        url: src.url.clone(),
                        rev: src.rev.clone(),
                        branch: src.branch.clone(),
                        installed_at: src.installed_at.clone(),
                    }
                }),
            })
            .collect();
        list.sort_by_key(|s| s.name.clone());
        drop(skills);
        self.emit(BackendEvent::SkillsList { skills: list });
    }

    /// Emit the full tool list (`BackendEvent::ToolsList`), including
    /// disabled tools, each with their
    /// category and (for this agent's perspective) enable state.
    pub async fn emit_tools_list(&self) {
        let defs = self.tools.full_definitions().await;
        let mut list: Vec<crate::event::ToolInfo> = defs
            .into_iter()
            .map(|(t, category, enabled, pkg)| crate::event::ToolInfo {
                name: t.name,
                description: t.description,
                parameters: t.parameters.unwrap_or(serde_json::Value::Null),
                category,
                enabled,
                package: pkg,
            })
            .collect();
        // 工具清单直接来自注册表（模型可见工具的唯一真源；spawn_subagent
        // 由组合根装配时注册、按包标签随 subagent 插件门控）。
        list.sort_by_key(|t| t.name.clone());
        self.emit(BackendEvent::ToolsList { tools: list });
    }

    /// Emit the team list (`BackendEvent::TeamsList`).
    pub async fn emit_teams_list(&self) {
        let teams = crate::agent_manager::global_manager()
            .map(|m| m.infos())
            .unwrap_or_default();
        self.emit(BackendEvent::TeamsList { teams });
    }

    /// Emit the full plugin list (`BackendEvent::PluginsList`).
    pub async fn emit_plugins_list(&self) {
        let plugins: Vec<crate::event::PluginInfo> = self
            .plugin_host()
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
                package: d.package,
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
        let mut frontmatter = format!(
            "---\nname: {}\ndescription: {}\n{keywords_str}",
            draft.name, draft.description,
        );
        if draft.always {
            frontmatter.push_str("metadata:\n  always: true\n");
        }
        if !draft.category.is_empty() {
            frontmatter.push_str(&format!("category: {}\n", draft.category));
        }
        if draft.system {
            frontmatter.push_str("system: true\n");
        }
        frontmatter.push_str("---\n");
        let body = format!("{frontmatter}{}", draft.content);
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
    pub system: bool,
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

/// 把磁盘技能目录重载进一组代理：返回 `(updated, checked, failures)`。
///
/// 从 [`BackendCommand::ReloadSkills`] 的处理臂抽出，便于测试直接注入代理集合
/// （生产路径的集合 = 所有运行中人格 + 管理代理自身）。
pub(crate) async fn reload_skills_into<'a>(
    dir: &str,
    targets: impl IntoIterator<Item = (&'a str, &'a Agent)>,
) -> (usize, usize, Vec<String>) {
    let mut updated = 0usize;
    let mut checked = 0usize;
    let mut failures = Vec::new();
    for (id, agent) in targets {
        checked += 1;
        match agent.reload_skills(dir).await {
            Ok(true) => updated += 1,
            Ok(false) => {}
            Err(error) => failures.push(format!("{id}: {error}")),
        }
    }
    (updated, checked, failures)
}

#[cfg(test)]
mod reload_skills_tests {
    use super::reload_skills_into;
    use crate::agent::Agent;
    use crate::config::AgentConfig;
    use crate::skill::SkillRegistry;
    use std::sync::Arc;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("echo-reload-skills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(dir: &std::path::Path, name: &str, description: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\nbody"),
        )
        .unwrap();
    }

    fn agent_with_skills(dir: &std::path::Path) -> Agent {
        Agent::new(
            Arc::new(crate::agent::tests::MockProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                reply: String::new(),
            }),
            AgentConfig {
                skills_dir: dir.to_string_lossy().into_owned(),
                ..Default::default()
            },
            SkillRegistry::discover(&dir.to_string_lossy()).unwrap(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_adapter::AdapterRegistry::new()),
        )
    }

    /// 广播语义（面板「重载技能」的正确性依据）：磁盘上的新技能必须到达
    /// **每个**目标代理的注册表，而不只是收到命令的那一个。
    #[tokio::test]
    async fn reload_skills_into_updates_every_target() {
        let dir = temp_dir("broadcast");
        write_skill(&dir, "alpha", "第一个技能");
        let a = agent_with_skills(&dir);
        let b = agent_with_skills(&dir);
        let dir_str = dir.to_string_lossy().into_owned();

        // 初始：两个代理都没有 beta。
        let (updated, checked, failures) =
            reload_skills_into(&dir_str, [("a", &a), ("b", &b)]).await;
        assert_eq!((updated, checked, failures.len()), (0, 2, 0), "无变化");

        // 磁盘新增技能 → 再次重载：两个都要拿到。
        write_skill(&dir, "beta", "第二个技能");
        let (updated, checked, failures) =
            reload_skills_into(&dir_str, [("a", &a), ("b", &b)]).await;
        assert_eq!(
            (updated, checked, failures.len()),
            (2, 2, 0),
            "两个代理都更新"
        );
        for agent in [&a, &b] {
            let mut names = agent.skills.lock().await.names();
            names.sort();
            assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
        }

        // 修改既有技能内容 → 重载换新（名字不变也算更新）。
        write_skill(&dir, "alpha", "描述已更新");
        let (updated, ..) = reload_skills_into(&dir_str, [("a", &a)]).await;
        assert_eq!(updated, 1);

        // 缺目录：不 panic，报错进入 failures。
        let (updated, checked, failures) =
            reload_skills_into("/nonexistent-skills-dir", [("a", &a)]).await;
        assert_eq!(updated, 0);
        assert_eq!(checked, 1);
        // 目录不存在时 reload_skills 返回 Ok(false)（静默），仅真实解析
        // 错误才进 failures——这里断言不 panic 且计数正确即可。
        assert!(failures.len() <= 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
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
