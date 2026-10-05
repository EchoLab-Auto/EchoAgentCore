//! Workspace-plugin frontend command handling（`echo-agent.workspace`）。
//!
//! 与 QQ 命令处理同构：命令来自 Panel（Frontend 清关），按 persona 路由
//! （`team_id` 必填，去主智能体后无默认人格兜底）。状态由每 persona 的
//! [`WorkspaceStore`](crate::workspace::WorkspaceStore) 持有并即时持久化；
//! git 状态是只读采集（`spawn_blocking` + 超时保护）。

/// 远程 git 状态占位（对端离线/旧版/失败降级）：保留 `node://` 目录
/// 标注与错误说明，Panel 显示为不可采集条目（与旧硬编码占位同形态）。
fn remote_git_placeholder(
    qualified_directory: &str,
    message: String,
) -> echo_protocol::WorkspaceGitInfo {
    echo_protocol::WorkspaceGitInfo {
        directory: qualified_directory.to_string(),
        is_repo: false,
        branch: None,
        ahead: 0,
        behind: 0,
        staged: 0,
        modified: 0,
        untracked: 0,
        changed_files: Vec::new(),
        last_commit: None,
        error: Some(message),
    }
}

use crate::agent::Agent;
use crate::command::BackendCommand;
use crate::event::BackendEvent;

