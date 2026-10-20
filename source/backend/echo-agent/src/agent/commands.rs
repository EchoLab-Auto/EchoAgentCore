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
        let blocks = self.context_blocks(resolved.as_deref(), &history).await;
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
            | BackendCommand::RequestTimelineEarlier { team_id, .. }
            | BackendCommand::RequestSessionHistory { team_id, .. }
            | BackendCommand::RequestContext { team_id, .. }
            | BackendCommand::ClearHistory { team_id, .. }
            | BackendCommand::ArchiveHistory { team_id, .. }
            | BackendCommand::CompactHistory { team_id, .. }
            | BackendCommand::RequestWorkspaceSessions { team_id }
            | BackendCommand::SaveWorkspaceSession { team_id, .. }
            | BackendCommand::DeleteWorkspaceSession { team_id, .. }
            | BackendCommand::ActivateWorkspaceSession { team_id, .. }
            | BackendCommand::RequestWorkspaceGitStatus { team_id, .. }
            | BackendCommand::RequestWorkspaceFiles { team_id, .. }
            | BackendCommand::SetSessionApi { team_id, .. }
            | BackendCommand::ClearSessionApi { team_id, .. } => team_id.as_deref(),
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
                | BackendCommand::RequestTimelineEarlier { .. }
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
                | BackendCommand::SetSessionApi { .. }
                | BackendCommand::ClearSessionApi { .. }
        )
    }

    /// 按范围清理人格记忆：`Some(session)` = 仅该会话；`None` = 全部会话。
    async fn clear_agent_history(agent: &Agent, session_id: Option<&str>) {
        match session_id.filter(|s| !s.is_empty()) {
            Some(session) => agent.trunk.clear_session(session).await,
            None => agent.trunk.clear_history().await,
        }
    }

    /// 清理完成的用户提示文案（区分会话级与全量）。
    fn clear_history_message(session_id: Option<&str>) -> String {
        match session_id.filter(|s| !s.is_empty()) {
            Some(session) => format!("会话历史已清理（{session}）"),
            None => "全部会话的历史记忆已清理".into(),
        }
    }

    /// 会话级 API 覆盖的持久化（2026-10 用户报障修复）：把
    /// `local:workspace:` 通道的覆盖写入对应工作区对话（`profile` = `None`
    /// 清除）。非工作区会话直接跳过。落盘失败 emit `BackendEvent::Error`
    /// 带消息（不再静默——存量工作区此前 find 失败导致选择过一会「失效」）。
    fn persist_session_api_profile(&self, session_id: &str, profile: Option<&str>) {
        let Some((ws_id, conv_id)) = crate::workspace::parse_workspace_channel(session_id) else {
            return;
        };
        let Some(store) = self.workspace_store() else {
            return;
        };
        let result = match conv_id.as_deref() {
            Some(conv_id) => store.set_conversation_api_profile(&ws_id, conv_id, profile),
            // legacy 裸通道：按 `legacy == true` 定位（不猜 id=="main"）。
            None => store.set_legacy_conversation_api_profile(&ws_id, profile),
        };
        if let Err(message) = result {
            self.emit(BackendEvent::Error {
                session_id: Some(session_id.to_string()),
                message: format!("会话 {session_id} API 覆盖持久化失败：{message}"),
            });
        }
    }

    /// 清理历史后向面板重发**实际剩余**的显示时间线（全量重建语义）。
    ///
    /// 2026-10 用户报告修复：此前无条件发 `messages: Vec::new()`——面板把
    /// "空全量"当权威数据，会清掉**整个本地聊天缓存**（含未被清理的其他
    /// 会话），且本地游标被推进到最新、刷新也拉不回来（增量=空）。改为发
    /// 清理后的真实剩余条目（快照为同步短锁 clone，不存在降级为空的路径）。
    fn emit_timeline_after_clear(agent: &Agent, team_id: Option<String>) {
        let messages = agent.trunk.timeline_snapshot();
        let messages = crate::timeline::elide_reasoning_for_wire(messages);
        agent.emit(BackendEvent::TrunkTimeline {
            messages,
            seq: agent.trunk.timeline_seq(),
            team_id,
            full: true,
            has_more: false, // 全量重建：已含全部条目
        });
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
                        match self.registry().and_then(|r| r.resolve(target_id)) {
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
        // ── 命令域归插件（plugin-framework v0.8 的 `Contribution::Command`）──
        //
        // 在落内置 match **之前**问一次插件：有插件声明处理这条命令就交给它，
        // 内置分支不再执行。于是"某个命令域归某个插件"是**注册关系**——
        // 插件未装/被禁用时，命令自然回落到内置分支（或明确报错），不需要在
        // 这里为每个插件写特判。
        //
        // **内置保留名单**：插件不能抢这些——它们是宿主的自我管理面，被抢走
        // 会让面板再也改不回来（禁用插件后无法重新启用）。命中保留名单时**不**
        // 分发，并记一条 warn（配置写错要能看见，而不是静默失效）。
        if !crate::plugins::command_is_host_reserved(cmd.kind_name()) {
            // 每个受本 agent 处理的命令都会经过这里——所以**先 clone 一份**给
            // 插件分发，命中插件就返回，未命中再落下面的内置 match（此时 cmd
            // 仍然完好）。clone 的代价是每条命令一次，而命中插件时省下的是一整
            // 段宿主分支；命中不了时多一次 clone 也远小于一次面板往返。
            let probe = cmd.clone();
            if let Some(outcome) = self.plugin_host().dispatch_command(&probe).await {
                match outcome {
                    Ok(_) => {}
                    Err(message) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    }),
                }
                return;
            }
        }
        match cmd {
            BackendCommand::SwitchModel { model } => {
                self.model.config.write().await.model = model.clone();
                self.set_model(model.clone()).await;
                self.emit_api_config().await;
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("switched model: {model}"),
                });
                let cfg = self.model.config.read().await.clone();
                self.persist_config(&cfg).await;
            }
            BackendCommand::SwitchProvider { provider } => {
                let mut config = self.model.config.write().await;
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
                    let mut config = self.model.config.write().await;
                    config.provider = old_provider;
                }
            }
            BackendCommand::SetSystemPrompt { prompt } => {
                self.model.config.write().await.system_prompt = prompt.clone();
                self.prompt.invalidate().await;
                self.persist_system_prompt_plugin(&prompt).await;
                self.emit(BackendEvent::SystemPrompt { text: prompt });
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "system prompt updated".into(),
                });
            }
            BackendCommand::RequestSystemPrompt => {
                let text = self.model.config.read().await.system_prompt.clone();
                self.emit(BackendEvent::SystemPrompt { text });
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
            BackendCommand::SetSessionApi {
                team_id: _,
                session_id,
                profile,
            } => {
                // 会话级 API 覆盖（2026-10）：该会话的 LLM 请求走指定
                // profile 的 provider；其余会话不受影响。team_id 已由
                // requires_explicit_team 保证非空。
                //
                // 持久化（2026-10 拓展）：同时写入工作区对话的
                // `api_profile`（`WorkspaceConversationInfo.api_profile`），
                // 重启后激活该对话时恢复。
                //
                // 顺序：先解析（provider 构建成功）再落盘——失败时不写入，
                // 避免在工作区 JSON 里留下无法解析的覆盖（重启后会反复
                // 报 profile not found）。2026-10 用户报障修正。
                let ok = self.set_session_provider(&session_id, &profile).await;
                if ok {
                    // 持久化到工作区对话（顺序：provider 构建成功才落盘；
                    // 落盘失败 emit Error，不再静默——2026-10 用户报障修复）。
                    self.persist_session_api_profile(&session_id, Some(profile.as_str()));
                }
                let message = if ok {
                    if profile.is_empty() {
                        format!("会话 {session_id} 已恢复跟随 persona 级配置")
                    } else {
                        format!("会话 {session_id} 已切换到 API: {profile}")
                    }
                } else {
                    format!("会话 {session_id} 切换 API 失败（profile 不存在或构建失败）")
                };
                self.emit(BackendEvent::Error {
                    session_id: Some(session_id.clone()),
                    message,
                });
                self.emit_api_config().await;
            }
            BackendCommand::ClearSessionApi {
                team_id: _,
                session_id,
            } => {
                // 清除会话级覆盖（回到跟随 persona 级）。
                // 同步清除工作区对话的持久化 api_profile。
                self.persist_session_api_profile(&session_id, None);
                self.set_session_provider(&session_id, "").await;
                self.emit(BackendEvent::Error {
                    session_id: Some(session_id.clone()),
                    message: format!("会话 {session_id} 已恢复跟随 persona 级配置"),
                });
                self.emit_api_config().await;
            }
            BackendCommand::RequestRemoteApiProfiles { peer } => {
                // 分布式供应商共享（2026-10）：经联邦拉远端**脱敏**供应商
                // 池回传前端；peer 离线/未实现该查询时 error 带原因。
                let result = match self.federation().querier.clone() {
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
            BackendCommand::QueryApiMetrics { name } => {
                self.query_api_metrics(&name).await;
            }
            BackendCommand::RestartPlugin { id } => {
                // 防自锁：重启管理面插件 = 先卸下 management WS，Panel 立即
                // 断连且无法经界面恢复（与禁用同一条约束）。
                if id == crate::plugins::MANAGEMENT_PANEL_PLUGIN_ID {
                    self.emit(BackendEvent::PluginOpResult {
                        id: id.clone(),
                        op: "restart".into(),
                        ok: false,
                        message: "管理面插件承载 management 通道，重启会断开 Panel；如需重载请重启 Core 服务".into(),
                    });
                    return;
                }
                // 外部条目：真的 stop + start 子进程（可逆停止语义，见
                // `PluginHandle::stop`）——这是「重启」对子进程插件唯一
                // 说得通的含义，也是它区别于「关再开」的地方（不写配置）。
                let result = if self.plugin_host().is_external_entry(&id) {
                    match self.plugin_host().stop_subprocess(&id).await {
                        Ok(()) => self.plugin_host().start_subprocess(&id).await,
                        Err(error) => Err(error),
                    }
                } else {
                    self.plugin_host().restart(&id)
                };
                match result {
                    Ok(()) => {
                        self.emit(BackendEvent::PluginOpResult {
                            id: id.clone(),
                            op: "restart".into(),
                            ok: true,
                            message: format!("{id} 已重启"),
                        });
                    }
                    Err(error) => {
                        self.emit(BackendEvent::PluginOpResult {
                            id: id.clone(),
                            op: "restart".into(),
                            ok: false,
                            message: format!("restart {id} failed: {error}"),
                        });
                    }
                }
                // 两种结果都刷新列表：失败时状态同样可能已变（停下来了但没起来）。
                self.emit_plugins_list().await;
            }
            // 工具管理面命令域已迁进 `echo-plugin-tools`（全插件化 S3：
            // 清单与启停本来就是这个插件拥有的东西）。这里只剩兜底。
            BackendCommand::RequestToolsList | BackendCommand::ToggleTool { .. } => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "工具命令由 echo-agent.tools.builtin 插件处理，但该插件未启用"
                        .into(),
                });
            }
            BackendCommand::RequestPluginsList => {
                self.emit_plugins_list().await;
            }
            BackendCommand::TogglePlugin { id, enabled } => {
                // 防自锁：管理面插件承载 management WS，经命令禁用它 = Panel 立即
                // 断连且无法经 UI 恢复。拒绝经 TogglePlugin 禁用——确需禁用只能
                // 编辑 core.toml 的 disabled_plugins 后重启 Core。
                if !enabled && id == crate::plugins::MANAGEMENT_PANEL_PLUGIN_ID {
                    self.emit(BackendEvent::PluginOpResult {
                        id: id.clone(),
                        op: "disable".into(),
                        ok: false,
                        message: "管理面插件禁用即关闭 management 通道（Panel 自锁），如需禁用请编辑 core.toml 的 disabled_plugins 后重启 Core".into(),
                    });
                    return;
                }
                // 外部条目（`plugins.toml` 的 stdio 插件）有独立子进程：
                // 启用/禁用要**真的停掉/拉起进程**，只改注册表状态是不够的
                // （用户点了「禁用」却发现插件还在跑，比没有开关更糟）。
                if self.plugin_host().is_external_entry(&id) {
                    let result = if enabled {
                        self.plugin_host().start_subprocess(&id).await
                    } else {
                        self.plugin_host().stop_subprocess(&id).await
                    };
                    let op = if enabled { "enable" } else { "disable" };
                    match result {
                        Ok(()) => {
                            // 同步注册表挂载态（descriptors 的 enabled 与进程
                            // 状态必须一致，否则面板显示「启用」但进程已停）。
                            let _ = self.plugin_host().set_enabled(&id, enabled).await;
                            let mut cfg = self.model.config.write().await;
                            cfg.disabled_plugins.retain(|p| p != &id);
                            if !enabled {
                                cfg.disabled_plugins.push(id.clone());
                            }
                            let cfg_snapshot = cfg.clone();
                            drop(cfg);
                            self.persist_config(&cfg_snapshot).await;
                            self.emit(BackendEvent::PluginOpResult {
                                id: id.clone(),
                                op: op.into(),
                                ok: true,
                                message: if enabled {
                                    format!("{id} 已启动")
                                } else {
                                    format!("{id} 已停止（子进程已退出）")
                                },
                            });
                            self.emit_plugins_list().await;
                        }
                        Err(error) => {
                            self.emit(BackendEvent::PluginOpResult {
                                id: id.clone(),
                                op: op.into(),
                                ok: false,
                                message: error,
                            });
                            self.emit_plugins_list().await;
                        }
                    }
                    return;
                }
                match self.plugin_host().set_enabled(&id, enabled).await {
                    Ok(()) => {
                        // Persist so the choice survives restarts.
                        let mut cfg = self.model.config.write().await;
                        cfg.disabled_plugins.retain(|p| p != &id);
                        if !enabled {
                            cfg.disabled_plugins.push(id.clone());
                        }
                        let cfg_snapshot = cfg.clone();
                        drop(cfg);
                        self.persist_config(&cfg_snapshot).await;
                        self.emit(BackendEvent::PluginOpResult {
                            id: id.clone(),
                            op: if enabled {
                                "enable".into()
                            } else {
                                "disable".into()
                            },
                            ok: true,
                            message: format!("{id} 已{}", if enabled { "启用" } else { "禁用" }),
                        });
                        self.emit_plugins_list().await;
                    }
                    Err(e) => {
                        // 结构化回执（面板按 ok 分流渲染）+ 刷新列表
                        //（失败时状态可能已被部分改变，以列表为准）。
                        self.emit(BackendEvent::PluginOpResult {
                            id: id.clone(),
                            op: if enabled {
                                "enable".into()
                            } else {
                                "disable".into()
                            },
                            ok: false,
                            message: format!("toggle plugin failed: {e}"),
                        });
                        self.emit_plugins_list().await;
                    }
                }
            }
            BackendCommand::DeleteApi { name } => {
                self.delete_api(&name).await;
            }
            BackendCommand::RenameApi { .. } => {
                // RenameApi 是**进程级**命令（改的是全局供应商池，不是某个人格），
                // 由 [`crate::process_commands`] 在命令泵里处理。这里 fail-closed。
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "API 改名应由进程级处理器处理（命令泵接线异常）".into(),
                });
            }
            // Shell 命令域已迁进 `echo-plugin-tools`（全插件化 S3：与 `shell_*`
            // 工具同属一个插件，会话的模型侧与面板侧不再横跨插件边界）。
            // 这里只剩"插件未启用"的兜底——插件禁用后命令明确报错，不静默。
            BackendCommand::RequestShellSessions { .. }
            | BackendCommand::ShellStart { .. }
            | BackendCommand::ShellExec { .. }
            | BackendCommand::ShellStop { .. } => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "shell 命令由 echo-agent.tools.builtin 插件处理，但该插件未启用".into(),
                });
            }
            // 技能命令域已迁进 `echo-plugin-skills`（全插件化 S3）：技能表、
            // frontmatter、目录校验、`.sources.json`、git 安装与列表投影都在
            // 那里。这里只剩"插件未启用"的兜底。
            BackendCommand::RequestSkillsList
            | BackendCommand::ReloadSkills
            | BackendCommand::ToggleSkill { .. }
            | BackendCommand::SaveSkill { .. }
            | BackendCommand::DeleteSkill { .. }
            | BackendCommand::InstallSkillFromGit { .. }
            | BackendCommand::UpdateSkillFromGit { .. }
            | BackendCommand::RemoveSkillSource { .. } => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "技能命令由 echo-agent.skills.dir 插件处理，但该插件未启用".into(),
                });
            }
            BackendCommand::RequestState => {
                self.emit_api_config().await;
                // 会话列表要覆盖**所有人格**：每个会话由事件自带 team_id
                //（SessionInfo.team_id / emit 的 annotate_team），Panel 按
                // 当前 agent 过滤展示。只遍历默认 agent 会让其他人格的会话
                // 缺失（或混入主 agent 会话，刷新后侧边栏串显）。
                let mut emitted_sessions = 0usize;
                // 身份由调用方传入（`Agent` 实例字段）：内层 fn 拿不到 self，
                // 因此显式作为参数——与 P3「身份是参数」保持一致。
                async fn emit_sessions_of(
                    agent: &Agent,
                    identity: &crate::NodeIdentity,
                    count: &mut usize,
                ) {
                    for s in agent.trunk.all() {
                        let last = s
                            .history
                            .lock()
                            .last()
                            .cloned()
                            .map(|m| m.content)
                            .unwrap_or_default();
                        agent.emit(BackendEvent::SessionUpdated {
                            session: s.info(last, identity),
                        });
                        *count += 1;
                    }
                }
                // 当前（默认）agent 的会话。
                let identity = self.node_identity();
                emit_sessions_of(self, &identity, &mut emitted_sessions).await;
                // 其余 persona 的会话（经 event_bus 镜像到默认 handle，各自
                // annotate 自己的 team_id）。
                if let Some(mgr) = self.registry() {
                    for running in mgr.all() {
                        if running.agent.team_id() == self.team_id() {
                            continue; // 已由上方覆盖
                        }
                        let running_identity = running.agent.node_identity();
                        emit_sessions_of(&running.agent, &running_identity, &mut emitted_sessions)
                            .await;
                    }
                }
                // DEBUG：这是「面板重连后的一次全量快照」，频次跟随连接
                // 抖动（实测 151 条/天），不是状态变化——默认级别不该看见。
                tracing::debug!(
                    sessions = emitted_sessions,
                    "state sessions emitted for all personas"
                );
                // Also emit instance status and instance management state so the
                // TUI starts up with the correct persisted values.
                self.emit_instance_list();
                // 插件实例管理状态：逐个实例下发（W3.4 单源化——InstanceState；
                // 缺管理面的实例在内部静默跳过）。
                for name in self.instances.names() {
                    let Some(instance) = self.instances.get(&name) else {
                        continue;
                    };
                    // 状态下发单源（投影在插件里）：`InstanceState` 的构造
                    // 归 `echo-plugin-tools`，宿主只决定"何时发"。
                    if let Some(management) = instance.management() {
                        if let Some(state) = management.cached_state() {
                            self.emit(BackendEvent::InstanceState {
                                instance: instance.name().to_string(),
                                state,
                            });
                        }
                    }
                }
            }
            BackendCommand::RequestContext {
                team_id,
                session_id,
            } => {
                // 会话隔离：team 指定时返回该成员自己的上下文快照；
                // session_id 指定时按会话（多会话上下文，2026-09）。
                if let Some(id) = team_id {
                    let agent = self.registry().and_then(|r| r.resolve(&id));
                    if let Some(t) = agent {
                        t.emit_context_snapshot(session_id.as_deref()).await;
                        return;
                    }
                }
                self.emit_context_snapshot(session_id.as_deref()).await;
            }
            BackendCommand::RequestTrunkTimeline {
                team_id,
                since_seq,
                limit,
            } => {
                // 指定人格时返回该 persona 的独立 timeline（不同记忆）。
                // since_seq > 0 时走增量快照；增量窗口不完整（None）回退全量。
                // full 标志告知前端本次是替换还是追加/修补，前端不再凭 seq
                // 大小猜测（全量误当增量会整段重复，空增量误当全量会清空聊天）。
                // 快照为同步短锁 clone（2026-10 评审：去掉 sleep 自旋与 None
                // 降级）；Option 外形保留给将来可能的降级路径。
                //
                // 返回值带上 `has_more`（2026-10 分页）：全量路径按 `limit`
                // 只回**最新** N 条时置 true，前端据此按需向上翻页。增量路径
                // **不截断**——那是缺口补齐，截断会让前端游标与实际内容不一致。
                let snapshot_or_skip = |trunk: &crate::session::TrunkStore,
                                        since: u64|
                 -> Option<(
                    Vec<crate::event::TimelineMessage>,
                    u64,
                    bool,
                    bool,
                )> {
                    if since > 0 {
                        match trunk.timeline_snapshot_since(since) {
                            Some((m, s)) => Some((m, s, false, false)),
                            None => {
                                let (m, more) = window_tail(trunk, limit);
                                Some((m, trunk.timeline_seq(), true, more))
                            }
                        }
                    } else {
                        let (m, more) = window_tail(trunk, limit);
                        Some((m, trunk.timeline_seq(), true, more))
                    }
                };
                /// 全量路径取窗口尾部：`limit` 为 None/0 时返回全部。
                fn window_tail(
                    trunk: &crate::session::TrunkStore,
                    limit: Option<usize>,
                ) -> (Vec<crate::event::TimelineMessage>, bool) {
                    match limit {
                        Some(n) if n > 0 => trunk.timeline_window_tail(n),
                        _ => (trunk.timeline_snapshot(), false),
                    }
                }
                let (messages, seq, full, has_more) = if let Some(ref id) = team_id {
                    match self.registry() {
                        Some(mgr) => match mgr.resolve(id) {
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
                    has_more,
                });
                // 活跃 turn 快照（刷新恢复，2026-10）：运行状态本是瞬时事件
                // 流，前端刷新重连后仅靠 TrunkTimeline 看不到正在运行的会话
                // （活动浮条/取消按钮丢失）。timeline 之后立刻补发——增量与
                // 全量都发，前端按快照重建 running 集合。
                let session_ids = match team_id.as_deref() {
                    Some(id) => self
                        .registry()
                        .and_then(|m| m.resolve(id))
                        .map(|a| a.active_turn_session_ids())
                        .unwrap_or_default(),
                    None => self.active_turn_session_ids(),
                };
                self.emit(BackendEvent::ActiveTurnsSnapshot {
                    session_ids,
                    team_id: team_id.clone(),
                });
            }
            BackendCommand::RequestTimelineEarlier {
                team_id,
                before_ord,
                limit,
            } => {
                // 向上翻页（2026-10 分页）：返回 `seq < before_seq` 的最近 N 条。
                // 与 `RequestTrunkTimeline` 分开的理由见协议注释：翻页是"前插
                // 到已加载列表头部"，不动同步游标；混在 full/增量语义里会让
                // 前端难以判断该替换还是该前插。
                let limit = limit.unwrap_or(200).clamp(1, 500);
                // 目标 persona 解析（与 RequestTrunkTimeline 相同口径）。
                let resolved = team_id
                    .as_deref()
                    .and_then(|id| self.registry()?.resolve(id));
                let trunk: &crate::session::TrunkStore = match &resolved {
                    Some(agent) => &agent.trunk,
                    None => &self.trunk,
                };
                let (messages, has_more) = trunk.timeline_page(before_ord, limit);
                // 与全量快照同一套线上瘦身（推理正文截断），保证前端拿到的
                // 形状与首屏一致。
                let messages = crate::timeline::elide_reasoning_for_wire(messages);
                self.emit(BackendEvent::TimelineEarlier {
                    team_id: team_id.clone(),
                    messages,
                    has_more,
                });
            }
            BackendCommand::RequestSessionHistory {
                team_id,
                session_id,
                limit,
            } => {
                // 按会话派生显示历史（2026-10 面板「会话无消息」修复）：
                // 共享显示时间线有 1024 条上限、跨会话 FIFO 淘汰，旧会话
                // 条目可能被整体挤出；事件日志按会话分区且完整，因此从这里
                // 按需派生（镜像实时投影的展示形状）。
                let limit = limit.unwrap_or(500).clamp(1, 2000);
                // 目标 persona 解析（与 RequestTrunkTimeline 相同）：
                // team_id 可解析 → 目标 agent 的 trunk；其余回退本 agent。
                let resolved = team_id
                    .as_deref()
                    .and_then(|id| self.registry()?.resolve(id));
                let trunk: &crate::session::TrunkStore = match &resolved {
                    Some(agent) => &agent.trunk,
                    None => &self.trunk,
                };
                let (messages, total, truncated) =
                    trunk.display_history_for_session(&session_id, limit);
                // 合并水位线：派生时该会话在显示时间线中的最大 seq。
                // **必须与派生范围一致**（`history_watermark` 排除推理 ——
                // 推理不落事件日志、派生不含；若把它计进水位线，前端会把
                // 实时推理判成"快照已覆盖"而整条丢弃 → 看不到思考 chip）。
                let session_key = session_id.as_str();
                let max_seq =
                    crate::session::history_watermark(trunk.timeline_snapshot(), session_key);
                self.emit(BackendEvent::SessionHistory {
                    team_id: team_id.clone(),
                    session_id,
                    messages,
                    total,
                    truncated,
                    max_seq,
                });
            }
            BackendCommand::ClearHistory {
                team_id,
                session_id,
            } => {
                // 会话级清理（2026-10 用户报告修复）：`session_id` 存在时只清
                // 该会话——此前只有 team 维度，面板「清理历史记忆」会把同人格
                // 下所有会话（QQ 群/私聊等）的记忆一并抹掉。
                if let Some(id) = team_id {
                    let target = self.registry().and_then(|r| r.resolve(&id));
                    match target {
                        Some(agent) => {
                            Self::clear_agent_history(&agent, session_id.as_deref()).await;
                            // 同步清除该会话的 provider 覆盖（2026-10 会话级
                            // API 清理一致性：历史清空时覆盖一并移除）。
                            if let Some(sid) = session_id.as_deref().filter(|s| !s.is_empty()) {
                                agent.model.session_providers.remove(sid);
                            }
                            tracing::info!(
                                team = %id,
                                session = session_id.as_deref().unwrap_or("<all>"),
                                "clear history command applied"
                            );
                            // 清理后重发**实际剩余**的显示时间线（全量）——绝不
                            // 发空快照：面板把"空全量"当权威数据，会清掉整个本地
                            // 聊天缓存（含未被清理的其他会话），且游标推到最新后
                            // 增量=空、刷新也拉不回（2026-10 用户报告修复）。
                            Self::emit_timeline_after_clear(&agent, Some(id));
                            agent.emit_context_snapshot_for().await;
                            agent.emit(BackendEvent::Error {
                                session_id: None,
                                message: Self::clear_history_message(session_id.as_deref()),
                            });
                        }
                        // 无管理器（嵌入式/测试）或解析落空，但消息已送达目标
                        // 人格本体（team 匹配）时，按转发语义直接处理自身——
                        // 与 RequestTrunkTimeline 的 self 兜底保持一致。
                        None if self.team_id().as_deref() == Some(id.as_str()) => {
                            Self::clear_agent_history(self, session_id.as_deref()).await;
                            if let Some(sid) = session_id.as_deref().filter(|s| !s.is_empty()) {
                                self.model.session_providers.remove(sid);
                            }
                            tracing::info!(
                                team = %id,
                                session = session_id.as_deref().unwrap_or("<all>"),
                                "clear history command applied (self)"
                            );
                            Self::emit_timeline_after_clear(self, Some(id));
                            self.emit_context_snapshot_for().await;
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message: Self::clear_history_message(session_id.as_deref()),
                            });
                        }
                        None => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("team {id} 不存在"),
                        }),
                    }
                } else {
                    Self::clear_agent_history(self, session_id.as_deref()).await;
                    tracing::info!(
                        session = session_id.as_deref().unwrap_or("<all>"),
                        "clear history command applied (no team scope)"
                    );
                    Self::emit_timeline_after_clear(self, None);
                    self.emit_context_snapshot(None).await;
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message: Self::clear_history_message(session_id.as_deref()),
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
            BackendCommand::ArchiveHistory {
                team_id,
                session_id,
            } => {
                // 迁移期 fail-closed（2026-10 审计）：无 session_id 的归档在
                // 旧语义下是"整档归档 + **全量清空**"——误清代价不可恢复，
                // 且没有任何 UI 入口需要它（唯一客户端面板已升级为必带
                // session_id）。缺失即视为过旧客户端：拒绝，而非降级执行
                // 全量清空。
                let Some(session) = session_id.filter(|s| !s.is_empty()) else {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message:
                            "归档需要指定会话（session_id 缺失——客户端过旧，请升级面板后重试）"
                                .into(),
                    });
                    return;
                };
                // 会话级归档（2026-10 审计）：只归档并清空该会话，其他会话
                // 不受影响——与面板「归档当前会话」的承诺对齐。此前该命令
                // 无会话维度，点一次清掉同人格全部会话（「清理历史记忆」
                // 同类漏网）。
                if let Some(id) = team_id {
                    match self.registry().and_then(|r| r.resolve(&id)) {
                        Some(agent) => {
                            match agent.trunk.archive_session(&session).await {
                                Ok(path) => {
                                    tracing::info!(
                                        team = %id,
                                        session = %session,
                                        "archive history command applied"
                                    );
                                    // 归档后重发**实际剩余**的显示时间线（全量）——
                                    // 与会话级清理同一闭环（2026-10 审计：此前不重发，
                                    // 面板残留被归档会话的旧聊天）。
                                    Self::emit_timeline_after_clear(&agent, Some(id));
                                    agent.emit_context_snapshot_for().await;
                                    agent.emit(BackendEvent::Error {
                                        session_id: None,
                                        message: format!("历史已归档：{path}"),
                                    });
                                }
                                Err(e) => agent.emit(BackendEvent::Error {
                                    session_id: None,
                                    message: format!("归档失败：{e}"),
                                }),
                            }
                        }
                        // 无管理器（嵌入式/测试）但消息已送达目标人格本体
                        // （team 匹配）时，按转发语义直接处理自身——与
                        // ClearHistory 的兜底保持一致。
                        None if self.team_id().as_deref() == Some(id.as_str()) => {
                            match self.trunk.archive_session(&session).await {
                                Ok(path) => {
                                    tracing::info!(
                                        team = %id,
                                        session = %session,
                                        "archive history command applied (self)"
                                    );
                                    Self::emit_timeline_after_clear(self, Some(id));
                                    self.emit_context_snapshot_for().await;
                                    self.emit(BackendEvent::Error {
                                        session_id: None,
                                        message: format!("历史已归档：{path}"),
                                    });
                                }
                                Err(e) => self.emit(BackendEvent::Error {
                                    session_id: None,
                                    message: format!("归档失败：{e}"),
                                }),
                            }
                        }
                        None => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("team {id} 不存在"),
                        }),
                    }
                } else {
                    match self.trunk.archive_session(&session).await {
                        Ok(path) => {
                            Self::emit_timeline_after_clear(self, None);
                            self.emit_context_snapshot_for().await;
                            self.emit(BackendEvent::Error {
                                session_id: None,
                                message: format!("历史已归档：{path}"),
                            });
                        }
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
                // team 路由：解析目标 agent；无 team_id 或解析落空时在本
                // agent 上执行（`target` 即压缩落地方）。
                let target = match team_id.as_deref() {
                    Some(id) => self.registry().and_then(|r| r.resolve(id)),
                    None => None,
                };
                let result = match &target {
                    Some(agent) => agent.compact_history(keep).await,
                    None => match team_id.as_deref() {
                        Some(id) if self.team_id().as_deref() != Some(id) => {
                            Err(format!("team {id} 不存在"))
                        }
                        _ => self.compact_history(keep).await,
                    },
                };
                match result {
                    Ok(msg) => {
                        // 2026-10 上下文审计 🟡 修复：压缩落地后重发**实际
                        // 剩余**时间线 + 上下文快照——此前 CompactHistory 是
                        // 唯一不重发的会话命令（Clear/Archive 都有），面板要
                        // 等下次主动同步才看到「历史已压缩」提示行与新的
                        // token 占用。team 路由时以目标 agent 为事件源。
                        match &target {
                            Some(agent) => {
                                Self::emit_timeline_after_clear(agent, team_id.clone());
                                agent.emit_context_snapshot_for().await;
                            }
                            None => {
                                Self::emit_timeline_after_clear(self, team_id.clone());
                                self.emit_context_snapshot_for().await;
                            }
                        }
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: msg,
                        });
                    }
                    Err(e) => self.emit(BackendEvent::Error {
                        session_id: None,
                        message: format!("压缩失败：{e}"),
                    }),
                }
            }
            // 团队 CRUD（SaveTeam / DeleteTeam / ToggleTeam）：**进程级命令**，
            // 由 [`crate::process_commands`] 在命令泵里先处理，不该到达任何
            // 人格。这里保留一个 fail-closed 兜底——真到了这里说明命令泵的
            // 接线坏了，要能看见（改造前它们是 `require_admin()` + 静默降级）。
            BackendCommand::SaveTeam { .. }
            | BackendCommand::DeleteTeam { .. }
            | BackendCommand::ToggleTeam { .. } => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "团队管理命令应由进程级处理器处理（命令泵接线异常）".into(),
                });
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
                    session: session.info(String::new(), &self.node_identity()),
                });
                self.emit(BackendEvent::MessageReceived {
                    session_id: sid.clone(),
                    instance_name: "local".into(),
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
            // ---- PluginInstance management ----
            // 实例（adapter）命令域已迁进 `echo-plugin-tools`（全插件化 S3：
            // 与 `instance_*` 工具同一个插件；清单投影也搬过去了——"这个平台
            // 有哪些展示字段"是适配器能力的知识）。这里只剩"插件未启用"的
            // 兜底：面板点了按钮必须有回应，不能静默。
            BackendCommand::StartInstance { .. }
            | BackendCommand::StopInstance { .. }
            | BackendCommand::RestartInstance { .. }
            | BackendCommand::RequestInstanceStatus
            | BackendCommand::StartAllInstances
            | BackendCommand::StopAllInstances => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message:
                        "实例命令由 echo-agent.tools.builtin 插件处理，但该插件未启用".into(),
                });
            }
            BackendCommand::InvokeInstanceOp { .. } => {
                // 通用实例管理方法转发也归 `echo-plugin-tools`（见
                // `instance_ops`）；未启用时明确报错，不让面板等超时。
                self.emit(BackendEvent::InstanceOpResult {
                    instance: String::new(),
                    method: String::new(),
                    result: serde_json::Value::Null,
                    error: Some(
                        "实例命令由 echo-agent.tools.builtin 插件处理，但该插件未启用".into(),
                    ),
                });
            }
            // 工作区命令域：**已迁入 `echo-agent.workspace` 插件**
            // （`Contribution::Command`）。走上面的插件分发路径；这里只剩
            // "插件没装/被禁用"的兜底——那种情况下用户点了按钮得有回应，
            // 不能静默。
            BackendCommand::RequestWorkspaceSessions { .. }
            | BackendCommand::SaveWorkspaceSession { .. }
            | BackendCommand::DeleteWorkspaceSession { .. }
            | BackendCommand::ActivateWorkspaceSession { .. }
            | BackendCommand::RequestWorkspaceGitStatus { .. }
            | BackendCommand::RequestWorkspaceFiles { .. }
            | BackendCommand::RequestBrowseDirectories { .. } => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "workspace 插件未启用（该命令由 echo-agent.workspace 处理）".into(),
                });
            }
            // 联邦 / 自更新命令域已迁进 `echo-plugin-federation`（全插件化
            // S3）：实现仍在组合根（`Federation` 句柄与 `ConfigStore` 是它的
            // 所有物，经 `ports.command_handler` 下发），但**归属关系**归插件
            // ——宿主不再需要这段 match，也不再有"谁来处理"的判断。
            BackendCommand::SaveFederationPeer { .. }
            | BackendCommand::DeleteFederationPeer { .. }
            | BackendCommand::RequestFederationStatus
            | BackendCommand::RequestFederationInvite
            | BackendCommand::MigrateSession { .. }
            | BackendCommand::RequestSelfUpdate
            | BackendCommand::RequestSelfUpdateStatus => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "联邦命令由 echo-agent.federation 插件处理，但该插件未启用".into(),
                });
            }
        }
    }

    /// Emit the full skill list (`BackendEvent::SkillsList`).
    /// 广播技能清单（**投影在插件里**：`echo_plugin_skills::commands::skills_list`）。
    ///
    /// 保留这个入口是因为"保存人格后刷新列表"发生在宿主的管理面流程里
    /// （`process_commands`），那处不必为此构造插件 handler——但它读的仍是
    /// 插件导出的同一份投影，不是第二套实现。
    pub async fn emit_skills_list(&self) {
        let dir = self.model.config.read().await.skills_dir.clone();
        let skills = self.prompt.skills.lock().await;
        let list = crate::skill::commands::skills_list(&skills, std::path::Path::new(&dir));
        drop(skills);
        self.emit(BackendEvent::SkillsList { skills: list });
    }

    /// Emit the team list (`BackendEvent::TeamsList`).
    pub async fn emit_teams_list(&self) {
        let teams = self.registry().map(|m| m.infos()).unwrap_or_default();
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
                runtime: d.runtime,
                package: d.package,
                error: d.error,
                config_origin: d.config_origin,
                state: d.state,
                restarts: d.restarts,
                generation: d.generation,
                source_path: d.source_path,
                command: d.command,
                args: d.args,
                env_keys: d.env_keys,
                required_services: d.required_services,
                optional_services: d.optional_services,
                pending_services: d.pending_services,
                subscribed_events: d.subscribed_events,
            })
            .collect();
        self.emit(BackendEvent::PluginsList { plugins });
    }

}

/// 从 `ReloadSkills` 的处理臂抽出时的批量重载助手——**现在只有测试用它**
/// （生产路径的重载已随命令域迁进 `echo-plugin-skills`，由
/// `SkillsCommandHost::reload` 表达）。保留它是为了让"跨人格生效"这条不变量
/// 继续有断言守着：等 `AgentSkillsCommandHost::reload` 的多人格版本落地后，
/// 这段和它的测试一起删。
#[cfg(test)]
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
            Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
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
            let mut names = agent.prompt.skills.lock().await.names();
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
