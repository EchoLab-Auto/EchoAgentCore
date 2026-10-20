//! 工作区命令域（`echo-agent.workspace` 插件的 `Contribution::Command` 实现）。
//!
//! 这 7 条命令（`RequestWorkspaceSessions` / `SaveWorkspaceSession` /
//! `DeleteWorkspaceSession` / `ActivateWorkspaceSession` / `RequestWorkspaceGitStatus` /
//! `RequestWorkspaceFiles` / `RequestBrowseDirectories`）原本是宿主
//! `echo-agent` 里的 `impl Agent` 方法（565 行）。搬到这里之后它们仍是同一份
//! 逻辑，但**依赖方向掉了个头**：插件不再需要 `Agent`，它需要的是宿主注入的
//! 四项能力（[`WorkspaceHost`]）。
//!
//! ## 为什么是这四项
//!
//! 逐条对着原实现里的 `self.xxx()` 调用看：
//!
//! | 原来调的 | 现在 | 为什么归宿主 |
//! |---|---|---|
//! | `self.workspace_store()` | [`WorkspaceHost::store`] | store 是 per-persona 的，按 `team_id` 解析 |
//! | `self.emit(...)` | [`WorkspaceHost::emit`] | 事件出口绑在人身上（`Agent::emit` 才带 team 标注） |
//! | `self.core_browse_roots()` | [`WorkspaceHost::browse_roots`] | 要遍历**所有** persona 的 store |
//! | `self.federation().querier` | [`WorkspaceHost::remote_query`] | 联邦传输属宿主 |
//!
//! **不在这四项里的**：`workspace_after_change`——store 变更后的列表/激活广播、
//! 会话通道注册、会话级 API 恢复。那是 store 的**变更钩子**，宿主在
//! `set_workspace_store` 时就接好了；插件只要改 store，广播自动发生。这一点很
//! 重要：它让命令处理器不需要知道"会话通道"这种东西存在。
//!
//! ## 降级而不是错误
//!
//! `remote_query()` 返回 `None`（单机部署 / 联邦未接线）时，远程目录一律降级
//! 为"远程不可采集"的占位条目——与插件工具侧 `RemoteWorkspaceQuery` 的做法
//! 一致。单机部署根本走不到那条路。

use std::sync::Arc;

use serde_json::{json, Value};

use echo_protocol::{
    BackendCommand, BackendEvent, BrowseRoot, WorkspaceFileEntry, WorkspaceGitInfo,
};

use crate::{RemoteWorkspaceQuery, WorkspaceStore};

/// 宿主注入的能力（见模块文档的对照表）。
///
/// 只暴露插件**真的需要**的：多一个方法就意味着插件多知道一件宿主的内部
/// 结构，也就多一处将来搬不动的耦合。
pub trait WorkspaceHost: Send + Sync {
    /// 某个 persona 的工作区存储（`None` = 该 persona 未挂载 workspace）。
    ///
    /// 按 `team_id` 解析而不是拿一个固定 store：进程级只有一个 workspace
    /// 插件，而 store 是 per-persona 的。
    fn store(&self, team_id: &str) -> Option<Arc<WorkspaceStore>>;

    /// 发一条后端事件给前端。
    ///
    /// `team_id` 用来路由到对应的 agent（事件出口绑在人身上，`Agent::emit`
    /// 负责注入 team 标注）。`None` = 没有指定人格（发到默认/广播）。
    fn emit(&self, team_id: Option<&str>, event: BackendEvent);

    /// 目录选择器的浏览根：**全部**运行中 persona 的工作区目录并集 ∪ HOME。
    ///
    /// 为什么归宿主：一个 persona 只看自己的目录就够了，但选择器是给"人"用
    /// 的——他可能要把另一个项目的目录加进来，所以得看得见全部。
    fn browse_roots(&self) -> Vec<BrowseRoot>;

    /// 跨机查询（`node://<peer>/…`）。`None` = 联邦未接线（远程降级）。
    fn remote_query(&self) -> Option<Arc<dyn RemoteWorkspaceQuery>>;
}

