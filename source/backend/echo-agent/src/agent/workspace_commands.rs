//! Workspace-plugin frontend command handling（`echo-agent.workspace`）。
//!
//! 与 QQ 命令处理同构：命令来自 Panel（Frontend 清关），按 persona 路由
//! （`team_id` 必填，去主智能体后无默认人格兜底）。状态由每 persona 的
//! [`WorkspaceStore`](crate::workspace::WorkspaceStore) 持有并即时持久化；
//! git 状态是只读采集（`spawn_blocking` + 超时保护）。

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
                        self.emit_workspace_sessions();
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
                        self.emit_workspace_sessions();
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
                        self.emit_workspace_sessions();
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
            BackendCommand::RequestWorkspaceGitStatus { session_id, .. } => {
                let Some(store) = self.workspace_store() else {
                    self.emit_workspace_unavailable();
                    return;
                };
                let Some(session) = store.get(&session_id) else {
                    self.emit_workspace_error(format!("工作区会话 {session_id} 不存在"));
                    return;
                };
                let directories = session.directories.clone();
                let collected = tokio::task::spawn_blocking(move || {
                    directories
                        .iter()
                        .map(|dir| crate::workspace::collect_dir_git(dir))
                        .collect::<Vec<_>>()
                })
                .await;
                match collected {
                    Ok(directories) => {
                        self.emit(BackendEvent::WorkspaceGitStatus {
                            team_id: self.team_id(),
                            session_id,
                            directories,
                        });
                    }
                    Err(error) => {
                        self.emit_workspace_error(format!("git 状态采集失败: {error}"));
                    }
                }
            }
            _ => {}
        }
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
