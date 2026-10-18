//! Workspace-based session management (the `echo-agent.workspace` plugin).
//!
//! A **workspace session** is a named group of working directories that the
//! user manages from the Panel (create / rename / delete / activate) and can
//! inspect (per-directory `git status`). The active session's directories are
//! injected into the system prompt so the model knows where it is expected to
//! operate; the `workspace` tool exposes list / status / use / create /
//! delete operations to the model.
//!
//! Storage: one JSON document per persona next to the session files
//! (`echo-workspaces-{id}.json`), written atomically on every mutation.
//! Git inspection is read-only and always runs through a bounded `git` CLI
//! invocation (`GIT_TIMEOUT`), never through a shell.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use echo_protocol::{
    WorkspaceConversationInfo, WorkspaceFileEntry, WorkspaceGitInfo, WorkspaceSessionInfo,
};

use crate::tool::{Tool, ToolError};

/// 工作区命令域（`SaveWorkspaceSession` / `RequestWorkspaceFiles` 等）——
/// 由主 `apply_command` 一行委托进来（见 `agent/commands.rs`）。
pub(crate) mod commands;

/// Cap on directories per session (protects the prompt/tool output size).
pub const MAX_DIRECTORIES: usize = 32;
/// Cap on reported changed-file paths per directory.
const CHANGED_FILES_CAP: usize = 30;
/// Bound on every single `git` invocation.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on file-browser entries returned for a single directory.
pub const FILES_CAP: usize = 500;

/// The persisted document: sessions + the single active marker.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct WorkspaceDocument {
    #[serde(default)]
    active: Option<String>,
    /// 当前激活对话 id（2026-10 多对话；`active` 仍为工作区 id）。
    #[serde(default)]
    active_conversation: Option<String>,
    #[serde(default)]
    sessions: Vec<WorkspaceSessionInfo>,
}

/// 变更通知钩子：store 每次成功变更（新建/重命名/删除/激活）后触发一次。
/// 组合根在装配期经 `Agent::set_workspace_store` 注入——「变更 → 广播」
/// 的唯一入口（面板命令与模型 `use` 工具共用同一条广播路径）。
pub type ChangeHook = std::sync::Arc<dyn Fn() + Send + Sync + 'static>;

/// 对话删除钩子（2026-10 多对话）：删除成功后触发，参数 = (工作区 id,
/// 被删对话)。组合根注入——清空该对话的通道历史（事件日志/投影/时间线），
/// 面板命令与模型 `workspace` 工具共用同一路径。
pub type ConversationDeleteHook =
    std::sync::Arc<dyn Fn(&str, &WorkspaceConversationInfo) + Send + Sync + 'static>;

/// Per-persona workspace store (shared between command handlers, the tool and
/// the system-prompt builder). All mutations persist atomically.
pub struct WorkspaceStore {
    doc: RwLock<WorkspaceDocument>,
    persist: Option<PathBuf>,
    /// 变更钩子（见 [`WorkspaceStore::set_on_change`]）；None = 未接线。
    on_change: RwLock<Option<ChangeHook>>,
    /// 对话删除钩子（见 [`WorkspaceStore::set_on_conversation_deleted`]）。
    on_conversation_deleted: RwLock<Option<ConversationDeleteHook>>,
}

impl std::fmt::Debug for WorkspaceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (sessions, active) = self.snapshot();
        f.debug_struct("WorkspaceStore")
            .field("sessions", &sessions.len())
            .field("active", &active)
            .field("persist", &self.persist)
            .finish()
    }
}

impl Default for WorkspaceStore {
    fn default() -> Self {
        Self {
            doc: RwLock::new(WorkspaceDocument::default()),
            persist: None,
            on_change: RwLock::new(None),
            on_conversation_deleted: RwLock::new(None),
        }
    }
}