impl Agent {
    /// Handle the workspace-specific command variants. Called by
    /// `apply_command` when the command is one of the workspace domain.
    pub(crate) async fn apply_workspace_command(&self, cmd: BackendCommand) {
        match cmd {
            BackendCommand::RequestWorkspaceSessions { .. } => {
                self.emit_workspace_sessions();
            }
            BackendCommand::SaveWorkspaceSession { session, .. } => {
                let Some(store) = self.workspace_store() else {
                    self.emit_workspace_unavailable();
                    return;
                };
                match store.upsert(session) {
                    Ok(saved) => {
                        // 列表/激活广播走 store 变更钩子（workspace_after_change）。
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("工作区会话已保存：{}（{}）", saved.name, saved.id),
                        });
                    }
                    Err(message) => self.emit_workspace_error(message),
                }
            }
            BackendCommand::DeleteWorkspaceSession { id, .. } => {
                let Some(store) = self.workspace_store() else {
                    self.emit_workspace_unavailable();
                    return;
                };
                match store.delete(&id) {
                    Ok(true) => {
                        // 列表/激活广播走 store 变更钩子（workspace_after_change）。
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message: format!("工作区会话已删除：{id}"),
                        });
                    }
                    Ok(false) => self.emit_workspace_error(format!("工作区会话 {id} 不存在")),
                    Err(message) => self.emit_workspace_error(message),
                }
            }
            BackendCommand::ActivateWorkspaceSession { id, .. } => {
                let Some(store) = self.workspace_store() else {
                    self.emit_workspace_unavailable();
                    return;
                };
                match store.set_active(id.clone()) {
                    Ok(()) => {
                        // 激活 = 进入项目对话：通道会话注册 + 列表广播都走
                        // store 变更钩子（workspace_after_change）。
                        let message = match id {
                            Some(id) => format!("已激活工作区会话：{id}"),
                            None => "已取消工作区会话激活".into(),
                        };
                        self.emit(BackendEvent::Error {
                            session_id: None,
                            message,
                        });
                    }
                    Err(message) => self.emit_workspace_error(message),
                }
            }
            BackendCommand::RequestWorkspaceFiles {
                session_id, path, ..
            } => {
                let Some(store) = self.workspace_store() else {
                    self.emit_workspace_unavailable();
                    return;
                };
                let Some(session) = store.get(&session_id) else {
                    self.emit_workspace_error(format!("工作区会话 {session_id} 不存在"));
                    return;
                };
                let directories = session.directories.clone();
                let requested = path.clone();
                // 跨机工作区（2026-10）：`node://<peer>/<绝对路径>` 前缀的
                // 路径经联邦 Query(WorkspaceFiles) 拉对端目录——文件浏览器
                // 因此能翻远程目录（此前远程目录只能本机占位）。
                if let Some(rest) = requested.strip_prefix("node://") {
                    let (peer, remote_path) = match rest.split_once('/') {
                        Some((p, r)) if !p.is_empty() => (p.to_string(), format!("/{r}")),
                        _ => {
                            self.emit(BackendEvent::WorkspaceFiles {
                                team_id: self.team_id(),
                                session_id,
                                path,
                                entries: Vec::new(),
                                error: Some("node:// 路径需形如 node://<peer>/<绝对路径>".into()),
                            });
                            return;
                        }
                    };
                    let result = match crate::federation::remote_querier() {
                        Some(querier) => {
                            querier(
                                peer.clone(),
                                echo_federation::QueryKind::WorkspaceFiles,
                                remote_path,
                            )
                            .await
                        }
                        None => Err("联邦未接线：远程文件浏览不可用".into()),
                    };
                    match result {
                        Ok(payload) => {
                            let entries =
                                serde_json::from_value::<Vec<echo_protocol::WorkspaceFileEntry>>(
                                    payload.get("entries").cloned().unwrap_or_default(),
                                )
                                .unwrap_or_default();
                            self.emit(BackendEvent::WorkspaceFiles {
                                team_id: self.team_id(),
                                session_id,
                                path,
                                entries,
                                error: None,
                            });
                        }
                        Err(message) => {
                            self.emit(BackendEvent::WorkspaceFiles {
                                team_id: self.team_id(),
                                session_id,
                                path,
                                entries: Vec::new(),
                                error: Some(message),
                            });
                        }
                    }
                    return;
                }
                let collected = tokio::task::spawn_blocking(move || {
                    let resolved =
                        crate::workspace::resolve_within_directories(&directories, &requested)?;
                    crate::workspace::collect_dir_files(&resolved)
                })
                .await;
                match collected {
                    Ok(Ok(entries)) => {
                        self.emit(BackendEvent::WorkspaceFiles {
                            team_id: self.team_id(),
                            session_id,
                            path,
                            entries,
                            error: None,
                        });
                    }
                    Ok(Err(message)) => {
                        self.emit(BackendEvent::WorkspaceFiles {
                            team_id: self.team_id(),
                            session_id,
                            path,
                            entries: Vec::new(),
                            error: Some(message),
                        });
                    }
                    Err(error) => {
                        self.emit_workspace_error(format!("文件列表采集失败: {error}"));
                    }
                }
            }
            BackendCommand::RequestWorkspaceGitStatus { session_id, .. } => {
                let Some(store) = self.workspace_store() else {
                    self.emit_workspace_unavailable();
                    return;
                };
                let Some(session) = store.get(&session_id) else {
                    self.emit_workspace_error(format!("工作区会话 {session_id} 不存在"));
                    return;
                };
                // 跨机工作区（2026-10）：远程目录经联邦
                // Query(WorkspaceGitStatus) 拉取对端 git 状态；本机目录照旧
                // spawn_blocking 采集。对端离线/旧版不认识该查询种类时降级
                // 为占位条目（与原行为一致，error 注明原因）。
                let directories = session.directories.clone();
                let mut infos: Vec<echo_protocol::WorkspaceGitInfo> = Vec::new();
                let mut local_paths: Vec<String> = Vec::new();
                for dir in &directories {
                    match dir.node() {
                        Some(node) => {
                            let qualified = format!("node://{node}/{}", dir.path());
                            let info = match crate::federation::remote_querier() {
                                Some(querier) => {
                                    match querier(
                                        node.to_string(),
                                        echo_federation::QueryKind::WorkspaceGitStatus,
                                        dir.path().to_string(),
                                    )
                                    .await
                                    {
                                        Ok(payload) => {
                                            match serde_json::from_value::<
                                                echo_protocol::WorkspaceGitInfo,
                                            >(
                                                payload.get("git").cloned().unwrap_or_default()
                                            ) {
                                                Ok(mut info) => {
                                                    info.directory = qualified.clone();
                                                    info
                                                }
                                                Err(e) => remote_git_placeholder(
                                                    &qualified,
                                                    format!("远程结果解析失败: {e}"),
                                                ),
                                            }
                                        }
                                        Err(message) => remote_git_placeholder(&qualified, message),
                                    }
                                }
                                None => remote_git_placeholder(
                                    &qualified,
                                    "联邦未接线：远程 git 状态不可用".into(),
                                ),
                            };
                            infos.push(info);
                        }
                        None => local_paths.push(dir.path().to_string()),
                    }
                }
                if !local_paths.is_empty() {
                    let collected = tokio::task::spawn_blocking(move || {
                        local_paths
                            .iter()
                            .map(|p| crate::workspace::collect_dir_git(p))
                            .collect::<Vec<_>>()
                    })
                    .await;
                    match collected {
                        Ok(mut local_infos) => infos.append(&mut local_infos),
                        Err(error) => {
                            self.emit_workspace_error(format!("git 状态采集失败: {error}"));
                            return;
                        }
                    }
                }
                self.emit(BackendEvent::WorkspaceGitStatus {
                    team_id: self.team_id(),
                    session_id,
                    directories: infos,
                });
            }
            _ => {}
        }
    }

    /// 确保当前激活工作区的本地通道会话已注册（激活 = 进入项目对话）。
    ///
    /// 返回刷新后的通道会话（昵称 = 最新工作区名），None = 未激活 / 未挂载
    /// 存储。幂等：重复调用只刷昵称与活跃时间。
    pub(crate) fn ensure_workspace_channel_for_active(&self) -> Option<crate::session::Session> {
        let store = self.workspace_store()?;
        let active = store.active()?;
        Some(
            self.trunk
                .ensure_workspace_channel(&active.id, &active.name),
        )
    }

    /// 工作区状态变更后的统一广播（store 变更钩子的唯一落点）：
    ///
    /// 1. active 存在 → 确保通道会话注册并推送 `SessionUpdated`（面板据此
    ///    把通道加进会话列表/刷新昵称）；
    /// 2. 广播 `WorkspaceSessions`（列表 + 激活标记）——面板的「本地当前
    ///    对话」按 active 投影切换（见文档 §工作区会话与项目通道）。
    ///
    /// 面板命令与模型侧 `workspace` 工具（use/create/delete）共用本路径。
    pub(crate) fn workspace_after_change(&self) {
        if let Some(session) = self.ensure_workspace_channel_for_active() {
            self.emit(BackendEvent::SessionUpdated {
                session: session.info(String::new()),
            });
        }
        self.emit_workspace_sessions();
    }

    /// Emit the current workspace session list snapshot.
    fn emit_workspace_sessions(&self) {
        let Some(store) = self.workspace_store() else {
            self.emit_workspace_unavailable();
            return;
        };
        let (sessions, active) = store.snapshot();
        self.emit(BackendEvent::WorkspaceSessions {
            team_id: self.team_id(),
            sessions,
            active,
        });
    }

    fn emit_workspace_error(&self, message: String) {
        self.emit(BackendEvent::Error {
            session_id: None,
            message,
        });
    }

    fn emit_workspace_unavailable(&self) {
        self.emit_workspace_error("workspace 插件未启用（该智能体未挂载工作区会话管理）".into());
    }
}
