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
    /// 发出**本地 TUI 会话**的上下文快照。
    ///
    /// 只用于确实没有会话维度的场景（启动期/嵌入式）。会话级命令一律用
    /// [`Self::emit_context_snapshot`] 显式传会话——`None` 口径会让一个窗口
    /// 收到别的会话的快照（2026-10 作用域普查）。
    pub async fn emit_context_snapshot_for(&self) {
        self.emit_context_snapshot(None).await;
    }

    /// 按会话发出上下文快照（多会话，2026-09）。
    ///
    /// `session_id` 缺省时**只**回退本地 TUI 会话（不存在则回空快照）——
    /// 不再退到"任一已有会话"：那是随机取值，会把别人的上下文发到当前窗口
    /// （2026-10 作用域普查）。
    pub(crate) async fn emit_context_snapshot(&self, session_id: Option<&str>) {
        for event in self.context_snapshot_events(session_id).await {
            self.emit(event);
        }
    }

    /// 上下文快照的**事件形态**（不发，只组装）。
    ///
    /// 拆出来供面板视图命令域（`echo-plugin-management::session_view`）按需
    /// 组装：命令层要知道"发几条、按什么顺序"，那属于线契约投影，不属于
    /// 会话存储。
    pub(crate) async fn context_snapshot_events(
        &self,
        session_id: Option<&str>,
    ) -> Vec<BackendEvent> {
        let resolved = match session_id {
            Some(id) => Some(id.to_string()),
            // Only the local TUI session, never "any existing session": a
            // random pick hands another window's context to this request.
            None => {
                let local = crate::session::SessionKey::local_tui().to_session_id();
                self.trunk.history_for(&local).map(|_| local)
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
        vec![BackendEvent::ContextSnapshot {
            session_id: resolved,
            messages,
            blocks,
            total_tokens,
            limit_tokens: self.trunk.memory_limit_tokens(),
        }]
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
            | BackendCommand::ClearSessionApi { team_id, .. }
            | BackendCommand::ShellStart { team_id, .. }
            | BackendCommand::ShellExec { team_id, .. }
            | BackendCommand::ShellStop { team_id, .. } => team_id.as_deref(),
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
    async fn clear_one_session(agent: &Agent, session: &str) {
        agent.trunk.clear_session(session).await;
        // Session API cleanup consistency: the history is gone, so the override
        // must go too (otherwise the session silently reuses the old profile).
        agent.model.session_providers.remove(session);
    }

    /// 清理后的闭环：重发实际剩余时间线 + 该会话的快照 + 会话级回执。
    ///
    /// 快照必须发给**被清理的会话**：走 `emit_context_snapshot(None)` 会落到
    /// 本地 TUI 会话，当前窗口的上下文面板因此不刷新（2026-10 作用域普查）。
    async fn notify_session_cleared(agent: &Agent, team_id: Option<String>, session: &str) {
        Self::emit_timeline_after_clear(agent, team_id.clone());
        agent.emit_context_snapshot(Some(session)).await;
        agent.emit(BackendEvent::Notice {
            team_id,
            session_id: Some(session.to_string()),
            ok: true,
            message: format!("会话历史已清理（{session}）"),
        });
    }

    /// 会话级 API 覆盖的持久化（2026-10 用户报障修复）：把
    /// `local:workspace:` 通道的覆盖写入对应工作区对话（`profile` = `None`
    /// 清除）。非工作区会话直接跳过。落盘失败 emit `BackendEvent::Error`
    /// 带消息（不再静默——存量工作区此前 find 失败导致选择过一会「失效」）。
    pub(crate) fn persist_session_api_profile(&self, session_id: &str, profile: Option<&str>) {
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
            // Receipt, not error: a failed persist does not affect the runtime
            // override nor the running turn (2026-10 作用域普查).
            self.emit(BackendEvent::Notice {
                team_id: self.team_id(),
                session_id: Some(session_id.to_string()),
                ok: false,
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
    pub async fn apply_command(self: &std::sync::Arc<Self>, cmd: BackendCommand) {
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
            // **按作用域分发**（2026-11 S10）：人格有自己的插件作用域时，
            // 命令在那一层执行——插件解析到的是**这个人格**的宿主适配器，
            // 而不是进程级核心服务代理的。无作用域（核心服务代理 / 未接线 /
            // 该命令域是进程级的）自动回落到进程级分发。
            let host = self.plugin_host();
            if let Some(outcome) = host.dispatch_command_for(self, &probe).await {
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
            // ── 供应商 / API 命令域已迁进 `echo-plugin-provider-llm`（全插件化 S5）──
            //
            // 池的规则（`admin`）、命令名、载荷解析、事件形状与文案都在插件里
            // （`echo_plugin_provider_llm::commands`）；宿主接缝在
            // `AgentProviderCommandHost`（只做状态迁移的原子动作）。
            // 这里只剩"插件未启用"的兜底——插件禁用后命令明确报错，不静默。
            //
            // `RenameApi` **不在**本域：它改的是全局供应商池（进程级），由
            // [`crate::process_commands`] 在命令泵里处理；落到这里说明接线异常。
            BackendCommand::SwitchModel { .. }
            | BackendCommand::SwitchProvider { .. }
            | BackendCommand::SetSystemPrompt { .. }
            | BackendCommand::RequestSystemPrompt
            | BackendCommand::UpdateApiConfig { .. }
            | BackendCommand::SwitchApi { .. }
            | BackendCommand::SetSessionApi { .. }
            | BackendCommand::ClearSessionApi { .. }
            | BackendCommand::RequestRemoteApiProfiles { .. }
            | BackendCommand::TestApi { .. }
            | BackendCommand::QueryApiBalance { .. }
            | BackendCommand::QueryApiMetrics { .. }
            | BackendCommand::DeleteApi { .. } => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "API 命令由 echo-agent.provider.llm 插件处理，但该插件未启用"
                        .into(),
                });
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
                // 外部条目使用 supervisor 的热替换：replacement 握手成功后
                // 原子切换流量并排空旧进程；握手失败时旧实例继续服务。
                let result = if self.plugin_host().is_external_entry(&id) {
                    self.plugin_host()
                        .upgrade_subprocess(&id, std::time::Duration::from_secs(30))
                        .await
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
            // ── 会话视图命令域已迁进 `echo-plugin-management`（全插件化 S5）──
            //
            // 面板要看到什么（快照顺序 / 增量与全量的 full 标志 / 分页 /
            // 历史派生 / 推理瘦身）是**线契约投影**，与显示时间线的投影策略
            // 同源——那套规则已经在 `echo-plugin-management`（`timeline` /
            // `history` / `session_view`）。宿主只剩数据：`TrunkStore` 的窗口
            // 与事件日志。这里只是"插件未启用"的兜底。
            //
            // **写操作不在本域**：ClearHistory / ArchiveHistory /
            // CompactHistory / CancelRequestedWork / SendMessage 动的是会话
            // 真源与 turn 驱动（宿主骨架），仍由下面的分支处理。
            BackendCommand::RequestState
            | BackendCommand::RequestContext { .. }
            | BackendCommand::RequestTrunkTimeline { .. }
            | BackendCommand::RequestTimelineEarlier { .. }
            | BackendCommand::RequestSessionHistory { .. } => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "会话视图命令由 echo-agent.management.panel 插件处理，但该插件未启用"
                        .into(),
                });
            }
            BackendCommand::ClearHistory {
                team_id,
                session_id,
            } => {
                // Session-scoped clear (2026-10 用户报告修复): only the target
                // session — before this it was team-only, so the panel's
                // "clear history" wiped every session of that persona (QQ
                // dm/groups, other workspace conversations).
                //
                // Migration fail-closed (2026-10 作用域普查): without a
                // session the old semantics were FULL wipe (event log +
                // projections + timeline), and no UI entry needs it (the panel
                // always sends the session). Same contract as Archive/Compact:
                // missing = stale client, refuse instead of degrading.
                let Some(session) = session_id.clone().filter(|s| !s.is_empty()) else {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message:
                            "清理历史需要指定会话（session_id 缺失——客户端过旧，请升级面板后重试）"
                                .into(),
                    });
                    return;
                };
                match team_id {
                    Some(id) => match self.registry().and_then(|r| r.resolve(&id)) {
                        Some(agent) => {
                            Self::clear_one_session(&agent, &session).await;
                            tracing::info!(team = %id, session = %session, "clear history command applied");
                            Self::notify_session_cleared(&agent, Some(id), &session).await;
                        }
                        // No manager (embedded/tests) but the command already
                        // reached the target persona (team matches): handle it
                        // ourselves, same as the RequestTrunkTimeline fallback.
                        None if self.team_id().as_deref() == Some(id.as_str()) => {
                            Self::clear_one_session(self, &session).await;
                            tracing::info!(team = %id, session = %session, "clear history command applied (self)");
                            Self::notify_session_cleared(self, Some(id), &session).await;
                        }
                        None => self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("team {id} 不存在"),
                        }),
                    },
                    // `requires_explicit_team` already rejects team-less calls;
                    // this is only a defensive fallback with the same semantics.
                    None => {
                        Self::clear_one_session(self, &session).await;
                        tracing::info!(session = %session, "clear history command applied (no team scope)");
                        Self::notify_session_cleared(self, None, &session).await;
                    }
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
                            match agent.archive_session_with_progress(&session).await {
                                Ok(path) => {
                                    tracing::info!(
                                        team = %id,
                                        session = %session,
                                        "archive history command applied"
                                    );
                                    // 归档后重发**实际剩余**的显示时间线（全量）——
                                    // 与会话级清理同一闭环（2026-10 审计：此前不重发，
                                    // 面板残留被归档会话的旧聊天）。
                                    Self::emit_timeline_after_clear(&agent, Some(id.clone()));
                                    agent.emit_context_snapshot(Some(&session)).await;
                                    agent.emit(BackendEvent::Notice {
                                        team_id: Some(id.clone()),
                                        session_id: Some(session.clone()),
                                        ok: true,
                                        message: format!("历史已归档：{path}"),
                                    });
                                }
                                Err(e) => agent.emit(BackendEvent::Notice {
                                    team_id: Some(id.clone()),
                                    session_id: Some(session.clone()),
                                    ok: false,
                                    message: format!("归档失败：{e}"),
                                }),
                            }
                        }
                        // 无管理器（嵌入式/测试）但消息已送达目标人格本体
                        // （team 匹配）时，按转发语义直接处理自身——与
                        // ClearHistory 的兜底保持一致。
                        None if self.team_id().as_deref() == Some(id.as_str()) => {
                            match self.archive_session_with_progress(&session).await {
                                Ok(path) => {
                                    tracing::info!(
                                        team = %id,
                                        session = %session,
                                        "archive history command applied (self)"
                                    );
                                    Self::emit_timeline_after_clear(self, Some(id.clone()));
                                    self.emit_context_snapshot(Some(&session)).await;
                                    self.emit(BackendEvent::Notice {
                                        team_id: Some(id.clone()),
                                        session_id: Some(session.clone()),
                                        ok: true,
                                        message: format!("历史已归档：{path}"),
                                    });
                                }
                                Err(e) => self.emit(BackendEvent::Notice {
                                    team_id: Some(id.clone()),
                                    session_id: Some(session.clone()),
                                    ok: false,
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
                    match self.archive_session_with_progress(&session).await {
                        Ok(path) => {
                            Self::emit_timeline_after_clear(self, None);
                            self.emit_context_snapshot(Some(&session)).await;
                            self.emit(BackendEvent::Notice {
                                team_id: None,
                                session_id: Some(session.clone()),
                                ok: true,
                                message: format!("历史已归档：{path}"),
                            });
                        }
                        Err(e) => self.emit(BackendEvent::Notice {
                            team_id: None,
                            session_id: Some(session.clone()),
                            ok: false,
                            message: format!("归档失败：{e}"),
                        }),
                    }
                }
            }
            BackendCommand::CompactHistory {
                team_id,
                session_id,
                keep_recent,
            } => {
                // Fail-closed (2026-10 用户报告修复): the panel button promises
                // "compact the current session". A stale client without a
                // session must NOT silently degrade to compacting every session
                // of that persona — that was the reported bug.
                let Some(session) = session_id.filter(|s| !s.is_empty()) else {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message:
                            "压缩需要指定会话（session_id 缺失——客户端过旧，请升级面板后重试）"
                                .into(),
                    });
                    return;
                };
                let keep = keep_recent.unwrap_or(40).clamp(10, 500);
                // team routing: resolve the target persona; without a team_id
                // (or an unresolved one) run on this agent.
                let target = match team_id.as_deref() {
                    Some(id) => self.registry().and_then(|r| r.resolve(id)),
                    None => None,
                };
                let result = match &target {
                    Some(agent) => agent.compact_session_history(&session, keep).await,
                    None => match team_id.as_deref() {
                        Some(id) if self.team_id().as_deref() != Some(id) => {
                            Err(format!("team {id} 不存在"))
                        }
                        _ => self.compact_session_history(&session, keep).await,
                    },
                };
                match result {
                    Ok(msg) => match &target {
                        Some(agent) => {
                            Self::emit_timeline_after_clear(agent, team_id.clone());
                            agent.emit_context_snapshot(Some(&session)).await;
                            agent.emit(BackendEvent::Notice {
                                team_id: team_id.clone(),
                                session_id: Some(session.clone()),
                                ok: true,
                                message: msg,
                            });
                        }
                        None => {
                            Self::emit_timeline_after_clear(self, team_id.clone());
                            self.emit_context_snapshot(Some(&session)).await;
                            self.emit(BackendEvent::Notice {
                                team_id: team_id.clone(),
                                session_id: Some(session.clone()),
                                ok: true,
                                message: msg,
                            });
                        }
                    },
                    Err(e) => self.emit(BackendEvent::Notice {
                        team_id: team_id.clone(),
                        session_id: Some(session.clone()),
                        ok: false,
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
                    message: "实例命令由 echo-agent.tools.builtin 插件处理，但该插件未启用".into(),
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
            | BackendCommand::RequestFederationDirectory
            | BackendCommand::RequestFederationInvite
            | BackendCommand::MigrateSession { .. } => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "联邦命令由 echo-agent.federation 插件处理，但该插件未启用".into(),
                });
            }
            // 自更新命令域已迁进 `echo-plugin-self-update`（全插件化 S5）：
            // updater 单元名、状态文件布局、重入判定都在那里。此前它们既是
            // `HOST_RESERVED`（插件不得接管）又只经插件分发可达——两条命令
            // 必失败，且文案误报"联邦插件未启用"。现在归属明确、兜底准确。
            BackendCommand::RequestSelfUpdate | BackendCommand::RequestSelfUpdateStatus => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "自更新命令由 echo-agent.selfupdate 插件处理，但该插件未启用".into(),
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