impl WorkspaceStore {
    /// Load a store from `persist` (missing/corrupt file → empty document;
    /// the file is rewritten on the next mutation).
    pub fn load(persist: Option<PathBuf>) -> Self {
        let doc = persist
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|raw| serde_json::from_str::<WorkspaceDocument>(&raw).ok())
            .unwrap_or_default();
        Self {
            doc: RwLock::new(doc),
            persist,
            on_change: RwLock::new(None),
            on_conversation_deleted: RwLock::new(None),
        }
    }

    /// 注册变更钩子（组合根装配期注入一次；重复调用替换旧钩子）。
    pub fn set_on_change(&self, hook: ChangeHook) {
        if let Ok(mut slot) = self.on_change.write() {
            *slot = Some(hook);
        }
    }

    /// 注册对话删除钩子（组合根装配期注入一次；用于清空被删对话的通道
    /// 历史）。钩子在 doc 锁释放后触发。
    pub fn set_on_conversation_deleted(&self, hook: ConversationDeleteHook) {
        if let Ok(mut slot) = self.on_conversation_deleted.write() {
            *slot = Some(hook);
        }
    }

    /// 触发变更通知。必须在释放 `doc` 写锁**之后**调用（钩子会回读 store），
    /// 且钩子只做广播、不得再次修改 store（避免递归）。
    fn notify_change(&self) {
        let hook = self.on_change.read().ok().and_then(|slot| slot.clone());
        if let Some(hook) = hook {
            hook();
        }
    }

    /// `(sessions, active)` snapshot.
    pub fn snapshot(&self) -> (Vec<WorkspaceSessionInfo>, Option<String>) {
        let doc = self.doc.read().unwrap_or_else(|e| e.into_inner());
        (doc.sessions.clone(), doc.active.clone())
    }

    /// One session by id.
    pub fn get(&self, id: &str) -> Option<WorkspaceSessionInfo> {
        let doc = self.doc.read().unwrap_or_else(|e| e.into_inner());
        doc.sessions.iter().find(|s| s.id == id).cloned()
    }

    /// The active session (if any).
    pub fn active(&self) -> Option<WorkspaceSessionInfo> {
        let doc = self.doc.read().unwrap_or_else(|e| e.into_inner());
        let id = doc.active.as_deref()?;
        doc.sessions.iter().find(|s| s.id == id).cloned()
    }

    /// Insert or update a session (upsert by id). Empty id → slugified name
    /// (deduplicated with a numeric suffix). Validation errors are returned
    /// as user-facing strings.
    pub fn upsert(
        &self,
        mut session: WorkspaceSessionInfo,
    ) -> Result<WorkspaceSessionInfo, String> {
        session.name = session.name.trim().to_string();
        if session.name.is_empty() {
            return Err("会话名称不能为空".into());
        }
        session.description = session.description.trim().to_string();
        // 规范化目录：去首尾空白/尾随斜杠；重复项去重（节点 + 路径联合判重）。
        // 允许暂时不存在的目录（项目可能还没克隆），存在性由 git 采集时呈现。
        // federation Phase 0：远程目录（带 node 归属）解析与保留；P3-1 起
        // 文件工具可经 node:// 前缀操作远程目录，git/文件浏览仍只作用于本机。
        let mut dirs: Vec<echo_protocol::WorkspaceDirectory> = Vec::new();
        for raw in &session.directories {
            // 跨机工作区（2026-10）：目录字符串支持 `node://<peer>/<绝对路径>`
            // 写法——untagged 线格式下 Panel 仍发纯字符串数组，Core 在这里
            // 解析为 Qualified 远程目录（此前远程目录无从经 Panel 录入，
            // 只能手写配置文件）。
            let (node, path_raw) = match raw.path().strip_prefix("node://") {
                Some(rest) => match rest.split_once('/') {
                    // 重复斜杠归一（`node://peer//srv` → `/srv`，2026-10）
                    Some((p, r)) if !p.is_empty() => (
                        Some(p.to_string()),
                        format!("/{}", r.trim_start_matches('/')),
                    ),
                    _ => (None, raw.path().to_string()),
                },
                None => (raw.node().map(str::to_string), raw.path().to_string()),
            };
            let normalized = normalize_directory(&path_raw);
            if normalized.is_empty() {
                continue;
            }
            let entry = match node {
                Some(node) => echo_protocol::WorkspaceDirectory::Qualified {
                    path: normalized,
                    node: Some(node),
                },
                None => echo_protocol::WorkspaceDirectory::Local(normalized),
            };
            if dirs
                .iter()
                .any(|d| d.node() == entry.node() && d.path() == entry.path())
            {
                continue;
            }
            dirs.push(entry);
        }
        if dirs.len() > MAX_DIRECTORIES {
            return Err(format!("工作区目录最多 {MAX_DIRECTORIES} 个"));
        }
        session.directories = dirs;

        let result = {
            let mut doc = self.doc.write().unwrap_or_else(|e| e.into_inner());
            if session.id.trim().is_empty() {
                let base = slugify(&session.name);
                let mut candidate = base.clone();
                let mut n = 2;
                while doc.sessions.iter().any(|s| s.id == candidate) {
                    candidate = format!("{base}-{n}");
                    n += 1;
                }
                session.id = candidate;
            }
            match doc.sessions.iter_mut().find(|s| s.id == session.id) {
                Some(existing) => {
                    // 元数据编辑防护（2026-10 线上事故）：Panel 等客户端编辑
                    // 名称/目录时可能不携带 conversations / last_conversation
                    // （协议两字段 serde(default) = 空 / None），不能整条覆盖
                    // 把它们清掉——否则该工作区的全部对话入口丢失。对话增删
                    // 有专门路径（upsert_conversation / delete_conversation），
                    // 空数组只解释为「本次未提供」；整组保留同时保住对话级
                    // 字段（如 api_profile）。
                    if session.conversations.is_empty() {
                        session.conversations = existing.conversations.clone();
                        // last_conversation 同为 None（未提供）且旧对话被保留
                        // 时一并保留——否则「最后使用的对话」入口同样丢失。
                        if session.last_conversation.is_none() && !session.conversations.is_empty()
                        {
                            session.last_conversation = existing.last_conversation.clone();
                        }
                    }
                    *existing = session.clone();
                }
                None => doc.sessions.push(session.clone()),
            }
            let result = session;
            self.save_locked(&doc)?;
            result
        };
        self.notify_change();
        Ok(result)
    }

    /// Delete a session; clears the active marker when it pointed at it.
    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let removed = {
            let mut doc = self.doc.write().unwrap_or_else(|e| e.into_inner());
            let before = doc.sessions.len();
            doc.sessions.retain(|s| s.id != id);
            if doc.sessions.len() == before {
                false
            } else {
                if doc.active.as_deref() == Some(id) {
                    doc.active = None;
                }
                self.save_locked(&doc)?;
                true
            }
        };
        if removed {
            self.notify_change();
        }
        Ok(removed)
    }

    /// Activate a session (`None` clears the marker). The id must exist.
    ///
    /// 2026-10 多对话：`conversation` 指定工作区内的对话；`None` = 进入
    /// `last_conversation`（无有效值时取首条；对话列表为空时物化存量主
    /// 对话 `main`）。激活即更新 `last_conversation`。
    pub fn set_active(
        &self,
        id: Option<String>,
        conversation: Option<String>,
    ) -> Result<(), String> {
        {
            let mut doc = self.doc.write().unwrap_or_else(|e| e.into_inner());
            match id {
                None => {
                    if conversation.is_some() {
                        return Err("指定对话时必须同时指定工作区".into());
                    }
                    doc.active = None;
                    doc.active_conversation = None;
                }
                Some(ref ws_id) => {
                    let Some(session) = doc.sessions.iter_mut().find(|s| &s.id == ws_id) else {
                        return Err(format!("工作区会话 {ws_id} 不存在"));
                    };
                    materialize_conversations(session);
                    let conv_id = match conversation {
                        Some(conv) => {
                            if !session.conversations.iter().any(|c| c.id == conv) {
                                return Err(format!("工作区 {ws_id} 不存在对话 {conv}"));
                            }
                            conv
                        }
                        None => session
                            .last_conversation
                            .as_deref()
                            .filter(|last| session.conversations.iter().any(|c| &c.id == last))
                            .unwrap_or(&session.conversations[0].id)
                            .to_string(),
                    };
                    session.last_conversation = Some(conv_id.clone());
                    doc.active = Some(ws_id.clone());
                    doc.active_conversation = Some(conv_id);
                }
            }
            self.save_locked(&doc)?;
        }
        self.notify_change();
        Ok(())
    }

    /// 当前激活的 `(工作区, 对话)`（读取期惰性补位：激活工作区但
    /// `active_conversation` 无效时回退首条对话 / 存量主对话）。
    pub fn active_pair(&self) -> Option<(WorkspaceSessionInfo, WorkspaceConversationInfo)> {
        let doc = self.doc.read().unwrap_or_else(|e| e.into_inner());
        let ws_id = doc.active.as_deref()?;
        let session = doc.sessions.iter().find(|s| s.id == ws_id)?;
        let convs = conversations_view(session);
        let conv = doc
            .active_conversation
            .as_deref()
            .and_then(|id| convs.iter().find(|c| c.id == id))
            .or_else(|| convs.first())?;
        Some((session.clone(), conv.clone()))
    }

    /// 某工作区的对话列表（读取视图：空列表 = 存量主对话惰性补位，
    /// 不落盘）。
    pub fn conversations_of(&self, ws_id: &str) -> Vec<WorkspaceConversationInfo> {
        let doc = self.doc.read().unwrap_or_else(|e| e.into_inner());
        match doc.sessions.iter().find(|s| s.id == ws_id) {
            Some(session) => conversations_view(session),
            None => Vec::new(),
        }
    }

    /// 新建/重命名一条对话（`conv.id` 空 = 依名称生成 slug，工作区内去重；
    /// 既有 id = 重命名，`legacy` 以既有条目为准——不可由输入伪造）。
    pub fn upsert_conversation(
        &self,
        ws_id: &str,
        mut conv: WorkspaceConversationInfo,
    ) -> Result<WorkspaceConversationInfo, String> {
        conv.name = conv.name.trim().to_string();
        if conv.name.is_empty() {
            return Err("对话名称不能为空".into());
        }
        let result = {
            let mut doc = self.doc.write().unwrap_or_else(|e| e.into_inner());
            let Some(session) = doc.sessions.iter_mut().find(|s| s.id == ws_id) else {
                return Err(format!("工作区会话 {ws_id} 不存在"));
            };
            materialize_conversations(session);
            match session.conversations.iter_mut().find(|c| c.id == conv.id) {
                Some(existing) => {
                    existing.name = conv.name.clone();
                    conv.legacy = existing.legacy;
                }
                None => {
                    if conv.id.trim().is_empty() {
                        let base = slugify_conv(&conv.name);
                        let mut candidate = base.clone();
                        let mut n = 2;
                        while session.conversations.iter().any(|c| c.id == candidate) {
                            candidate = format!("{base}-{n}");
                            n += 1;
                        }
                        conv.id = candidate;
                    } else if session.conversations.iter().any(|c| c.id == conv.id) {
                        return Err(format!("对话 id {} 已存在", conv.id));
                    }
                    // 存量裸通道 id（legacy）每工作区至多一条：列表中已有
                    // legacy 对话时新对话一律走 `<ws>/<conv>` 格式；legacy
                    // 对话被删除后新对话可再次占用裸通道（其历史已在删除时
                    // 清空）。
                    conv.legacy = !session.conversations.iter().any(|c| c.legacy);
                    session.conversations.push(conv.clone());
                }
            }
            self.save_locked(&doc)?;
            conv
        };
        self.notify_change();
        Ok(result)
    }

    /// 删除一条对话；返回被删对话（含 `legacy`，调用方据此定位通道并清空
    /// 其历史）。删除当前激活对话时切换到同工作区其余对话（列表清空则
    /// 取消激活对话标记；下次激活该工作区时自动新建）。
    pub fn delete_conversation(
        &self,
        ws_id: &str,
        conv_id: &str,
    ) -> Result<Option<WorkspaceConversationInfo>, String> {
        let removed = {
            let mut doc = self.doc.write().unwrap_or_else(|e| e.into_inner());
            let was_active = doc.active.as_deref() == Some(ws_id)
                && doc.active_conversation.as_deref() == Some(conv_id);
            let Some(session) = doc.sessions.iter_mut().find(|s| s.id == ws_id) else {
                return Err(format!("工作区会话 {ws_id} 不存在"));
            };
            materialize_conversations(session);
            match session
                .conversations
                .iter()
                .find(|c| c.id == conv_id)
                .cloned()
            {
                None => None,
                Some(removed_conv) => {
                    session.conversations.retain(|c| c.id != conv_id);
                    if session.last_conversation.as_deref() == Some(conv_id) {
                        session.last_conversation =
                            session.conversations.first().map(|c| c.id.clone());
                    }
                    if was_active {
                        doc.active_conversation =
                            session.conversations.first().map(|c| c.id.clone());
                    }
                    self.save_locked(&doc)?;
                    Some(removed_conv)
                }
            }
        };
        if let Some(ref conv) = removed {
            let hook = self
                .on_conversation_deleted
                .read()
                .ok()
                .and_then(|slot| slot.clone());
            if let Some(hook) = hook {
                hook(ws_id, conv);
            }
        }
        if removed.is_some() {
            self.notify_change();
        }
        Ok(removed)
    }

    /// System-prompt text for the **active** workspace (`None` = nothing to inject).
    ///
    /// ⚠️ 这是 **per-persona 全局** active——与"本次 turn 是哪个会话"无关。
    /// 多窗口场景下必须用 [`Self::prompt_text_for`]（按会话解析），否则 A 窗口
    /// 激活工作区后，B 窗口在自己通道里发消息会被注入 A 的工作区上下文。
    pub fn prompt_text(&self) -> Option<String> {
        let (session, conv) = self.active_pair()?;
        Some(render_prompt_text(&session, &conv))
    }

    /// System-prompt text for the workspace **owning `session_id`**
    /// （`None` = 该会话不是工作区通道，调用方决定回退策略）。
    ///
    /// 2026-10 多窗口修复：工作区上下文必须按**本次 turn 的会话**解析，不能
    /// 读全局 active。`local:workspace:<ws>[/<conv>]:<user>` → 取该工作区 +
    /// 该对话；非通道会话 → `None`。
    ///
    /// 对话解析：通道内嵌 `<conv>` 就找那条；legacy 裸通道（无 `/` 段）按
    /// `legacy == true` 定位（与 [`Self::set_legacy_conversation_api_profile`]
    /// 同一约定，不猜 id 为 `main`）。
    pub fn prompt_text_for(&self, session_id: &str) -> Option<String> {
        let (ws_id, conv_id) = parse_workspace_channel(session_id)?;
        let session = self.get(&ws_id)?;
        let convs = conversations_view(&session);
        let conv = match conv_id {
            Some(id) => convs.into_iter().find(|c| c.id == id)?,
            None => convs.into_iter().find(|c| c.legacy)?,
        };
        Some(render_prompt_text(&session, &conv))
    }

    /// API profile 改名传播（2026-10）：把全部工作区对话里
    /// `api_profile == from` 的引用改写为 `to`（`None` = 清空引用，供删除
    /// 语义复用）。一次 doc 锁内改完、只写一次盘；返回改写条数
    /// （0 = 无引用，不触发变更通知）。
    pub fn rename_api_profile(&self, from: &str, to: Option<&str>) -> Result<usize, String> {
        let changed = {
            let mut doc = self.doc.write().unwrap_or_else(|e| e.into_inner());
            let mut changed = 0usize;
            for session in doc.sessions.iter_mut() {
                for conv in session.conversations.iter_mut() {
                    if conv.api_profile.as_deref() == Some(from) {
                        conv.api_profile = to.map(str::to_string);
                        changed += 1;
                    }
                }
            }
            if changed > 0 {
                self.save_locked(&doc)?;
            }
            changed
        };
        if changed > 0 {
            self.notify_change();
        }
        Ok(changed)
    }

    /// 写锁内先物化对话（存量工作区 `conversations` 为空时先落下惰性主
    /// 对话，否则按 id 查找必然失败、覆盖静默丢失），再定位对话；工作区或
    /// 对话不存在 → `Ok(false)`。值未变化 → 不写盘、不通知，直接
    /// `Ok(true)`；否则改写、`save_locked`，释放锁后 `notify_change`。
    pub fn set_conversation_api_profile(
        &self,
        ws_id: &str,
        conv_id: &str,
        profile: Option<&str>,
    ) -> Result<bool, String> {
        self.update_conversation_api_profile(ws_id, Some(conv_id), profile)
    }

    /// 同上，但定位该工作区的 **legacy 对话**（存量裸通道，`legacy == true`）
    /// ——不依赖其 id 恰好为 `main`（面板通道 id 无 `/` 段时走本路径）。
    pub fn set_legacy_conversation_api_profile(
        &self,
        ws_id: &str,
        profile: Option<&str>,
    ) -> Result<bool, String> {
        self.update_conversation_api_profile(ws_id, None, profile)
    }

    /// 覆盖写入的公共实现：`target` = `Some(id)` 按 id 定位；`None` = 按
    /// `legacy == true` 定位（裸通道回退）。
    fn update_conversation_api_profile(
        &self,
        ws_id: &str,
        target: Option<&str>,
        profile: Option<&str>,
    ) -> Result<bool, String> {
        let profile = profile.filter(|p| !p.is_empty());
        let changed = {
            let mut doc = self.doc.write().unwrap_or_else(|e| e.into_inner());
            let Some(session) = doc.sessions.iter_mut().find(|s| s.id == ws_id) else {
                return Ok(false);
            };
            materialize_conversations(session);
            let Some(conv) = session.conversations.iter_mut().find(|c| match target {
                Some(id) => c.id == id,
                None => c.legacy,
            }) else {
                return Ok(false);
            };
            if conv.api_profile.as_deref() == profile {
                false
            } else {
                conv.api_profile = profile.map(str::to_string);
                self.save_locked(&doc)?;
                true
            }
        };
        if changed {
            self.notify_change();
        }
        Ok(true)
    }

    /// Atomic save (temp file + rename). No-op when no persist path is set.
    fn save_locked(&self, doc: &WorkspaceDocument) -> Result<(), String> {
        let Some(path) = self.persist.as_deref() else {
            return Ok(());
        };
        let raw =
            serde_json::to_string_pretty(doc).map_err(|e| format!("工作区会话序列化失败: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, raw.as_bytes())
            .map_err(|e| format!("写入 {} 失败: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("替换 {} 失败: {e}", path.display()))?;
        Ok(())
    }
}

/// Trim + strip trailing separators (`/`); keeps the path otherwise verbatim.
/// 远程 git 状态占位（对端离线/未接线/失败降级）：保留 `node://` 目录
/// 标注与错误说明。
fn remote_git_placeholder(qualified_directory: &str, message: String) -> WorkspaceGitInfo {
    WorkspaceGitInfo {
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

fn normalize_directory(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut end = trimmed.len();
    while end > 1 && trimmed[..end].ends_with('/') {
        end -= 1;
    }
    trimmed[..end].to_string()
}

/// 对话读取视图：空列表（旧档/新建工作区）惰性补位存量主对话——
/// `legacy: true` 表示其通道 id 为裸 ws 格式 `local:workspace:<ws>:local_user`
///（旧数据的历史挂在该 id 下，事件日志 append-only 不可改写）。
fn conversations_view(session: &WorkspaceSessionInfo) -> Vec<WorkspaceConversationInfo> {
    if session.conversations.is_empty() {
        vec![WorkspaceConversationInfo {
            id: "main".into(),
            name: "主对话".into(),
            legacy: true,
            api_profile: None,
        }]
    } else {
        session.conversations.clone()
    }
}

/// 写入路径物化：把读取视图的惰性主对话落进列表（之后随 doc 持久化）。
fn materialize_conversations(session: &mut WorkspaceSessionInfo) {
    if session.conversations.is_empty() {
        session.conversations = conversations_view(session);
    }
}

/// 对话通道的 `scope_id`：legacy 对话 = 裸 ws_id；其余 = `<ws_id>/<conv_id>`。
pub fn channel_scope_id(ws_id: &str, conv: &WorkspaceConversationInfo) -> String {
    if conv.legacy {
        ws_id.to_string()
    } else {
        format!("{ws_id}/{}", conv.id)
    }
}

/// 解析工作区通道的会话 id（`local:workspace:<ws>[/<conv>]:<user>`）。
///
/// 返回 `(ws_id, Some(conv_id))`（多对话通道，`<ws>/<conv>`）或
/// `(ws_id, None)`（legacy 裸通道，无 `/` 段——其对话条目由
/// [`WorkspaceStore::set_legacy_conversation_api_profile`] 按
/// `legacy == true` 定位，而不是猜 id 为 `main`）。非工作区会话（前缀不
/// 匹配）→ `None`。user 段允许 `@<account>` 后缀（多实例维度），按最后
/// 一个 `:` 剥离。
pub(crate) fn parse_workspace_channel(session_id: &str) -> Option<(String, Option<String>)> {
    let rest = session_id.strip_prefix("local:workspace:")?;
    let channel = match rest.rsplit_once(':') {
        Some((channel, _user)) => channel,
        None => rest,
    };
    if channel.is_empty() {
        return None;
    }
    match channel.split_once('/') {
        Some((ws, conv)) if !ws.is_empty() && !conv.is_empty() => {
            Some((ws.to_string(), Some(conv.to_string())))
        }
        Some(_) => None, // `ws/` 或 `/conv`：形态非法，不猜测
        None => Some((channel.to_string(), None)),
    }
}

/// 对话 id slug（slugify 的 CJK 回退改为 `conv`）。
fn slugify_conv(name: &str) -> String {
    let slug = slugify(name);
    if slug == "ws" {
        "conv".into()
    } else {
        slug
    }
}

/// ASCII slug from a display name; CJK-only names fall back to `ws`.
fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true; // 不以下划线/短横开头
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    // 纯数字或空（CJK-only 名称）时回退 `ws`——id 尽量含字母，便于辨认。
    if slug.is_empty() || slug.chars().all(|ch| ch.is_ascii_digit()) {
        "ws".to_string()
    } else {
        slug
    }
}

// ── Git inspection（只读，带超时） ────────────────────────────────────────

/// Outcome of one bounded `git` invocation.
enum GitOutcome {
    Ok(String),
    /// Command ran, exited non-zero (message = stderr).
    Failed(String),
    /// Spawn / timeout / pipe error (user-facing).
    Unavailable(String),
}

/// Run `git -C <dir> <args…>` with a hard timeout. Stdout/stderr are drained
/// on dedicated threads so a verbose command cannot deadlock on a full pipe.
fn run_git(dir: &str, args: &[&str]) -> GitOutcome {
    let mut child = match Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_OPTIONAL_LOCKS", "0")
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return GitOutcome::Unavailable(format!("无法执行 git: {error}")),
    };
    let mut stdout = match child.stdout.take() {
        Some(pipe) => pipe,
        None => return GitOutcome::Unavailable("无法读取 git 输出".into()),
    };
    let mut stderr = match child.stderr.take() {
        Some(pipe) => pipe,
        None => return GitOutcome::Unavailable("无法读取 git 输出".into()),
    };
    let out_reader = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });

    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return GitOutcome::Unavailable(format!(
                        "git 命令超时（>{}s）",
                        GIT_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return GitOutcome::Unavailable(format!("等待 git 失败: {error}"));
            }
        }
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    match status {
        Some(status) if status.success() => GitOutcome::Ok(stdout),
        Some(_) => GitOutcome::Failed(stderr.trim().to_string()),
        None => GitOutcome::Unavailable("git 未返回状态".into()),
    }
}

/// Collect the read-only git status of one directory (blocking; wrap in
/// `spawn_blocking` on async paths).
pub fn collect_dir_git(directory: &str) -> WorkspaceGitInfo {
    let mut info = WorkspaceGitInfo {
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
        error: None,
    };
    if !Path::new(directory).is_dir() {
        info.error = Some("目录不存在".into());
        return info;
    }
    match run_git(directory, &["rev-parse", "--is-inside-work-tree"]) {
        GitOutcome::Ok(out) if out.trim() == "true" => info.is_repo = true,
        GitOutcome::Ok(_) => {
            info.error = Some("不是 git 仓库".into());
            return info;
        }
        GitOutcome::Failed(stderr) => {
            // 探测失败 = 不是仓库（git 对非仓库目录退出非零）；stderr 仅留调试日志。
            tracing::debug!(directory = %directory, %stderr, "git repo probe failed");
            info.error = Some("不是 git 仓库".into());
            return info;
        }
        GitOutcome::Unavailable(message) => {
            info.error = Some(message);
            return info;
        }
    }

    // 分支名（detached HEAD 时 git 返回 "HEAD"）。
    if let GitOutcome::Ok(out) = run_git(directory, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        let branch = out.trim();
        if !branch.is_empty() {
            info.branch = Some(branch.to_string());
        }
    }
    // 相对上游的领先/落后（无上游时此命令失败，保持 0/0）。
    if let GitOutcome::Ok(out) = run_git(
        directory,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    ) {
        let mut parts = out.split_whitespace();
        info.behind = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        info.ahead = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    }
    // 变更清单（porcelain v1：XY<空格>路径；`??` = 未跟踪）。
    if let GitOutcome::Ok(out) = run_git(directory, &["status", "--porcelain"]) {
        for line in out.lines() {
            if line.len() < 3 {
                continue;
            }
            let mut chars = line.chars();
            let x = chars.next().unwrap_or(' ');
            let y = chars.next().unwrap_or(' ');
            let path = line[3..].to_string();
            if x == '?' && y == '?' {
                info.untracked += 1;
            } else {
                if x != ' ' && x != '?' {
                    info.staged += 1;
                }
                if y != ' ' && y != '?' {
                    info.modified += 1;
                }
            }
            if info.changed_files.len() < CHANGED_FILES_CAP {
                info.changed_files.push(path);
            }
        }
    }
    // 最近一次提交摘要（仓库无提交时失败 → None）。
    if let GitOutcome::Ok(out) = run_git(directory, &["log", "-1", "--pretty=format:%h %s"]) {
        let line = out.trim();
        if !line.is_empty() {
            info.last_commit = Some(line.to_string());
        }
    }
    info
}

/// 把 `path` 解析为会话某个工作区目录内的 canonical 路径。
///
/// 文件浏览器只允许浏览会话自己声明的目录（含子孙），其余一律拒绝：
/// - 先 canonicalize 请求路径（解析符号链接与 `..`）；
/// - 再要求它等于某个目录的 canonical 路径，或以 `目录路径 + /` 为前缀
///   （字面前缀而非 `Path::starts_with`，避免 `/a/bc` 被 `/a/b` 误放行）。
pub fn resolve_within_directories(
    directories: &[echo_protocol::WorkspaceDirectory],
    path: &str,
) -> Result<PathBuf, String> {
    let requested = Path::new(path)
        .canonicalize()
        .map_err(|e| format!("路径不存在或不可访问: {e}"))?;
    for dir in directories {
        if dir.is_remote() {
            continue; // 远程目录不参与本机路径解析（文件浏览器限本机；
                      // 远程文件操作走 node:// 前缀 + 联邦 Invoke，P3-1）
        }
        let Ok(root) = Path::new(dir.path()).canonicalize() else {
            continue; // 目录本身不存在：跳过，由其余目录决定
        };
        if requested == root {
            return Ok(requested);
        }
        let mut prefix = root.as_os_str().to_os_string();
        prefix.push("/");
        if requested
            .as_os_str()
            .as_encoded_bytes()
            .starts_with(prefix.as_encoded_bytes())
        {
            return Ok(requested);
        }
    }
    Err("路径不在该会话的工作区目录内".into())
}

/// 目录选择器的**浏览根**（2026-10）：返回可进入浏览的位置列表。
///
/// - 各工作区目录（label = 会话名/目录名，去重）；
/// - HOME 家目录（label = 「家目录 (~)」）——新项目常在家目录下，
///   且比根目录 `/` 安全得多。
///
/// 与文件浏览器的区别：文件浏览器限「会话已声明的目录内」；浏览根
/// 面向"选择新目录"，必须能在**未声明**的路径里浏览（否则鸡生蛋）。
pub fn browse_roots(
    directories: &[echo_protocol::WorkspaceDirectory],
    labels: &std::collections::HashMap<String, String>,
) -> Vec<echo_protocol::BrowseRoot> {
    let mut out: Vec<echo_protocol::BrowseRoot> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // 1) 家目录在首位（最常用）
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            seen.insert(home.clone());
            out.push(echo_protocol::BrowseRoot {
                label: "家目录 (~)".into(),
                path: home,
            });
        }
    }
    // 2) 各工作区目录（本机；远程由调用方追加带 node:// 的条目）
    for dir in directories {
        if dir.is_remote() {
            continue;
        }
        let path = dir.path().to_string();
        if !seen.insert(path.clone()) {
            continue;
        }
        let label = labels.get(&path).cloned().unwrap_or_else(|| path.clone());
        out.push(echo_protocol::BrowseRoot { label, path });
    }
    out
}