/// 命令处理器：把 [`WorkspaceHost`] 包成一个可供 `Plugin::handle_command_async` 的闭包。
///
/// 之所以单独成结构体而不是函数：`with_command_handler_async` 需要
/// `Fn + Send + Sync + 'static`，而这个结构体持有 `Arc<dyn WorkspaceHost>`
/// （可克隆、线程安全），并且让"命令名 ↔ 处理分支"的对应关系在一个 `match`
/// 里看得全。
pub struct WorkspaceCommandHandler {
    host: Arc<dyn WorkspaceHost>,
}

/// 本插件处理的 7 条命令（声明用；`Contribution::Command` 的 `name`）。
///
/// 顺序与 `BackendCommand` 里的声明顺序一致，便于人工比对。
pub const COMMANDS: [&str; 7] = [
    "RequestWorkspaceSessions",
    "SaveWorkspaceSession",
    "DeleteWorkspaceSession",
    "ActivateWorkspaceSession",
    "RequestWorkspaceGitStatus",
    "RequestWorkspaceFiles",
    "RequestBrowseDirectories",
];

impl WorkspaceCommandHandler {
    /// 用宿主能力构造。
    pub fn new(host: Arc<dyn WorkspaceHost>) -> Self {
        Self { host }
    }

    /// 处理一条命令（返回 `None` = 不归本插件管）。
    ///
    /// `payload` 是**剥掉外层 tag** 的命令载荷（见 `BackendCommand::payload_value`）。
    /// 这里用 `serde_json::from_value` 把 `{"<name>": payload}` 还原成强类型的
    /// `BackendCommand`——插件依赖 `echo-protocol`，那正是共享命令词汇该在的
    /// 地方（不是框架层，也不是插件自己定义的影子类型）。
    pub async fn handle(&self, name: &str, payload: Value) -> Option<Result<Value, String>> {
        let command: BackendCommand = match serde_json::from_value(json!({ name: payload })) {
            Ok(command) => command,
            // 载荷形状不对 = 前端/协议层 bug。回明确错误比"静默不处理"好：
            // 后者会让面板转圈。
            Err(error) => {
                return Some(Err(format!("{name} 载荷解析失败: {error}")));
            }
        };
        // team_id 从命令里取（去主智能体后无默认人格兜底，缺失即报错）。
        let team_id = command_team_id(&command);
        match command {
            BackendCommand::RequestWorkspaceSessions { .. } => {
                Some(self.request_sessions(team_id.as_deref()))
            }
            BackendCommand::SaveWorkspaceSession { session, .. } => {
                Some(self.mutate(team_id.as_deref(), |store| {
                    store
                        .upsert(session)
                        .map(|saved| format!("工作区会话已保存：{}（{}）", saved.name, saved.id))
                }))
            }
            BackendCommand::DeleteWorkspaceSession { id, .. } => Some(self.mutate(
                team_id.as_deref(),
                move |store| match store.delete(&id) {
                    Ok(true) => Ok(format!("工作区会话已删除：{id}")),
                    Ok(false) => Err(format!("工作区会话 {id} 不存在")),
                    Err(message) => Err(message),
                },
            )),
            BackendCommand::ActivateWorkspaceSession {
                id, conversation, ..
            } => {
                // 激活 = 进入项目对话：通道会话注册 + 列表广播由 store 变更钩子
                // 负责（宿主在 `set_workspace_store` 时接好），这里只报回执。
                let message = match (&id, &conversation) {
                    (Some(id), Some(conv)) => format!("已激活工作区会话：{id}（对话 {conv}）"),
                    (Some(id), None) => format!("已激活工作区会话：{id}"),
                    (None, _) => "已取消工作区会话激活".into(),
                };
                Some(self.mutate(team_id.as_deref(), move |store| {
                    store.set_active(id, conversation).map(|()| message)
                }))
            }
            BackendCommand::RequestWorkspaceGitStatus { session_id, .. } => {
                Some(self.git_status(team_id.as_deref(), &session_id).await)
            }
            BackendCommand::RequestWorkspaceFiles {
                session_id, path, ..
            } => Some(self.files(team_id.as_deref(), &session_id, &path).await),
            BackendCommand::RequestBrowseDirectories { path } => Some(self.browse(path).await),
            // 不归本插件管：让位（宿主继续找）。
            _ => None,
        }
    }
}

