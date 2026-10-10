//! Workspace-plugin frontend command handling（`echo-agent.workspace`）。
//!
//! 与 QQ 命令处理同构：命令来自 Panel（Frontend 清关），按 persona 路由
//! （`team_id` 必填，去主智能体后无默认人格兜底）。状态由每 persona 的
//! [`WorkspaceStore`](crate::workspace::WorkspaceStore) 持有并即时持久化；
//! git 状态是只读采集（`spawn_blocking` + 超时保护）。

/// 远程浏览条目路径补全：`node://<peer>/<对端绝对路径>`（单斜杠规范）。
///
/// 对端 `collect_dir_files` 返回的是对端机器的绝对路径；面板文件浏览器
/// 下钻用 `entry.path` 继续发起请求，必须带 `node://` 限定才能路由回
/// 对端（2026-10 修复：此前远程下钻被当成本机路径）。
fn qualify_remote_entries(peer: &str, entries: &mut [echo_protocol::WorkspaceFileEntry]) {
    for entry in entries {
        entry.path = format!("node://{peer}/{}", entry.path.trim_start_matches('/'));
    }
}

/// 远程 git 状态占位（对端离线/未实现查询/失败降级）：保留 `node://` 目录
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
            BackendCommand::RequestBrowseDirectories { path } => {
                self.handle_browse_directories(path).await;
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
                        Some((p, r)) if !p.is_empty() => {
                            (p.to_string(), format!("/{}", r.trim_start_matches('/')))
                        }
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
                            let mut entries =
                                serde_json::from_value::<Vec<echo_protocol::WorkspaceFileEntry>>(
                                    payload.get("entries").cloned().unwrap_or_default(),
                                )
                                .unwrap_or_default();
                            // 条目路径补全（2026-10 修复）：对端返回的是**对端
                            // 机器上的绝对路径**（如 `/srv/repo/src`），面板下钻
                            // 会用 entry.path 继续发请求——不补 `node://<peer>/`
                            // 前缀会被当成本机路径（下钻远程子目录直接失效）。
                            qualify_remote_entries(&peer, &mut entries);
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
                // spawn_blocking 采集。对端离线/未实现该查询种类时降级
                // 为占位条目（与原行为一致，error 注明原因）。
                let directories = session.directories.clone();
                // 索引回填保持声明顺序（远程/本机混合时不被重排，2026-10）。
                let mut infos: Vec<Option<echo_protocol::WorkspaceGitInfo>> =
                    vec![None; directories.len()];
                let mut local: Vec<(usize, String)> = Vec::new();
                for (index, dir) in directories.iter().enumerate() {
                    match dir.node() {
                        Some(node) => {
                            let qualified =
                                format!("node://{node}/{}", dir.path().trim_start_matches('/'));
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
                            infos[index] = Some(info);
                        }
                        None => local.push((index, dir.path().to_string())),
                    }
                }
                if !local.is_empty() {
                    let paths: Vec<String> = local.iter().map(|(_, p)| p.clone()).collect();
                    let collected = tokio::task::spawn_blocking(move || {
                        paths
                            .iter()
                            .map(|p| crate::workspace::collect_dir_git(p))
                            .collect::<Vec<_>>()
                    })
                    .await;
                    match collected {
                        Ok(local_infos) => {
                            for ((index, _), info) in local.into_iter().zip(local_infos) {
                                infos[index] = Some(info);
                            }
                        }
                        Err(error) => {
                            self.emit_workspace_error(format!("git 状态采集失败: {error}"));
                            return;
                        }
                    }
                }
                self.emit(BackendEvent::WorkspaceGitStatus {
                    team_id: self.team_id(),
                    session_id,
                    directories: infos.into_iter().flatten().collect(),
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

    /// 目录选择器浏览（2026-10）：本机浏览根 / 根内列举；`node://` 前缀
    /// 经联邦拉对端（同浏览根语义）。
    pub(crate) async fn handle_browse_directories(&self, path: Option<String>) {
        // 远程：node://<peer>（根列表）或 node://<peer>/<abs>（列举）。
        if let Some(raw) = path.as_deref() {
            if let Some(rest) = raw.strip_prefix("node://") {
                let (peer, remote_path) = match rest.split_once('/') {
                    Some((p, r)) if !p.is_empty() => (
                        p.to_string(),
                        Some(format!("/{}", r.trim_start_matches('/'))),
                    ),
                    _ => (rest.to_string(), None),
                };
                if peer.is_empty() {
                    self.emit(BackendEvent::BrowseDirectories {
                        path,
                        roots: Vec::new(),
                        entries: Vec::new(),
                        error: Some("node:// 需形如 node://<peer>[/<绝对路径>]".into()),
                    });
                    return;
                }
                let result = match crate::federation::remote_querier() {
                    Some(querier) => {
                        querier(
                            peer.clone(),
                            echo_federation::QueryKind::BrowseDirectories,
                            remote_path.clone().unwrap_or_default(),
                        )
                        .await
                    }
                    None => Err("联邦未接线：远程目录浏览不可用".into()),
                };
                match result {
                    Ok(payload) => {
                        // 对端返回的 roots/entries 路径是**对端机器**上的
                        // 绝对路径——补 `node://<peer>/` 前缀（Panel 显示与
                        // 选择都直接用，2026-10）。
                        let mut roots: Vec<echo_protocol::BrowseRoot> = serde_json::from_value(
                            payload.get("roots").cloned().unwrap_or_default(),
                        )
                        .unwrap_or_default();
                        for root in &mut roots {
                            root.path =
                                format!("node://{peer}/{}", root.path.trim_start_matches('/'));
                        }
                        let mut entries: Vec<echo_protocol::WorkspaceFileEntry> =
                            serde_json::from_value(
                                payload.get("entries").cloned().unwrap_or_default(),
                            )
                            .unwrap_or_default();
                        qualify_remote_entries(&peer, &mut entries);
                        self.emit(BackendEvent::BrowseDirectories {
                            path,
                            roots,
                            entries,
                            error: None,
                        });
                    }
                    Err(message) => {
                        self.emit(BackendEvent::BrowseDirectories {
                            path,
                            roots: Vec::new(),
                            entries: Vec::new(),
                            error: Some(message),
                        });
                    }
                }
                return;
            }
        }
        // 本机：浏览根 = 全部运行中 persona 的工作区目录并集 ∪ HOME。
        let roots = self.core_browse_roots();
        match path {
            None => {
                self.emit(BackendEvent::BrowseDirectories {
                    path: None,
                    roots,
                    entries: Vec::new(),
                    error: None,
                });
            }
            Some(requested) => {
                let for_closure = requested.clone();
                let collected = tokio::task::spawn_blocking(move || {
                    let resolved =
                        crate::workspace::resolve_within_browse_roots(&roots, &for_closure)?;
                    crate::workspace::collect_child_dirs(&resolved)
                })
                .await;
                match collected {
                    Ok(Ok(entries)) => {
                        self.emit(BackendEvent::BrowseDirectories {
                            path: Some(requested),
                            roots: Vec::new(),
                            entries,
                            error: None,
                        });
                    }
                    Ok(Err(message)) => {
                        self.emit(BackendEvent::BrowseDirectories {
                            path: Some(requested),
                            roots: Vec::new(),
                            entries: Vec::new(),
                            error: Some(message),
                        });
                    }
                    Err(error) => {
                        self.emit_workspace_error(format!("目录浏览失败: {error}"));
                    }
                }
            }
        }
    }

    /// 本 core 的浏览根：全部运行中 persona 的工作区目录并集 ∪ HOME。
    fn core_browse_roots(&self) -> Vec<echo_protocol::BrowseRoot> {
        let mut dirs: Vec<echo_protocol::WorkspaceDirectory> = Vec::new();
        let mut labels: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Some(manager) = crate::agent_manager::global_manager() {
            for running in manager.all() {
                if let Some(store) = running.agent.workspace_store() {
                    let (sessions, _) = store.snapshot();
                    for session in sessions {
                        for dir in &session.directories {
                            labels
                                .entry(dir.path().to_string())
                                .or_insert_with(|| session.name.clone());
                            dirs.push(dir.clone());
                        }
                    }
                }
            }
        }
        crate::workspace::browse_roots(&dirs, &labels)
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

#[cfg(test)]
mod tests {
    use super::qualify_remote_entries;
    use echo_protocol::WorkspaceFileEntry;

    fn entry(name: &str, path: &str, is_dir: bool) -> WorkspaceFileEntry {
        WorkspaceFileEntry {
            name: name.into(),
            path: path.into(),
            is_dir,
            size: 0,
        }
    }

    #[test]
    fn qualify_remote_entries_prefixes_peer_single_slash() {
        let mut entries = vec![
            entry("src", "/srv/repo/src", true),
            entry("main.rs", "/srv/repo/src/main.rs", false),
            // 对端路径异常带前导双斜杠：归一为单斜杠
            entry("odd", "//srv/repo/odd", false),
        ];
        qualify_remote_entries("gpu-box", &mut entries);
        assert_eq!(entries[0].path, "node://gpu-box/srv/repo/src");
        assert_eq!(entries[1].path, "node://gpu-box/srv/repo/src/main.rs");
        assert_eq!(entries[2].path, "node://gpu-box/srv/repo/odd");
        // name 不受影响（浏览器 label 用）
        assert_eq!(entries[0].name, "src");
    }
}