/// 浏览根内校验：`path` 的 canonical 形式必须落在任一浏览根的 canonical
/// 前缀内（字面前缀 + `/`，避免 `/a/bc` 被 `/a/b` 误放行）。返回
/// canonical 路径。
pub fn resolve_within_browse_roots(
    roots: &[echo_protocol::BrowseRoot],
    path: &str,
) -> Result<PathBuf, String> {
    let requested = Path::new(path)
        .canonicalize()
        .map_err(|e| format!("路径不存在或不可访问: {e}"))?;
    for root in roots {
        if root.path.starts_with("node://") {
            continue; // 远程根不参与本机解析
        }
        let Ok(root_canonical) = Path::new(&root.path).canonicalize() else {
            continue;
        };
        if requested == root_canonical {
            return Ok(requested);
        }
        let mut prefix = root_canonical.as_os_str().to_os_string();
        prefix.push("/");
        if requested
            .as_os_str()
            .as_encoded_bytes()
            .starts_with(prefix.as_encoded_bytes())
        {
            return Ok(requested);
        }
    }
    Err("路径不在浏览根内（可浏览：家目录与各工作区目录）".into())
}

/// 列出一级子目录（目录选择器用；文件被过滤——选择器只选目录）。
pub fn collect_child_dirs(
    directory: &Path,
) -> Result<Vec<echo_protocol::WorkspaceFileEntry>, String> {
    let mut entries = collect_dir_files(directory)?;
    entries.retain(|e| e.is_dir);
    Ok(entries)
}