/// 从命令里取 `team_id`（各变体字段名一致，抽出来避免 7 处重复）。
fn command_team_id(command: &BackendCommand) -> Option<String> {
    let value = serde_json::to_value(command).ok()?;
    let payload = value.as_object()?.values().next()?;
    payload
        .get("team_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl WorkspaceCommandHandler {
    /// 读快照并广播（`RequestWorkspaceSessions`）。
    fn request_sessions(&self, team_id: Option<&str>) -> Result<Value, String> {
        let store = self.store_or_err(team_id)?;
        let (sessions, active) = store.snapshot();
        let active_conversation = store.active_pair().map(|(_, conv)| conv.id);
        self.host.emit(
            team_id,
            BackendEvent::WorkspaceSessions {
                team_id: team_id.map(str::to_string),
                sessions,
                active,
                active_conversation,
            },
        );
        Ok(Value::Null)
    }

    /// 变更类命令的公共骨架：取 store → 执行 → 回执。
    ///
    /// **列表/激活广播不在这里发**：store 的变更钩子会触发宿主的
    /// `workspace_after_change`（确保通道会话 + 恢复会话级 API + 发
    /// `SessionUpdated` / `WorkspaceSessions`）。插件因此不需要知道"会话通道"
    /// 存在——这是把命令域搬进插件后仍然保持单一路径的关键。
    fn mutate<F>(&self, team_id: Option<&str>, f: F) -> Result<Value, String>
    where
        F: FnOnce(&WorkspaceStore) -> Result<String, String>,
    {
        let store = self.store_or_err(team_id)?;
        match f(&store) {
            Ok(message) => {
                self.host.emit(
                    team_id,
                    BackendEvent::Error {
                        session_id: None,
                        message,
                    },
                );
                Ok(Value::Null)
            }
            Err(message) => {
                self.emit_error(team_id, message.clone());
                Err(message)
            }
        }
    }

    fn store_or_err(&self, team_id: Option<&str>) -> Result<Arc<WorkspaceStore>, String> {
        let Some(team_id) = team_id else {
            let message = "该命令必须指定 team_id（没有默认智能体）".to_string();
            self.emit_error(None, message.clone());
            return Err(message);
        };
        match self.host.store(team_id) {
            Some(store) => Ok(store),
            None => {
                let message = "workspace 插件未启用（该智能体未挂载工作区会话管理）".to_string();
                self.emit_error(Some(team_id), message.clone());
                Err(message)
            }
        }
    }

    fn emit_error(&self, team_id: Option<&str>, message: String) {
        self.host.emit(
            team_id,
            BackendEvent::Error {
                session_id: None,
                message,
            },
        );
    }

    /// 目录浏览（`RequestBrowseDirectories`）：根列表 / 根内列举 / `node://` 远程。
    async fn browse(&self, path: Option<String>) -> Result<Value, String> {
        // 远程：node://<peer>（根列表）或 node://<peer>/<abs>（列举）。
        if let Some(raw) = path.as_deref() {
            if let Some(rest) = raw.strip_prefix("node://") {
                let (peer, remote_path) = match rest.split_once('/') {
                    Some((peer, tail)) if !peer.is_empty() => (
                        peer.to_string(),
                        Some(format!("/{}", tail.trim_start_matches('/'))),
                    ),
                    _ => (rest.to_string(), None),
                };
                if peer.is_empty() {
                    let message = "node:// 需形如 node://<peer>[/<绝对路径>]".to_string();
                    self.emit_error(None, message.clone());
                    return Err(message);
                }
                let result = match self.host.remote_query() {
                    Some(query) => {
                        query
                            .browse(peer.clone(), remote_path.clone().unwrap_or_default())
                            .await
                    }
                    None => Err("联邦未接线：远程目录浏览不可用".into()),
                };
                let (roots, entries, error) = match result {
                    Ok(payload) => {
                        // 对端返回的 roots/entries 路径是**对端机器**上的绝对
                        // 路径——补 `node://<peer>/` 前缀（面板显示与选择都直接
                        // 用，2026-10）。
                        let mut roots: Vec<BrowseRoot> = serde_json::from_value(
                            payload.get("roots").cloned().unwrap_or_default(),
                        )
                        .unwrap_or_default();
                        for root in &mut roots {
                            root.path =
                                format!("node://{peer}/{}", root.path.trim_start_matches('/'));
                        }
                        let mut entries: Vec<WorkspaceFileEntry> = serde_json::from_value(
                            payload.get("entries").cloned().unwrap_or_default(),
                        )
                        .unwrap_or_default();
                        qualify_remote_entries(&peer, &mut entries);
                        (roots, entries, None)
                    }
                    Err(message) => (Vec::new(), Vec::new(), Some(message)),
                };
                self.host.emit(
                    None,
                    BackendEvent::BrowseDirectories {
                        path,
                        roots,
                        entries,
                        error,
                    },
                );
                return Ok(Value::Null);
            }
        }
        // 本机：浏览根 = 全部运行中 persona 的工作区目录并集 ∪ HOME。
        let roots = self.host.browse_roots();
        match path {
            None => {
                self.host.emit(
                    None,
                    BackendEvent::BrowseDirectories {
                        path: None,
                        roots,
                        entries: Vec::new(),
                        error: None,
                    },
                );
            }
            Some(requested) => {
                let wanted = requested.clone();
                let collected = tokio::task::spawn_blocking(move || {
                    let resolved = crate::resolve_within_browse_roots(&roots, &wanted)?;
                    crate::collect_child_dirs(&resolved)
                })
                .await;
                let (entries, error) = match collected {
                    Ok(Ok(entries)) => (entries, None),
                    Ok(Err(message)) => (Vec::new(), Some(message)),
                    Err(error) => (Vec::new(), Some(format!("目录浏览失败: {error}"))),
                };
                self.host.emit(
                    None,
                    BackendEvent::BrowseDirectories {
                        path: Some(requested),
                        roots: Vec::new(),
                        entries,
                        error,
                    },
                );
            }
        }
        Ok(Value::Null)
    }

    /// 文件列表（`RequestWorkspaceFiles`）：限定在工作区目录内，支持远程。
    async fn files(
        &self,
        team_id: Option<&str>,
        session_id: &str,
        path: &str,
    ) -> Result<Value, String> {
        let store = self.store_or_err(team_id)?;
        let Some(session) = store.get(session_id) else {
            let message = format!("工作区会话 {session_id} 不存在");
            self.emit_error(team_id, message.clone());
            return Err(message);
        };
        let directories = session.directories.clone();
        let requested = path.to_string();
        let session_id = session_id.to_string();
        // 跨机工作区：`node://<peer>/<绝对路径>` 经联邦拉对端目录。
        if let Some(rest) = requested.strip_prefix("node://") {
            let (peer, remote_path) = match rest.split_once('/') {
                Some((peer, tail)) if !peer.is_empty() => (
                    peer.to_string(),
                    format!("/{}", tail.trim_start_matches('/')),
                ),
                _ => {
                    let message = "node:// 路径需形如 node://<peer>/<绝对路径>".to_string();
                    self.host.emit(
                        team_id,
                        BackendEvent::WorkspaceFiles {
                            team_id: team_id.map(str::to_string),
                            session_id,
                            path: requested,
                            entries: Vec::new(),
                            error: Some(message.clone()),
                        },
                    );
                    return Err(message);
                }
            };
            let result = match self.host.remote_query() {
                Some(query) => query.list_files(peer.clone(), remote_path).await,
                None => Err("联邦未接线：远程文件浏览不可用".into()),
            };
            let (entries, error) = match result {
                Ok(payload) => {
                    let mut entries: Vec<WorkspaceFileEntry> =
                        serde_json::from_value(payload.get("entries").cloned().unwrap_or_default())
                            .unwrap_or_default();
                    // 条目路径补全：对端返回的是**对端机器上的绝对路径**，面板
                    // 下钻用 entry.path 继续发请求——不补前缀会被当成本机路径。
                    qualify_remote_entries(&peer, &mut entries);
                    (entries, None)
                }
                Err(message) => (Vec::new(), Some(message)),
            };
            self.host.emit(
                team_id,
                BackendEvent::WorkspaceFiles {
                    team_id: team_id.map(str::to_string),
                    session_id,
                    path: requested,
                    entries,
                    error,
                },
            );
            return Ok(Value::Null);
        }
        let wanted = requested.clone();
        let collected = tokio::task::spawn_blocking(move || {
            let resolved = crate::resolve_within_directories(&directories, &wanted)?;
            crate::collect_dir_files(&resolved)
        })
        .await;
        let (entries, error) = match collected {
            Ok(Ok(entries)) => (entries, None),
            Ok(Err(message)) => (Vec::new(), Some(message)),
            Err(error) => (Vec::new(), Some(format!("文件采集失败: {error}"))),
        };
        self.host.emit(
            team_id,
            BackendEvent::WorkspaceFiles {
                team_id: team_id.map(str::to_string),
                session_id,
                path: requested,
                entries,
                error,
            },
        );
        Ok(Value::Null)
    }

    /// git 状态（`RequestWorkspaceGitStatus`）：远程走联邦、本机 `spawn_blocking`。
    async fn git_status(&self, team_id: Option<&str>, session_id: &str) -> Result<Value, String> {
        let store = self.store_or_err(team_id)?;
        let Some(session) = store.get(session_id) else {
            let message = format!("工作区会话 {session_id} 不存在");
            self.emit_error(team_id, message.clone());
            return Err(message);
        };
        let directories = session.directories.clone();
        // 索引回填保持声明顺序（远程/本机混合时不被重排，2026-10）。
        let mut infos: Vec<Option<WorkspaceGitInfo>> = vec![None; directories.len()];
        let mut local: Vec<(usize, String)> = Vec::new();
        let remote = self.host.remote_query();
        for (index, dir) in directories.iter().enumerate() {
            match dir.node() {
                Some(node) => {
                    let qualified = format!("node://{node}/{}", dir.path().trim_start_matches('/'));
                    let info = match &remote {
                        Some(query) => {
                            match query
                                .git_status(node.to_string(), dir.path().to_string())
                                .await
                            {
                                Ok(payload) => match serde_json::from_value::<WorkspaceGitInfo>(
                                    payload.get("git").cloned().unwrap_or_default(),
                                ) {
                                    Ok(mut info) => {
                                        info.directory = qualified.clone();
                                        info
                                    }
                                    Err(error) => remote_git_placeholder(
                                        &qualified,
                                        format!("远程结果解析失败: {error}"),
                                    ),
                                },
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
            let paths: Vec<String> = local.iter().map(|(_, path)| path.clone()).collect();
            let collected = tokio::task::spawn_blocking(move || {
                paths
                    .iter()
                    .map(|path| crate::collect_dir_git(path))
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
                    let message = format!("git 状态采集失败: {error}");
                    for (index, path) in local {
                        infos[index] =
                            Some(remote_git_placeholder(&path, format!("采集失败: {error}")));
                    }
                    self.emit_error(team_id, message);
                }
            }
        }
        let directories: Vec<WorkspaceGitInfo> = infos.into_iter().flatten().collect();
        self.host.emit(
            team_id,
            BackendEvent::WorkspaceGitStatus {
                team_id: team_id.map(str::to_string),
                session_id: session_id.to_string(),
                directories,
            },
        );
        Ok(Value::Null)
    }
}

/// 远程浏览条目路径补全：`node://<peer>/<对端绝对路径>`（单斜杠规范）。
///
/// 对端 `collect_dir_files` 返回的是对端机器的绝对路径；面板文件浏览器下钻用
/// `entry.path` 继续发起请求，必须带 `node://` 限定才能路由回对端
/// （2026-10 修复：此前远程下钻被当成本机路径）。
fn qualify_remote_entries(peer: &str, entries: &mut [WorkspaceFileEntry]) {
    for entry in entries {
        entry.path = format!("node://{peer}/{}", entry.path.trim_start_matches('/'));
    }
}

/// 远程 git 状态占位（对端离线 / 未实现 / 失败降级）：保留 `node://` 标注与
/// 错误说明，面板显示为不可采集条目（与旧硬编码占位同形态）。
fn remote_git_placeholder(directory: &str, message: String) -> WorkspaceGitInfo {
    WorkspaceGitInfo {
        directory: directory.to_string(),
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

    /// 远程条目路径补全：单斜杠规范、`name` 不受影响。
    ///
    /// 为什么这条必须有：面板文件浏览器**下钻**时用 `entry.path` 继续发请求，
    /// 不补 `node://<peer>/` 前缀会被当成本机路径（2026-10 修过的真实 bug）。
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
