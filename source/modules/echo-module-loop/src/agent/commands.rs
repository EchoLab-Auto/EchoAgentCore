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
    /// 拆出来供面板视图命令域（`echo-module-management::session_view`）按需
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
            team_id: self.team_id(),
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

    /// 会话级 API 覆盖的持久化（2026-10 用户报障修复）：把
    /// `local:workspace:` 通道的覆盖写入对应工作区对话（`profile` = `None`
    /// 清除）。非工作区会话直接跳过。落盘失败 emit `BackendEvent::Error`
    /// 带消息（不再静默——存量工作区此前 find 失败导致选择过一会「失效」）。
    pub(crate) fn persist_session_api_profile(
        self: &std::sync::Arc<Self>,
        session_id: &str,
        profile: Option<&str>,
    ) {
        // store 在模块里（独立仓库）——写入必须经命令通道回到**唯一写者**。
        // 宿主只持有镜像（只读），所以这里不能像迁移前那样直接改内存。
        if echo_module_workspace::store::parse_workspace_channel(session_id).is_none() {
            return;
        }
        let host = self.module_host();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!("no tokio runtime; workspace api profile persist skipped");
            return;
        };
        let agent = std::sync::Arc::clone(self);
        let team_id = self.team_id();
        let session_id = session_id.to_string();
        let profile = profile.map(str::to_string);
        handle.spawn(async move {
            let payload = serde_json::json!({
                "team_id": team_id,
                "session_id": session_id,
                "profile": profile,
            });
            // 回执而不是错误：持久化失败不影响运行期覆盖，也不打断正在跑的
            // turn（2026-10 作用域普查结论）。`None` = 模块未挂载，静默跳过
            // （与迁移前"没有 store 就 return"同语义）。
            if let Some(Err(message)) = host
                .dispatch_raw("WorkspaceSetConversationApiProfile", payload)
                .await
            {
                agent.emit(BackendEvent::Notice {
                    team_id: agent.team_id(),
                    session_id: Some(session_id.clone()),
                    ok: false,
                    message: format!("会话 {session_id} API 覆盖持久化失败：{message}"),
                });
            }
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
        // ── 命令域归模块（echo-fabric-arch v0.8 的 `Contribution::Command`）──
        //
        // 在落内置 match **之前**问一次模块：有模块声明处理这条命令就交给它，
        // 内置分支不再执行。于是"某个命令域归某个模块"是**注册关系**——
        // 模块未装/被禁用时，命令自然回落到内置分支（或明确报错），不需要在
        // 这里为每个模块写特判。
        //
        // **内置保留名单**：模块不能抢这些——它们是宿主的自我管理面，被抢走
        // 会让面板再也改不回来（禁用模块后无法重新启用）。命中保留名单时**不**
        // 分发，并记一条 warn（配置写错要能看见，而不是静默失效）。
        if !crate::modules::command_is_host_reserved(cmd.kind_name()) {
            // 每个受本 agent 处理的命令都会经过这里——所以**先 clone 一份**给
            // 模块分发，命中模块就返回，未命中再落下面的内置 match（此时 cmd
            // 仍然完好）。clone 的代价是每条命令一次，而命中模块时省下的是一整
            // 段宿主分支；命中不了时多一次 clone 也远小于一次面板往返。
            let probe = cmd.clone();
            // **按作用域分发**（2026-11 S10）：人格有自己的模块作用域时，
            // 命令在那一层执行——模块解析到的是**这个人格**的宿主适配器，
            // 而不是进程级核心服务代理的。无作用域（核心服务代理 / 未接线 /
            // 该命令域是进程级的）自动回落到进程级分发。
            let host = self.module_host();
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
            // ── 供应商 / API 命令域已迁进 `echo-module-provider-llm`（全模块化 S5）──
            //
            // 池的规则（`admin`）、命令名、载荷解析、事件形状与文案都在模块里
            // （`echo_module_provider_llm::commands`）；宿主接缝在
            // `AgentProviderCommandHost`（只做状态迁移的原子动作）。
            // 这里只剩"模块未启用"的兜底——模块禁用后命令明确报错，不静默。
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
                    message: "API 命令由 echo-agent.provider.llm 模块处理，但该模块未启用".into(),
                });
            }
            BackendCommand::RestartModule { id } => {
                // 防自锁：重启管理面模块 = 先卸下 management WS，Panel 立即
                // 断连且无法经界面恢复（与禁用同一条约束）。
                if id == crate::modules::MANAGEMENT_PANEL_MODULE_ID {
                    self.emit(BackendEvent::ModuleOpResult {
                        id: id.clone(),
                        op: "restart".into(),
                        ok: false,
                        message: "管理面模块承载 management 通道，重启会断开 Panel；如需重载请重启 Core 服务".into(),
                    });
                    return;
                }
                // 外部条目使用 supervisor 的热替换：replacement 握手成功后
                // 原子切换流量并排空旧进程；握手失败时旧实例继续服务。
                let result = if self.module_host().is_external_entry(&id) {
                    self.module_host()
                        .upgrade_subprocess(&id, std::time::Duration::from_secs(30))
                        .await
                } else {
                    self.module_host().restart(&id)
                };
                match result {
                    Ok(()) => {
                        self.emit(BackendEvent::ModuleOpResult {
                            id: id.clone(),
                            op: "restart".into(),
                            ok: true,
                            message: format!("{id} 已重启"),
                        });
                    }
                    Err(error) => {
                        self.emit(BackendEvent::ModuleOpResult {
                            id: id.clone(),
                            op: "restart".into(),
                            ok: false,
                            message: format!("restart {id} failed: {error}"),
                        });
                    }
                }
                // 两种结果都刷新列表：失败时状态同样可能已变（停下来了但没起来）。
                self.emit_modules_list().await;
            }
            // 工具管理面命令域已迁进 `echo-module-tools`（全模块化 S3：
            // 清单与启停本来就是这个模块拥有的东西）。这里只剩兜底。
            BackendCommand::RequestToolsList | BackendCommand::ToggleTool { .. } => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "工具命令由 echo-agent.tools.builtin 模块处理，但该模块未启用".into(),
                });
            }
            BackendCommand::RequestModulesList => {
                self.emit_modules_list().await;
            }
            BackendCommand::RenameApi { .. } => {
                // RenameApi 是**进程级**命令（改的是全局供应商池，不是某个人格），
                // 由 [`crate::process_commands`] 在命令泵里处理。这里 fail-closed。
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "API 改名应由进程级处理器处理（命令泵接线异常）".into(),
                });
            }
            // Shell 命令域已迁进 `echo-module-tools`（全模块化 S3：与 `shell_*`
            // 工具同属一个模块，会话的模型侧与面板侧不再横跨模块边界）。
            // 这里只剩"模块未启用"的兜底——模块禁用后命令明确报错，不静默。
            BackendCommand::RequestShellSessions { .. }
            | BackendCommand::ShellStart { .. }
            | BackendCommand::ShellExec { .. }
            | BackendCommand::ShellStop { .. } => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "shell 命令由 echo-agent.tools.builtin 模块处理，但该模块未启用"
                        .into(),
                });
            }
            // 技能命令域已迁进 `echo-module-skills`（全模块化 S3）：技能表、
            // frontmatter、目录校验、`.sources.json`、git 安装与列表投影都在
            // 那里。这里只剩"模块未启用"的兜底。
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
                    message: "技能命令由 echo-agent.skills.dir 模块处理，但该模块未启用".into(),
                });
            }
            // ── 会话视图命令域已迁进 `echo-module-management`（全模块化 S5）──
            //
            // 面板要看到什么（快照顺序 / 增量与全量的 full 标志 / 分页 /
            // 历史派生 / 推理瘦身）是**线契约投影**，与显示时间线的投影策略
            // 同源——那套规则已经在 `echo-module-management`（`timeline` /
            // `history` / `session_view`）。宿主只剩数据：`TrunkStore` 的窗口
            // 与事件日志。这里只是"模块未启用"的兜底。
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
                    message: "会话视图命令由 echo-agent.management.panel 模块处理，但该模块未启用"
                        .into(),
                });
            }
            // ── 会话写命令（清理 / 归档 / 压缩）已迁进 `echo-module-session` ──
            //
            // 校验（session_id 缺失 = 拒绝）、fail-closed 文案、归档进度事件、
            // 清理后重发真实剩余时间线、回执——全在模块的 `commands` 域。宿主
            // 只留"只有它做得了"的四件事（人格解析 / 压缩 / 快照 / 覆盖表），
            // 见 `AgentSessionCommandHost`。这里只剩"模块未启用"的兜底。
            BackendCommand::ClearHistory { .. }
            | BackendCommand::ArchiveHistory { .. }
            | BackendCommand::CompactHistory { .. } => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message:
                        "会话写命令（清理/归档/压缩）由 echo-agent.session 模块处理，但该模块未启用"
                            .into(),
                });
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
                // 懒加载（2026-11）：**第一次真正要用这份历史**时才读盘。
                // 启动期不读，闲置人格就不占那份 20 MB 事件日志的内存。
                self.trunk.ensure_loaded().await;
                // Parse or fall back to local TUI session key.
                let key = SessionKey::parse(&session_id).unwrap_or_else(SessionKey::local_tui);
                // 工作区通道（激活 = 进入项目对话）：昵称解析为工作区名，重建
                // 不退化（正常路径由激活 ensure 注册，这里是兜底）。
                let nickname = if key.scope == "workspace" {
                    // 昵称来自模块推送的镜像（不再有宿主 store）。
                    self.workspace_mirror()
                        .and_then(|mirror| {
                            mirror
                                .snapshot()
                                .sessions
                                .into_iter()
                                .find(|ws| ws.id == key.scope_id)
                        })
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
            // ---- ModuleInstance management ----
            // 实例（adapter）命令域已迁进 `echo-module-tools`（全模块化 S3：
            // 与 `instance_*` 工具同一个模块；清单投影也搬过去了——"这个平台
            // 有哪些展示字段"是适配器能力的知识）。这里只剩"模块未启用"的
            // 兜底：面板点了按钮必须有回应，不能静默。
            BackendCommand::StartInstance { .. }
            | BackendCommand::StopInstance { .. }
            | BackendCommand::RestartInstance { .. }
            | BackendCommand::RequestInstanceStatus
            | BackendCommand::StartAllInstances
            | BackendCommand::StopAllInstances => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "实例命令由 echo-agent.tools.builtin 模块处理，但该模块未启用".into(),
                });
            }
            BackendCommand::InvokeInstanceOp { .. } => {
                // 通用实例管理方法转发也归 `echo-module-tools`（见
                // `instance_ops`）；未启用时明确报错，不让面板等超时。
                self.emit(BackendEvent::InstanceOpResult {
                    instance: String::new(),
                    method: String::new(),
                    result: serde_json::Value::Null,
                    error: Some(
                        "实例命令由 echo-agent.tools.builtin 模块处理，但该模块未启用".into(),
                    ),
                });
            }
            // 工作区命令域：**已迁入 `echo-agent.workspace` 模块**
            // （`Contribution::Command`）。走上面的模块分发路径；这里只剩
            // "模块没装/被禁用"的兜底——那种情况下用户点了按钮得有回应，
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
                    message: "workspace 模块未启用（该命令由 echo-agent.workspace 处理）".into(),
                });
            }
            // ProDoc 文档域：由 `echo-agent.prodoc` 模块处理（上面的模块分发）。
            // 落到这里说明模块没装 / 被禁用——给明确文案，别让面板画布空转。
            BackendCommand::RequestProDocBundle { .. }
            | BackendCommand::BrowseProDocDirs { .. }
            | BackendCommand::ProDocRoots => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "prodoc 模块未启用（该命令由 echo-agent.prodoc 处理）".into(),
                });
            }
            // 联邦 / 自更新命令域已迁进 `echo-module-federation`（全模块化
            // S3）：实现仍在组合根（`Federation` 句柄与 `ConfigStore` 是它的
            // 所有物，经 `ports.command_handler` 下发），但**归属关系**归模块
            // ——宿主不再需要这段 match，也不再有"谁来处理"的判断。
            BackendCommand::SaveFederationPeer { .. }
            | BackendCommand::DeleteFederationPeer { .. }
            | BackendCommand::RequestFederationStatus
            | BackendCommand::RequestFederationDirectory
            | BackendCommand::RequestFederationInvite
            | BackendCommand::MigrateSession { .. } => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "联邦命令由 echo-agent.federation 模块处理，但该模块未启用".into(),
                });
            }
            // 自更新命令域已迁进 `echo-module-self-update`（全模块化 S5）：
            // updater 单元名、状态文件布局、重入判定都在那里。此前它们既是
            // `HOST_RESERVED`（模块不得接管）又只经模块分发可达——两条命令
            // 必失败，且文案误报"联邦模块未启用"。现在归属明确、兜底准确。
            BackendCommand::RequestSelfUpdate | BackendCommand::RequestSelfUpdateStatus => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: "自更新命令由 echo-agent.selfupdate 模块处理，但该模块未启用".into(),
                });
            }
        }
    }

    /// Emit the full skill list (`BackendEvent::SkillsList`).
    /// 广播技能清单（**投影在模块里**：`echo_module_skills::commands::skills_list`）。
    ///
    /// 保留这个入口是因为"保存人格后刷新列表"发生在宿主的管理面流程里
    /// （`process_commands`），那处不必为此构造模块 handler——但它读的仍是
    /// 模块导出的同一份投影，不是第二套实现。
    pub async fn emit_skills_list(&self) {
        if let Some(controller) = self.prompt.capability_controller() {
            controller.emit_list().await;
        }
    }

    /// Emit the team list (`BackendEvent::TeamsList`).
    pub async fn emit_teams_list(&self) {
        let teams = self.registry().map(|m| m.infos()).unwrap_or_default();
        self.emit(BackendEvent::TeamsList { teams });
    }

    /// 模块**提供**的服务键（依赖树的"我贡献什么"半边）。
    ///
    /// 这些键就是各模块 crate 里声明的服务常量（`echo-module-session` 的
    /// `SESSION_FACTORY_SERVICE`、`echo-module-loop` 的 `SERVICE_KEY` 等），
    /// Core 与模块之间的契约。大多数模块不提供任何服务（只消费宿主接缝），
    /// 返回空。
    fn provides_services_for(id: &str) -> Vec<String> {
        let keys: &[&str] = match id {
            "echo-agent.session" => &["echo-agent.session.factory"],
            "echo-agent.loop.single" => &["loop"],
            "echo-agent.shell" => &["shell"],
            "echo-agent.tools.builtin" => &["tools.builtin"],
            "echo-agent.provider.llm" => &[],
            _ => &[],
        };
        keys.iter().map(|k| k.to_string()).collect()
    }

    /// Emit the full module list (`BackendEvent::ModulesList`).
    pub async fn emit_modules_list(&self) {
        let modules: Vec<crate::event::ModuleInfo> = self
            .module_host()
            .descriptors()
            .into_iter()
            // 清单的语义是「当前挂载的模块」，不是「曾经发现的模块」：
            // 禁用即卸载，已卸载的模块不再出现在管理面清单里。
            .filter(|d| d.enabled)
            .map(|d| {
                // id 在 move 进 ModuleInfo 之前取一份，用于查 provides 映射。
                let id = d.id.clone();
                // 作用域由 kind 派生（Core 是唯一计算方，Panel 只显示）。
                // 在 move 掉 `d.kind` 之前算好。
                let gating = crate::modules::scope_of_kind(&d.kind).as_str().to_string();
                crate::event::ModuleInfo {
                    id: d.id.clone(),
                    name: d.name,
                    version: d.version,
                    kind: d.kind,
                    description: d.description,
                    entry: d.entry,
                    author: d.author,
                    builtin: d.builtin,
                    runtime: d.runtime,
                    package: d.package,
                    gating,
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
                    provides_services: Self::provides_services_for(&id),
                    optional_services: d.optional_services,
                    pending_services: d.pending_services,
                    subscribed_events: d.subscribed_events,
                }
            })
            .collect();
        self.emit(BackendEvent::ModulesList { modules });
    }
}

/// 从 `ReloadSkills` 的处理臂抽出时的批量重载助手——**现在只有测试用它**
/// （生产路径的重载已随命令域迁进 `echo-module-skills`，由
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
            Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
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