/// List one directory level for the file browser (blocking; read-only).
///
/// 目录在前、文件在后，各自按名称不区分大小写排序；超过 [`FILES_CAP`]
/// 截断。隐藏文件（`.` 开头）跳过——它们是工具链噪音（.git 等），
/// 不是用户在浏览器里找的目标。
pub fn collect_dir_files(directory: &Path) -> Result<Vec<WorkspaceFileEntry>, String> {
    let reader = std::fs::read_dir(directory).map_err(|e| format!("读取目录失败: {e}"))?;
    let mut entries: Vec<WorkspaceFileEntry> = Vec::new();
    for item in reader.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = item.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let size = if is_dir {
            0
        } else {
            item.metadata().map(|m| m.len()).unwrap_or(0)
        };
        entries.push(WorkspaceFileEntry {
            name,
            path: item.path().to_string_lossy().to_string(),
            is_dir,
            size,
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries.truncate(FILES_CAP);
    Ok(entries)
}

// ── LLM tool ─────────────────────────────────────────────────────────────

/// The `workspace` tool: model-facing access to the workspace sessions.
pub struct WorkspaceTool {
    store: std::sync::Arc<WorkspaceStore>,
}

impl WorkspaceTool {
    pub fn new(store: std::sync::Arc<WorkspaceStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for WorkspaceTool {
    fn name(&self) -> &str {
        "workspace"
    }

    fn description(&self) -> &str {
        "工作区会话管理。操作: list 列出全部会话与激活状态, status 查看某会话各目录的 git 状态(默认激活会话), use 激活会话(需要 id，可选 conversation 指定工作区内对话), create 新建会话(需要 name，可选 directories), delete 删除会话(需要 id), conversations 管理工作区内对话(需要 conversation_action: list/create/rename/delete；id 缺省 = 激活工作区；create/rename 需要 name；rename/delete 需要 conversation_id)。目录为绝对路径。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["list", "status", "use", "create", "delete", "conversations"],
                    "description": "要执行的操作"
                },
                "id": {
                    "type": "string",
                    "description": "会话 id（status/use/delete/conversations 使用；status/conversations 缺省 = 激活会话）"
                },
                "conversation": {
                    "type": "string",
                    "description": "对话 id（operation=use 时可选：激活该工作区的指定对话；缺省 = 最近使用的对话）"
                },
                "conversation_action": {
                    "type": "string",
                    "enum": ["list", "create", "rename", "delete"],
                    "description": "对话操作（operation=conversations 时使用；缺省 = list）"
                },
                "conversation_id": {
                    "type": "string",
                    "description": "对话 id（conversation_action=rename/delete 时必填）"
                },
                "name": {
                    "type": "string",
                    "description": "会话名称（operation=create 时必填）"
                },
                "directories": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "工作区目录列表（operation=create 时可选）：本机绝对路径，或跨机目录 `node://<联邦peer>/<绝对路径>`"
                }
            },
            "required": ["operation"]
        })
    }

    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        let operation = arguments
            .get("operation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("缺少 operation".into()))?;
        match operation {
            "list" => {
                let (sessions, active) = self.store.snapshot();
                Ok(serde_json::to_string_pretty(&json!({
                    "active": active,
                    "sessions": sessions,
                }))
                .unwrap_or_else(|_| "{}".into()))
            }
            "status" => {
                let session = match arguments.get("id").and_then(|v| v.as_str()) {
                    Some(id) if !id.trim().is_empty() => self
                        .store
                        .get(id)
                        .ok_or_else(|| ToolError::Execution(format!("工作区会话 {id} 不存在")))?,
                    _ => self
                        .store
                        .active()
                        .ok_or_else(|| ToolError::Execution("没有激活的工作区会话".into()))?,
                };
                // 跨机工作区（2026-10）：远程目录经联邦
                // Query(WorkspaceGitStatus) 拉对端 git 状态（与 Panel 命令
                // 路径同一逻辑）；对端离线/未接线时降级为占位条目。
                // 索引回填保持声明顺序（远程/本机混合时不被重排，2026-10）。
                let mut results: Vec<Option<WorkspaceGitInfo>> =
                    vec![None; session.directories.len()];
                let mut local: Vec<(usize, String)> = Vec::new();
                for (index, dir) in session.directories.iter().enumerate() {
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
                                            match serde_json::from_value::<WorkspaceGitInfo>(
                                                payload.get("git").cloned().unwrap_or_default(),
                                            ) {
                                                Ok(mut info) => {
                                                    info.directory = qualified;
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
                            results[index] = Some(info);
                        }
                        None => local.push((index, dir.path().to_string())),
                    }
                }
                if !local.is_empty() {
                    let paths: Vec<String> = local.iter().map(|(_, p)| p.clone()).collect();
                    let collected = tokio::task::spawn_blocking(move || {
                        paths.iter().map(|p| collect_dir_git(p)).collect::<Vec<_>>()
                    })
                    .await
                    .map_err(|e| ToolError::Execution(format!("git 采集失败: {e}")))?;
                    for ((index, _), info) in local.into_iter().zip(collected) {
                        results[index] = Some(info);
                    }
                }
                Ok(serde_json::to_string_pretty(&json!({
                    "session": session.id,
                    "name": session.name,
                    "directories": results.into_iter().flatten().collect::<Vec<_>>(),
                }))
                .unwrap_or_else(|_| "{}".into()))
            }
            "use" => {
                let id = arguments
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| ToolError::InvalidArguments("use 需要 id".into()))?;
                let conversation = arguments
                    .get("conversation")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                    .map(str::to_string);
                self.store
                    .set_active(Some(id.to_string()), conversation.clone())
                    .map_err(ToolError::Execution)?;
                match conversation {
                    Some(conv) => Ok(format!("已激活工作区会话 {id}（对话 {conv}）")),
                    None => Ok(format!("已激活工作区会话 {id}")),
                }
            }
            "conversations" => {
                let action = arguments
                    .get("conversation_action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("list");
                // 目标工作区：id 缺省 = 当前激活工作区。
                let ws_id = match arguments.get("id").and_then(|v| v.as_str()) {
                    Some(id) if !id.trim().is_empty() => id.to_string(),
                    _ => self.store.active().map(|s| s.id).ok_or_else(|| {
                        ToolError::InvalidArguments(
                            "conversations 需要 id（当前无激活工作区）".into(),
                        )
                    })?,
                };
                match action {
                    "list" => {
                        let convs = self.store.conversations_of(&ws_id);
                        if convs.is_empty() {
                            return Err(ToolError::Execution(format!("工作区会话 {ws_id} 不存在")));
                        }
                        let active = self
                            .store
                            .active_pair()
                            .filter(|(ws, _)| ws.id == ws_id)
                            .map(|(_, c)| c.id);
                        Ok(serde_json::to_string_pretty(&json!({
                            "workspace": ws_id,
                            "active_conversation": active,
                            "conversations": convs,
                        }))
                        .unwrap_or_else(|_| "{}".into()))
                    }
                    "create" => {
                        let name =
                            arguments
                                .get("name")
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| {
                                    ToolError::InvalidArguments("create 需要 name".into())
                                })?;
                        let conv = self
                            .store
                            .upsert_conversation(
                                &ws_id,
                                WorkspaceConversationInfo {
                                    id: String::new(),
                                    name: name.to_string(),
                                    legacy: false,
                                    api_profile: None,
                                },
                            )
                            .map_err(ToolError::Execution)?;
                        Ok(format!("已创建对话 {}（{}）", conv.id, conv.name))
                    }
                    "rename" => {
                        let conv_id = arguments
                            .get("conversation_id")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| {
                                ToolError::InvalidArguments("rename 需要 conversation_id".into())
                            })?;
                        let name =
                            arguments
                                .get("name")
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| {
                                    ToolError::InvalidArguments("rename 需要 name".into())
                                })?;
                        let conv = self
                            .store
                            .upsert_conversation(
                                &ws_id,
                                WorkspaceConversationInfo {
                                    id: conv_id.to_string(),
                                    name: name.to_string(),
                                    legacy: false,
                                    api_profile: None,
                                },
                            )
                            .map_err(ToolError::Execution)?;
                        Ok(format!("已重命名对话 {}（{}）", conv.id, conv.name))
                    }
                    "delete" => {
                        let conv_id = arguments
                            .get("conversation_id")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| {
                                ToolError::InvalidArguments("delete 需要 conversation_id".into())
                            })?;
                        match self
                            .store
                            .delete_conversation(&ws_id, conv_id)
                            .map_err(ToolError::Execution)?
                        {
                            Some(_) => Ok(format!("已删除对话 {conv_id}（其通道历史已清空）")),
                            None => Err(ToolError::Execution(format!(
                                "工作区 {ws_id} 不存在对话 {conv_id}"
                            ))),
                        }
                    }
                    other => Err(ToolError::InvalidArguments(format!(
                        "未知 conversation_action: {other}"
                    ))),
                }
            }
            "create" => {
                let name = arguments
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidArguments("create 需要 name".into()))?;
                let directories = arguments
                    .get("directories")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| {
                                v.as_str().map(echo_protocol::WorkspaceDirectory::local)
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let session = self
                    .store
                    .upsert(WorkspaceSessionInfo {
                        id: String::new(),
                        name: name.to_string(),
                        description: String::new(),
                        directories,
                        ..Default::default()
                    })
                    .map_err(ToolError::Execution)?;
                Ok(format!(
                    "已创建工作区会话 {}（{}，{} 个目录）",
                    session.id,
                    session.name,
                    session.directories.len()
                ))
            }
            "delete" => {
                let id = arguments
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| ToolError::InvalidArguments("delete 需要 id".into()))?;
                let removed = self.store.delete(id).map_err(ToolError::Execution)?;
                if removed {
                    Ok(format!("已删除工作区会话 {id}"))
                } else {
                    Err(ToolError::Execution(format!("工作区会话 {id} 不存在")))
                }
            }
            other => Err(ToolError::InvalidArguments(format!("未知操作: {other}"))),
        }
    }
}

