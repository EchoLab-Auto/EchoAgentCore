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

use echo_protocol::{WorkspaceGitInfo, WorkspaceSessionInfo};

use crate::tool::{Tool, ToolError};

/// Cap on directories per session (protects the prompt/tool output size).
pub const MAX_DIRECTORIES: usize = 32;
/// Cap on reported changed-file paths per directory.
const CHANGED_FILES_CAP: usize = 30;
/// Bound on every single `git` invocation.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The persisted document: sessions + the single active marker.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct WorkspaceDocument {
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    sessions: Vec<WorkspaceSessionInfo>,
}

/// Per-persona workspace store (shared between command handlers, the tool and
/// the system-prompt builder). All mutations persist atomically.
#[derive(Debug, Default)]
pub struct WorkspaceStore {
    doc: RwLock<WorkspaceDocument>,
    persist: Option<PathBuf>,
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
        // 规范化目录：去首尾空白/尾随斜杠；重复项去重。允许暂时不存在的
        // 目录（项目可能还没克隆），存在性由 git 采集时呈现。
        let mut dirs: Vec<String> = Vec::new();
        for raw in &session.directories {
            let normalized = normalize_directory(raw);
            if normalized.is_empty() || dirs.iter().any(|d| d == &normalized) {
                continue;
            }
            dirs.push(normalized);
        }
        if dirs.len() > MAX_DIRECTORIES {
            return Err(format!("工作区目录最多 {MAX_DIRECTORIES} 个"));
        }
        session.directories = dirs;

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
            Some(existing) => *existing = session.clone(),
            None => doc.sessions.push(session.clone()),
        }
        let result = session;
        self.save_locked(&doc)?;
        Ok(result)
    }

    /// Delete a session; clears the active marker when it pointed at it.
    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let mut doc = self.doc.write().unwrap_or_else(|e| e.into_inner());
        let before = doc.sessions.len();
        doc.sessions.retain(|s| s.id != id);
        if doc.sessions.len() == before {
            return Ok(false);
        }
        if doc.active.as_deref() == Some(id) {
            doc.active = None;
        }
        self.save_locked(&doc)?;
        Ok(true)
    }

    /// Activate a session (`None` clears the marker). The id must exist.
    pub fn set_active(&self, id: Option<String>) -> Result<(), String> {
        let mut doc = self.doc.write().unwrap_or_else(|e| e.into_inner());
        if let Some(ref id) = id {
            if !doc.sessions.iter().any(|s| &s.id == id) {
                return Err(format!("工作区会话 {id} 不存在"));
            }
        }
        doc.active = id;
        self.save_locked(&doc)
    }

    /// System-prompt text for the active session (`None` = nothing to inject).
    pub fn prompt_text(&self) -> Option<String> {
        let session = self.active()?;
        if session.directories.is_empty() && session.name.is_empty() {
            return None;
        }
        let mut text = format!("# 当前工作区会话\n{}", session.name);
        if !session.description.is_empty() {
            text.push_str(&format!("（{}）", session.description));
        }
        if !session.directories.is_empty() {
            text.push_str("\n工作区目录：\n");
            for dir in &session.directories {
                text.push_str(&format!("- {dir}\n"));
            }
            text.push_str("在这些目录范围内工作；git 状态与其它会话可用 workspace 工具查询/切换。");
        } else {
            text.push_str("\n（尚未配置工作区目录，可用 workspace 工具或面板添加）");
        }
        Some(text)
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
        "工作区会话管理。操作: list 列出全部会话与激活状态, status 查看某会话各目录的 git 状态(默认激活会话), use 激活会话(需要 id), create 新建会话(需要 name，可选 directories), delete 删除会话(需要 id)。目录为绝对路径。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["list", "status", "use", "create", "delete"],
                    "description": "要执行的操作"
                },
                "id": {
                    "type": "string",
                    "description": "会话 id（status/use/delete 使用；status 缺省 = 激活会话）"
                },
                "name": {
                    "type": "string",
                    "description": "会话名称（operation=create 时必填）"
                },
                "directories": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "工作区目录绝对路径列表（operation=create 时可选）"
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
                let directories = session.directories.clone();
                let results = tokio::task::spawn_blocking(move || {
                    directories
                        .iter()
                        .map(|dir| collect_dir_git(dir))
                        .collect::<Vec<_>>()
                })
                .await
                .map_err(|e| ToolError::Execution(format!("git 采集失败: {e}")))?;
                Ok(serde_json::to_string_pretty(&json!({
                    "session": session.id,
                    "name": session.name,
                    "directories": results,
                }))
                .unwrap_or_else(|_| "{}".into()))
            }
            "use" => {
                let id = arguments
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| ToolError::InvalidArguments("use 需要 id".into()))?;
                self.store
                    .set_active(Some(id.to_string()))
                    .map_err(ToolError::Execution)?;
                Ok(format!("已激活工作区会话 {id}"))
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
                            .filter_map(|v| v.as_str().map(str::to_string))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (std::sync::Arc<WorkspaceStore>, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "echo-workspace-test-{}-{name}.json",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
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
            })
            .expect("upsert");
        assert_eq!(session.id, "my-project");
        assert_eq!(session.description, "desc");
        // 归一化 + 去重
        assert_eq!(session.directories, vec!["/tmp/proj"]);

        // 重新加载（模拟重启）
        let reloaded = WorkspaceStore::load(Some(path.clone()));
        let (sessions, active) = reloaded.snapshot();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "my-project");
        assert!(active.is_none());

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
            })
            .unwrap();
        assert_eq!(a.id, "ws"); // CJK-only name → fallback slug
        let b = store
            .upsert(WorkspaceSessionInfo {
                id: String::new(),
                name: "项目 2".into(),
                description: String::new(),
                directories: vec![],
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
            })
            .unwrap();
        assert!(store.prompt_text().is_none(), "未激活不进提示词");

        store.set_active(Some(session.id.clone())).unwrap();
        let text = store.prompt_text().expect("active prompt");
        assert!(text.contains("Proj"));
        assert!(text.contains("/srv/a"));
        assert!(text.contains("/srv/b"));

        assert!(store.set_active(Some("nope".into())).is_err());
        assert!(store.delete("proj").unwrap());
        assert!(store.active().is_none(), "删除激活会话后清除激活标记");
        assert!(store.prompt_text().is_none());
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
            })
            .is_err());
        let many: Vec<String> = (0..(MAX_DIRECTORIES + 5))
            .map(|i| format!("/tmp/dir-{i}"))
            .collect();
        assert!(store
            .upsert(WorkspaceSessionInfo {
                id: "big".into(),
                name: "Big".into(),
                description: String::new(),
                directories: many,
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
}