/// 工作区提示词正文（单一实现，供 [`WorkspaceStore::prompt_text`]（全局 active）
/// 与 [`WorkspaceStore::prompt_text_for`]（按会话解析）两条路径共用）。
///
/// 2026-10 多窗口修复时从 `prompt_text` 抽出：编辑器里两处拼装同一段文案
/// 极易漂移，而这段文案直接决定模型看到的项目上下文。
fn render_prompt_text(session: &WorkspaceSessionInfo, conv: &WorkspaceConversationInfo) -> String {
    if session.directories.is_empty() && session.name.is_empty() {
        return String::new();
    }
    let mut text = format!("# 当前工作区会话\n{}", session.name);
    if !session.description.is_empty() {
        text.push_str(&format!("（{}）", session.description));
    }
    // 多对话（2026-10）：标注当前对话名——各对话上下文独立，
    // 名称帮助模型区分任务线。
    text.push_str(&format!("\n当前对话：{}（id: {}）", conv.name, conv.id));
    if !session.directories.is_empty() {
        text.push_str("\n工作区目录：\n");
        for dir in &session.directories {
            match dir.node() {
                // P3-1 跨机文件协作：远程目录可操作——5 个文件工具
                // （read_file/write_file/edit_file/list_files/search_code）
                // 的 path 参数用 `node://<peer>/<绝对路径>` 前缀，经联邦
                // Invoke 在对端沙箱内执行。
                Some(node) => text.push_str(&format!(
                    "- node://{node}/{} [remote:{node}]（远程节点：文件工具的 path 用 node://{node}/<绝对路径> 前缀直接读写/列目录/搜索；bash 工具可用 node://{node}/<命令> 在对端执行，如 node://{node}/cargo test）\n",
                    dir.path().trim_start_matches('/')
                )),
                None => text.push_str(&format!("- {}\n", dir.path())),
            }
        }
        text.push_str("在本机目录范围内工作；标注 [remote:<peer>] 的远程目录以 node:// 前缀路径直接操作（文件工具用 path 前缀，bash 用命令前缀；对端白名单/沙箱裁决，拒绝会原样返回）；git 状态与其它会话可用 workspace 工具查询/切换。");
    } else {
        text.push_str("\n（尚未配置工作区目录，可用 workspace 工具或面板添加）");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (std::sync::Arc<WorkspaceStore>, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "echo-workspace-test-{}-{name}.json",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
        let _ = &path; // 静默清理竞态（持久化写与删文件的时序）
        (
            std::sync::Arc::new(WorkspaceStore::load(Some(path.clone()))),
            path,
        )
    }

    #[test]
    fn upsert_generates_id_and_persists() {
        let (store, path) = temp_store("upsert");
        let session = store
            .upsert(WorkspaceSessionInfo {
                id: String::new(),
                name: "My Project".into(),
                description: "  desc  ".into(),
                directories: vec!["/tmp/proj/".into(), "/tmp/proj".into()],

                conversations: vec![],
                last_conversation: None,
            })
            .expect("upsert");
        assert_eq!(session.id, "my-project");
        assert_eq!(session.description, "desc");
        // 归一化 + 去重
        assert_eq!(session.directories, vec!["/tmp/proj".into()]);
        assert_eq!(session.directories[0].path(), "/tmp/proj");

        // 重新加载（模拟重启）
        let reloaded = WorkspaceStore::load(Some(path.clone()));
        let (sessions, active) = reloaded.snapshot();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "my-project");
        assert!(active.is_none());

        std::fs::remove_file(&path).ok();
    }

    /// 元数据编辑防护（2026-10 线上事故）：Panel 编辑名称/目录时不携带
    /// conversations / last_conversation（协议 serde(default) = 空 / None），
    /// upsert 不得整条覆盖清空对话——空数组只解释为「未提供」；整组保留
    /// 同时保住对话级字段（如 api_profile）。对话增删走专门路径
    /// （upsert_conversation / delete_conversation）。
    #[test]
    fn upsert_preserves_conversations_on_metadata_edit() {
        let (store, path) = temp_store("keep-convs");
        store
            .upsert(WorkspaceSessionInfo {
                id: "proj".into(),
                name: "Proj".into(),
                description: String::new(),
                directories: vec!["/srv/a".into()],
                conversations: vec![
                    WorkspaceConversationInfo {
                        id: "main".into(),
                        name: "主对话".into(),
                        legacy: true,
                        api_profile: None,
                    },
                    WorkspaceConversationInfo {
                        id: "api".into(),
                        name: "API".into(),
                        legacy: false,
                        api_profile: Some("alpha".into()),
                    },
                ],
                last_conversation: Some("api".into()),
            })
            .unwrap();

        // Panel 只改目录：conversations 空、last_conversation 未携带（None）。
        let saved = store
            .upsert(WorkspaceSessionInfo {
                id: "proj".into(),
                name: "Proj".into(),
                description: String::new(),
                directories: vec!["/srv/b".into()],
                conversations: vec![],
                last_conversation: None,
            })
            .unwrap();

        assert_eq!(saved.directories[0].path(), "/srv/b", "目录已更新");
        assert_eq!(saved.conversations.len(), 2, "对话不被清空: {saved:?}");
        assert_eq!(saved.conversations[0].id, "main");
        assert_eq!(
            saved.conversations[1].api_profile.as_deref(),
            Some("alpha"),
            "对话级字段整组保留"
        );
        assert_eq!(saved.last_conversation.as_deref(), Some("api"));

        // 持久化往返：重载后对话与 last_conversation 仍在。
        let reloaded = WorkspaceStore::load(Some(path.clone()));
        let ws = reloaded.get("proj").unwrap();
        assert_eq!(ws.conversations.len(), 2);
        assert_eq!(ws.last_conversation.as_deref(), Some("api"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn id_generation_dedupes() {
        let (store, path) = temp_store("dedupe");
        let a = store
            .upsert(WorkspaceSessionInfo {
                id: String::new(),
                name: "项目".into(),
                description: String::new(),
                directories: vec![],

                conversations: vec![],
                last_conversation: None,
            })
            .unwrap();
        assert_eq!(a.id, "ws"); // CJK-only name → fallback slug
        let b = store
            .upsert(WorkspaceSessionInfo {
                id: String::new(),
                name: "项目 2".into(),
                description: String::new(),
                directories: vec![],

                conversations: vec![],
                last_conversation: None,
            })
            .unwrap();
        assert_eq!(b.id, "ws-2");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn activate_delete_and_prompt_text() {
        let (store, path) = temp_store("active");
        let session = store
            .upsert(WorkspaceSessionInfo {
                id: "proj".into(),
                name: "Proj".into(),
                description: "主项目".into(),
                directories: vec!["/srv/a".into(), "/srv/b".into()],

                conversations: vec![],
                last_conversation: None,
            })
            .unwrap();
        assert!(store.prompt_text().is_none(), "未激活不进提示词");

        store.set_active(Some(session.id.clone()), None).unwrap();
        let text = store.prompt_text().expect("active prompt");
        assert!(text.contains("Proj"));
        assert!(text.contains("/srv/a"));
        assert!(text.contains("/srv/b"));

        assert!(store.set_active(Some("nope".into()), None).is_err());
        assert!(store.delete("proj").unwrap());
        assert!(store.active().is_none(), "删除激活会话后清除激活标记");
        assert!(store.prompt_text().is_none());
        std::fs::remove_file(&path).ok();
    }

    /// 2026-10 多对话：旧档（无 conversations 字段）惰性补位存量主对话——
    /// legacy 通道 id 为裸 ws 格式，不落盘；新建对话走 `<ws>/<conv>` 格式。
    #[test]
    fn conversations_lazy_legacy_main_and_channel_ids() {
        let (store, path) = temp_store("convs");
        store
            .upsert(WorkspaceSessionInfo {
                id: "proj".into(),
                name: "Proj".into(),
                ..Default::default()
            })
            .unwrap();
        // 读取视图：空列表 → 惰性主对话（不落盘）。
        let convs = store.conversations_of("proj");
        assert_eq!(convs.len(), 1);
        assert!(convs[0].legacy);
        assert_eq!(convs[0].id, "main");
        assert_eq!(channel_scope_id("proj", &convs[0]), "proj");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("conversations"), "惰性补位不落盘: {raw}");

        // 新建第二条对话：主对话物化 + 新对话非 legacy。
        let conv = store
            .upsert_conversation(
                "proj",
                WorkspaceConversationInfo {
                    id: String::new(),
                    name: "支线任务".into(),
                    legacy: false,
                    api_profile: None,
                },
            )
            .unwrap();
        assert!(!conv.legacy);
        assert_eq!(channel_scope_id("proj", &conv), format!("proj/{}", conv.id));
        let convs = store.conversations_of("proj");
        assert_eq!(convs.len(), 2);
        assert!(convs.iter().any(|c| c.legacy), "存量主对话已物化");
        std::fs::remove_file(&path).ok();
    }

    /// 对话 id slug 去重 + 重命名保留 legacy。
    #[test]
    fn conversation_upsert_dedupes_and_preserves_legacy_on_rename() {
        let (store, path) = temp_store("conv-dedupe");
        store
            .upsert(WorkspaceSessionInfo {
                id: "w".into(),
                name: "W".into(),
                ..Default::default()
            })
            .unwrap();
        let a = store
            .upsert_conversation(
                "w",
                WorkspaceConversationInfo {
                    id: String::new(),
                    name: "Task".into(),
                    legacy: false,
                    api_profile: None,
                },
            )
            .unwrap();
        let b = store
            .upsert_conversation(
                "w",
                WorkspaceConversationInfo {
                    id: String::new(),
                    name: "Task".into(),
                    legacy: false,
                    api_profile: None,
                },
            )
            .unwrap();
        assert_ne!(a.id, b.id, "同名对话 id 去重");

        // 重命名存量主对话（legacy 不可由输入伪造）。
        let renamed = store
            .upsert_conversation(
                "w",
                WorkspaceConversationInfo {
                    id: "main".into(),
                    name: "主线".into(),
                    legacy: false,
                    api_profile: None,
                },
            )
            .unwrap();
        assert!(renamed.legacy, "legacy 以既有条目为准");
        assert_eq!(renamed.name, "主线");
        std::fs::remove_file(&path).ok();
    }

    /// 激活解析：不带对话 → last_conversation；删除激活对话 → 切到其余；
    /// 删空 → active_conversation 清空；全部持久化可恢复。
    #[test]
    fn conversation_activation_resolution_and_delete_switch() {
        let (store, path) = temp_store("conv-active");
        store
            .upsert(WorkspaceSessionInfo {
                id: "proj".into(),
                name: "Proj".into(),
                ..Default::default()
            })
            .unwrap();
        let c2 = store
            .upsert_conversation(
                "proj",
                WorkspaceConversationInfo {
                    id: String::new(),
                    name: "支线".into(),
                    legacy: false,
                    api_profile: None,
                },
            )
            .unwrap();

        // 激活工作区（不带对话）→ 首条（main）。
        store.set_active(Some("proj".into()), None).unwrap();
        let (_, conv) = store.active_pair().unwrap();
        assert_eq!(conv.id, "main");

        // 激活指定对话 → last_conversation 更新。
        store
            .set_active(Some("proj".into()), Some(c2.id.clone()))
            .unwrap();
        let (_, conv) = store.active_pair().unwrap();
        assert_eq!(conv.id, c2.id);

        // 再激活不带对话 → 回到 last_conversation（c2）。
        store.set_active(None, None).unwrap();
        store.set_active(Some("proj".into()), None).unwrap();
        let (_, conv) = store.active_pair().unwrap();
        assert_eq!(conv.id, c2.id, "last_conversation 生效");

        // 删除当前激活对话 → 切到 main。
        let removed = store.delete_conversation("proj", &c2.id).unwrap();
        assert!(removed.is_some());
        let (_, conv) = store.active_pair().unwrap();
        assert_eq!(conv.id, "main", "删除激活对话后切换");

        // 删除 main → 对话列表清空；工作区仍激活，读取视图惰性回到存量
        // 主对话（裸通道 id 是工作区的不灭通道——历史已在删除时清空，
        // 继续在该通道对话是安全的）。
        store.delete_conversation("proj", "main").unwrap();
        let (_, conv) = store.active_pair().unwrap();
        assert!(conv.legacy && conv.id == "main", "删空后惰性回到存量主对话");

        // 持久化往返（空列表不落盘，重载后仍是惰性视图）。
        let reloaded = WorkspaceStore::load(Some(path.clone()));
        assert_eq!(reloaded.snapshot().1.as_deref(), Some("proj"));
        let convs = reloaded.conversations_of("proj");
        assert_eq!(convs.len(), 1);
        assert!(convs[0].legacy);
        std::fs::remove_file(&path).ok();
    }

    /// 多对话提示词：当前对话名注入；对话独立通道 id 可解析往返。
    ///
    /// 多窗口修复：`prompt_text_for` 必须按**会话**取工作区，与全局 active
    /// 无关——否则 A 窗口激活 W2 后，B 在 W1 通道发消息会被注入 W2 的上下文
    /// （项目信息串台）。
    #[test]
    fn prompt_text_for_uses_session_workspace_not_global_active() {
        let unique = format!(
            "conv-prompt-for-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let (store, _path) = temp_store(&unique);
        for (id, name) in [("w1", "项目一"), ("w2", "项目二")] {
            store
                .upsert(WorkspaceSessionInfo {
                    id: id.into(),
                    name: name.into(),
                    directories: vec!["/srv/shared".into()],
                    ..Default::default()
                })
                .unwrap();
        }
        // 全局 active = w2（另一个窗口刚切走的那个）
        store.set_active(Some("w2".into()), None).unwrap();

        // 本会话是 w1 的 legacy 通道 → 必须解析出 w1，而不是 active 的 w2
        let text = store
            .prompt_text_for("local:workspace:w1:local_user")
            .expect("w1 通道应有提示词");
        assert!(text.contains("项目一"), "应按会话取 w1: {text}");
        assert!(!text.contains("项目二"), "不得混入 active 的 w2: {text}");

        // 多对话通道 `<ws>/<conv>` 同样按会话解析
        let conv = store
            .upsert_conversation(
                "w1",
                WorkspaceConversationInfo {
                    id: String::new(),
                    name: "支线".into(),
                    legacy: false,
                    api_profile: None,
                },
            )
            .unwrap();
        let text = store
            .prompt_text_for(&format!("local:workspace:w1/{}:local_user", conv.id))
            .expect("多对话通道应有提示词");
        assert!(text.contains("项目一") && text.contains("支线"), "{text}");

        // 非工作区会话 → None（调用方回退全局 active，保持单窗口旧语义）
        assert!(store.prompt_text_for("local:tui::local_user").is_none());
        assert!(store.prompt_text_for("qq:dm::42").is_none());
    }

    #[test]
    fn prompt_text_names_active_conversation() {
        let (store, path) = temp_store("conv-prompt");
        store
            .upsert(WorkspaceSessionInfo {
                id: "proj".into(),
                name: "Proj".into(),
                directories: vec!["/srv/a".into()],
                ..Default::default()
            })
            .unwrap();
        let conv = store
            .upsert_conversation(
                "proj",
                WorkspaceConversationInfo {
                    id: String::new(),
                    name: "支线任务".into(),
                    legacy: false,
                    api_profile: None,
                },
            )
            .unwrap();
        store
            .set_active(Some("proj".into()), Some(conv.id.clone()))
            .unwrap();
        let text = store.prompt_text().expect("prompt");
        assert!(text.contains("支线任务"), "提示词含对话名: {text}");
        assert!(text.contains("Proj"));

        // 通道 id 往返：scope_id = "proj/<conv>" 经 SessionKey 解析不丢。
        let scope = channel_scope_id("proj", &conv);
        let key = crate::session::SessionKey::local_workspace(&scope);
        let parsed = crate::session::SessionKey::parse(&key.to_session_id()).unwrap();
        assert_eq!(parsed.scope_id, scope);
        assert_eq!(parsed.platform, "local");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn change_hook_fires_on_mutations_only() {
        let (store, path) = temp_store("hook");
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let hits = hits.clone();
            store.set_on_change(std::sync::Arc::new(move || {
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));
        }
        let s = store
            .upsert(WorkspaceSessionInfo {
                id: "p".into(),
                name: "P".into(),
                description: String::new(),
                directories: vec![],

                conversations: vec![],
                last_conversation: None,
            })
            .unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "upsert");
        // 读取路径不触发。
        let _ = store.snapshot();
        let _ = store.prompt_text();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        store.set_active(Some(s.id.clone()), None).unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2, "激活");
        store.set_active(None, None).unwrap();
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "取消激活"
        );
        // 无效操作（不存在 id / 重复删除）不触发。
        assert!(store.set_active(Some("nope".into()), None).is_err());
        assert!(!store.delete("nope").unwrap());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
        store.delete("p").unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 4, "删除");
        std::fs::remove_file(&path).ok();
    }

    /// 钩子在锁外回调：钩子内部回读 store（active/snapshot）不得死锁。
    #[test]
    fn change_hook_can_read_the_store() {
        let (store, path) = temp_store("hook-read");
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
        {
            let observed = observed.clone();
            let weak = std::sync::Arc::downgrade(&store);
            store.set_on_change(std::sync::Arc::new(move || {
                if let Some(store) = weak.upgrade() {
                    let (sessions, active) = store.snapshot();
                    observed
                        .lock()
                        .unwrap()
                        .push(active.or(Some(format!("{} sessions", sessions.len()))));
                }
            }));
        }
        store
            .upsert(WorkspaceSessionInfo {
                id: "p".into(),
                name: "P".into(),
                description: String::new(),
                directories: vec![],

                conversations: vec![],
                last_conversation: None,
            })
            .unwrap();
        store.set_active(Some("p".into()), None).unwrap();
        let log = observed.lock().unwrap().clone();
        assert_eq!(log, vec![Some("1 sessions".into()), Some("p".into())]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn upsert_validates_name_and_caps_directories() {
        let (store, path) = temp_store("validate");
        assert!(store
            .upsert(WorkspaceSessionInfo {
                id: String::new(),
                name: "   ".into(),
                description: String::new(),
                directories: vec![],

                conversations: vec![],
                last_conversation: None,
            })
            .is_err());
        let many: Vec<echo_protocol::WorkspaceDirectory> = (0..(MAX_DIRECTORIES + 5))
            .map(|i| echo_protocol::WorkspaceDirectory::local(format!("/tmp/dir-{i}")))
            .collect();
        assert!(store
            .upsert(WorkspaceSessionInfo {
                id: "big".into(),
                name: "Big".into(),
                description: String::new(),
                directories: many,

                conversations: vec![],
                last_conversation: None,
            })
            .is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn collects_git_status_of_the_repo_checkout() {
        // 本仓库 checkout 是真实 git 仓库：断言只读采集能识别它。
        let root = env!("CARGO_MANIFEST_DIR");
        let info = collect_dir_git(root);
        assert!(info.is_repo, "repo checkout should be detected: {info:?}");
        assert!(info.branch.is_some(), "branch should be read: {info:?}");
        assert!(info.error.is_none(), "no error expected: {info:?}");
    }

    // ── 文件浏览器（collect_dir_files / resolve_within_directories）──

    #[test]
    fn collect_dir_files_sorts_dirs_first_and_skips_hidden() {
        let root = std::env::temp_dir().join(format!("echo-files-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".hidden-dir")).unwrap();
        std::fs::write(root.join("b.txt"), "bb").unwrap();
        std::fs::write(root.join("A.txt"), "a").unwrap();
        std::fs::write(root.join(".hidden-file"), "x").unwrap();

        let entries = collect_dir_files(&root).expect("list");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        // 目录在前（字母序），文件在后（不区分大小写字母序）；隐藏项跳过。
        assert_eq!(names, vec!["src", "A.txt", "b.txt"], "got: {names:?}");
        assert!(entries[0].is_dir);
        assert!(!entries[1].is_dir);
        assert_eq!(entries[1].size, 1);
        assert!(entries[1].path.starts_with(root.to_str().unwrap()));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_within_directories_accepts_subtree_rejects_others() {
        let root = std::env::temp_dir().join(format!("echo-scope-test-{}", std::process::id()));
        let other = std::env::temp_dir().join(format!("echo-scope-other-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        let dirs = vec![echo_protocol::WorkspaceDirectory::local(
            root.to_string_lossy().to_string(),
        )];
        // 目录本身与其子孙放行。
        assert!(resolve_within_directories(&dirs, root.to_str().unwrap()).is_ok());
        assert!(resolve_within_directories(&dirs, root.join("sub").to_str().unwrap()).is_ok());
        // 会话目录之外拒绝；不存在路径拒绝。
        assert!(resolve_within_directories(&dirs, other.to_str().unwrap()).is_err());
        assert!(resolve_within_directories(&dirs, "/nonexistent/echo-scope").is_err());
        // 形似前缀（sibling 同名扩展）不得误放行。
        let sibling =
            std::env::temp_dir().join(format!("echo-scope-test-{}-sibling", std::process::id()));
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(resolve_within_directories(&dirs, sibling.to_str().unwrap()).is_err());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other);
        let _ = std::fs::remove_dir_all(&sibling);
    }

    #[test]
    fn browse_roots_include_home_and_workspace_dirs_deduped() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("/srv/proj".to_string(), "Proj".to_string());
        let dirs = vec![
            echo_protocol::WorkspaceDirectory::local("/srv/proj"),
            // 与上一条重复（去重）
            echo_protocol::WorkspaceDirectory::local("/srv/proj"),
            // 远程目录不进本机浏览根
            echo_protocol::WorkspaceDirectory::Qualified {
                path: "/srv/remote".into(),
                node: Some("gpu-box".into()),
            },
        ];
        let roots = browse_roots(&dirs, &labels);
        // HOME 在首位（测试环境必有 HOME）
        assert!(roots[0].path.starts_with('/'), "roots[0]: {:?}", roots[0]);
        assert!(
            roots
                .iter()
                .any(|r| r.label == "Proj" && r.path == "/srv/proj"),
            "roots: {roots:?}"
        );
        // 远程目录不在本机浏览根
        assert!(!roots.iter().any(|r| r.path.contains("node://")));
        // 去重：/srv/proj 只出现一次
        assert_eq!(roots.iter().filter(|r| r.path == "/srv/proj").count(), 1);
    }

    #[test]
    fn resolve_within_browse_roots_rejects_outside() {
        let tmp = std::env::temp_dir();
        let inside = tmp.join(format!("echo-browse-{}", std::process::id()));
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(inside.join("keep.txt"), "x").unwrap();
        let roots = vec![echo_protocol::BrowseRoot {
            label: "tmp".into(),
            path: tmp.to_string_lossy().into_owned(),
        }];
        // 根内：放行（canonical）
        let resolved =
            resolve_within_browse_roots(&roots, &inside.to_string_lossy()).expect("inside root");
        assert!(resolved.starts_with(tmp.canonicalize().unwrap()));
        // 根外（/etc 不在浏览根内）：拒绝
        if std::path::Path::new("/etc").exists() {
            let err = resolve_within_browse_roots(&roots, "/etc").expect_err("outside");
            assert!(err.contains("不在浏览根内"), "{err}");
        }
        std::fs::remove_file(inside.join("keep.txt")).ok();
        std::fs::remove_dir(&inside).ok();
    }

    #[test]
    fn collect_child_dirs_filters_files() {
        let dir = std::env::temp_dir().join(format!("echo-childdirs-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("file.txt"), "x").unwrap();
        let entries = collect_child_dirs(&dir).expect("list");
        assert_eq!(entries.len(), 1, "entries: {entries:?}");
        assert_eq!(entries[0].name, "sub");
        assert!(entries[0].is_dir);
        std::fs::remove_file(dir.join("file.txt")).ok();
        std::fs::remove_dir(dir.join("sub")).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn upsert_parses_node_qualified_directory_strings() {
        let (store, _path) = temp_store("node-dirs");
        let session = store
            .upsert(echo_protocol::WorkspaceSessionInfo {
                id: String::new(),
                name: "cross-machine".into(),
                description: String::new(),
                directories: vec![
                    echo_protocol::WorkspaceDirectory::from("/srv/local-repo"),
                    echo_protocol::WorkspaceDirectory::from("node://gpu-box/srv/remote-repo"),
                ],

                conversations: vec![],
                last_conversation: None,
            })
            .expect("upsert");
        assert_eq!(session.directories.len(), 2);
        let remote = &session.directories[1];
        assert!(remote.is_remote(), "node:// 字符串应解析为远程目录");
        assert_eq!(remote.node(), Some("gpu-box"));
        assert_eq!(remote.path(), "/srv/remote-repo");
        // 本机目录不受影响
        assert!(!session.directories[0].is_remote());
        // 持久化后重载仍保留远程归属
        let reloaded = crate::workspace::WorkspaceStore::load(Some(_path.clone()));
        let (sessions, _) = reloaded.snapshot();
        assert!(sessions[0].directories[1].is_remote());
    }

    /// P3-1：远程目录在提示词中标注 [remote:<peer>] 且说明 node:// 可操作。
    #[test]
    fn prompt_marks_remote_dirs_as_operable() {
        let (store, path) = temp_store("remote-prompt");
        let session = store
            .upsert(WorkspaceSessionInfo {
                id: "r".into(),
                name: "R".into(),
                description: String::new(),
                directories: vec![
                    echo_protocol::WorkspaceDirectory::local("/srv/local"),
                    echo_protocol::WorkspaceDirectory::Qualified {
                        path: "/srv/repo".into(),
                        node: Some("gpu-box".into()),
                    },
                ],

                conversations: vec![],
                last_conversation: None,
            })
            .unwrap();
        store.set_active(Some(session.id), None).unwrap();
        let text = store.prompt_text().expect("active prompt");
        assert!(text.contains("[remote:gpu-box]"), "text: {text}");
        assert!(text.contains("node://gpu-box/srv/repo"), "text: {text}");
        assert!(
            text.contains("node://gpu-box/<绝对路径>"),
            "操作说明: {text}"
        );
        // 本机目录不带 remote 标注。
        assert!(text.contains("- /srv/local\n"), "text: {text}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn non_repo_and_missing_dirs_report_cleanly() {
        let missing = collect_dir_git("/nonexistent/echo-workspace-test");
        assert!(!missing.is_repo);
        assert!(missing
            .error
            .as_deref()
            .unwrap_or("")
            .contains("目录不存在"));

        let tmp = std::env::temp_dir().join("echo-workspace-nonrepo");
        std::fs::create_dir_all(&tmp).ok();
        let non_repo = collect_dir_git(tmp.to_str().unwrap());
        assert!(!non_repo.is_repo);
        let error = non_repo.error.clone().unwrap_or_default();
        assert!(
            error.contains("不是 git 仓库") || error.contains("无法执行 git"),
            "unexpected: {error}"
        );
    }

    /// API 改名传播：全部对话的引用一次性改写、只写一次盘、只通知一次；
    /// 无引用的二次调用零变更零通知。
    #[test]
    fn rename_api_profile_rewrites_all_conversations_and_saves_once() {
        let (store, path) = temp_store("rename-api");
        store
            .upsert(WorkspaceSessionInfo {
                id: "ws".into(),
                name: "WS".into(),
                description: String::new(),
                directories: vec![],
                conversations: vec![],
                last_conversation: None,
            })
            .unwrap();
        {
            let mut doc = store.doc.write().unwrap();
            let session = doc.sessions.iter_mut().find(|s| s.id == "ws").unwrap();
            session.conversations = vec![
                WorkspaceConversationInfo {
                    id: "c1".into(),
                    name: "C1".into(),
                    legacy: false,
                    api_profile: Some("alpha".into()),
                },
                WorkspaceConversationInfo {
                    id: "c2".into(),
                    name: "C2".into(),
                    legacy: false,
                    api_profile: Some("beta".into()),
                },
                WorkspaceConversationInfo {
                    id: "c3".into(),
                    name: "C3".into(),
                    legacy: false,
                    api_profile: Some("alpha".into()),
                },
            ];
            store.save_locked(&doc).unwrap();
        }
        let notified = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let notified = notified.clone();
            store.set_on_change(std::sync::Arc::new(move || {
                notified.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));
        }

        let changed = store.rename_api_profile("alpha", Some("gamma")).unwrap();
        assert_eq!(changed, 2, "c1 + c3 rewritten");
        assert_eq!(
            notified.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one change notification"
        );

        let ws = store.get("ws").unwrap();
        assert_eq!(
            ws.conversations
                .iter()
                .filter(|c| c.api_profile.as_deref() == Some("gamma"))
                .count(),
            2
        );
        assert!(
            ws.conversations
                .iter()
                .any(|c| c.api_profile.as_deref() == Some("beta")),
            "unrelated reference untouched"
        );
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("\"gamma\""), "persisted: {on_disk}");
        assert!(!on_disk.contains("\"alpha\""), "old ref gone: {on_disk}");

        // 无引用：零变更、零通知。
        let again = store.rename_api_profile("alpha", Some("gamma")).unwrap();
        assert_eq!(again, 0);
        assert_eq!(notified.load(std::sync::atomic::Ordering::SeqCst), 1);
        std::fs::remove_file(&path).ok();
    }

    /// `to = None` 清空引用（删除语义预埋）：只清匹配条目。
    #[test]
    fn rename_api_profile_none_clears_refs() {
        let (store, path) = temp_store("rename-api-clear");
        store
            .upsert(WorkspaceSessionInfo {
                id: "ws".into(),
                name: "WS".into(),
                description: String::new(),
                directories: vec![],
                conversations: vec![],
                last_conversation: None,
            })
            .unwrap();
        {
            let mut doc = store.doc.write().unwrap();
            let session = doc.sessions.iter_mut().find(|s| s.id == "ws").unwrap();
            session.conversations = vec![
                WorkspaceConversationInfo {
                    id: "c1".into(),
                    name: "C1".into(),
                    legacy: false,
                    api_profile: Some("alpha".into()),
                },
                WorkspaceConversationInfo {
                    id: "c2".into(),
                    name: "C2".into(),
                    legacy: false,
                    api_profile: Some("beta".into()),
                },
            ];
            store.save_locked(&doc).unwrap();
        }

        let changed = store.rename_api_profile("alpha", None).unwrap();
        assert_eq!(changed, 1);
        let ws = store.get("ws").unwrap();
        assert!(ws.conversations[0].api_profile.is_none(), "cleared");
        assert_eq!(ws.conversations[1].api_profile.as_deref(), Some("beta"));
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("alpha"),
            "cleared refs not persisted: {on_disk}"
        );
        std::fs::remove_file(&path).ok();
    }
    /// 会话级 API 覆盖持久化（2026-10 用户报障修复）：存量工作区
    /// （conversations 为空、仅惰性补位 main）经裸通道设置后物化并落盘——
    /// 重载同路径可读回（此前面板选择 profile 过一会「失效」）。相同值
    /// 不写盘、不通知。
    #[test]
    fn set_api_profile_legacy_channel_materializes_and_persists() {
        let (store, path) = temp_store("api-legacy");
        store
            .upsert(WorkspaceSessionInfo {
                id: "proj".into(),
                name: "Proj".into(),
                ..Default::default()
            })
            .unwrap();
        // 存量工作区：conversations 为空（惰性补位，不落盘）。
        assert!(store.get("proj").unwrap().conversations.is_empty());

        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let hits = hits.clone();
            store.set_on_change(std::sync::Arc::new(move || {
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));
        }

        // 命令路径的解析回退：裸通道 → (ws, None) → legacy 定位。
        let (ws_id, conv) = parse_workspace_channel("local:workspace:proj:local_user").unwrap();
        assert_eq!(ws_id, "proj");
        assert!(conv.is_none());
        assert!(store
            .set_legacy_conversation_api_profile(&ws_id, Some("alpha"))
            .unwrap());
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "一次变更一次通知"
        );

        // 物化 + 覆盖就位。
        let convs = store.conversations_of("proj");
        assert_eq!(convs.len(), 1);
        assert!(convs[0].legacy);
        assert_eq!(convs[0].api_profile.as_deref(), Some("alpha"));

        // 相同值：Ok(true) 且不写盘、不通知。
        assert!(store
            .set_legacy_conversation_api_profile("proj", Some("alpha"))
            .unwrap());
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "无变化不通知"
        );

        // 持久化往返：重载同路径可读回（重启后恢复）。
        let reloaded = WorkspaceStore::load(Some(path.clone()));
        let ws = reloaded.get("proj").unwrap();
        assert_eq!(ws.conversations.len(), 1);
        assert!(ws.conversations[0].legacy);
        assert_eq!(ws.conversations[0].api_profile.as_deref(), Some("alpha"));

        // 清除（None）：落盘为空覆盖。
        assert!(store
            .set_legacy_conversation_api_profile("proj", None)
            .unwrap());
        let reloaded = WorkspaceStore::load(Some(path.clone()));
        assert!(reloaded.get("proj").unwrap().conversations[0]
            .api_profile
            .is_none());
        std::fs::remove_file(&path).ok();
    }

    /// 多对话对话级覆盖：显式 conv 设置/清除、空串 = 清除；不存在的
    /// 工作区/对话 → Ok(false)。
    #[test]
    fn set_api_profile_explicit_conversation_set_clear_and_missing() {
        let (store, path) = temp_store("api-conv");
        store
            .upsert(WorkspaceSessionInfo {
                id: "proj".into(),
                name: "Proj".into(),
                ..Default::default()
            })
            .unwrap();
        let docs = store
            .upsert_conversation(
                "proj",
                WorkspaceConversationInfo {
                    id: String::new(),
                    name: "docs".into(),
                    legacy: false,
                    api_profile: None,
                },
            )
            .unwrap();
        let session_id = format!("local:workspace:proj/{}:local_user", docs.id);
        let (ws_id, conv) = parse_workspace_channel(&session_id).unwrap();
        assert_eq!(ws_id, "proj");
        assert_eq!(conv.as_deref(), Some(docs.id.as_str()));

        assert!(store
            .set_conversation_api_profile(&ws_id, conv.as_deref().unwrap(), Some("beta"))
            .unwrap());
        let saved = store
            .get("proj")
            .unwrap()
            .conversations
            .into_iter()
            .find(|c| c.id == docs.id)
            .unwrap();
        assert_eq!(saved.api_profile.as_deref(), Some("beta"));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"beta\""), "persisted: {raw}");

        // 空串 = None（清除），并持久化。
        assert!(store
            .set_conversation_api_profile("proj", &docs.id, Some(""))
            .unwrap());
        let reloaded = WorkspaceStore::load(Some(path.clone()));
        let saved = reloaded
            .get("proj")
            .unwrap()
            .conversations
            .into_iter()
            .find(|c| c.id == docs.id)
            .unwrap();
        assert!(saved.api_profile.is_none());

        // 不存在的对话 / 工作区 → Ok(false)，不 panic。
        assert!(!store
            .set_conversation_api_profile("proj", "nope", Some("beta"))
            .unwrap());
        assert!(!store
            .set_conversation_api_profile("ghost", "docs", Some("beta"))
            .unwrap());
        assert!(!store
            .set_legacy_conversation_api_profile("ghost", Some("beta"))
            .unwrap());
        std::fs::remove_file(&path).ok();
    }

    /// 通道 id 解析：legacy / 多对话 / 非工作区（含 `@account` 后缀）。
    #[test]
    fn parse_workspace_channel_covers_legacy_multi_and_non_workspace() {
        assert_eq!(
            parse_workspace_channel("local:workspace:ws1:local_user"),
            Some(("ws1".to_string(), None))
        );
        assert_eq!(
            parse_workspace_channel("local:workspace:ws1/docs:local_user"),
            Some(("ws1".to_string(), Some("docs".to_string())))
        );
        // 多实例维度：user 段带 `@<account>` 后缀。
        assert_eq!(
            parse_workspace_channel("local:workspace:ws1/docs:local_user@work"),
            Some(("ws1".to_string(), Some("docs".to_string())))
        );
        // 非工作区：前缀不匹配 / 其它平台通道 / 空通道。
        assert_eq!(parse_workspace_channel("local:tui::local_user"), None);
        assert_eq!(parse_workspace_channel("qq:group:12345:67890"), None);
        assert_eq!(parse_workspace_channel("local:workspace:"), None);
    }
}
