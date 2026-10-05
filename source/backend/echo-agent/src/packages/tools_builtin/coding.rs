//! Coding tools — filesystem access for the agent.
//! read_file, list_files, search_code, write_file.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::tool::{Tool, ToolError, ToolRegistry};

/// Canonicalised workspace root for containment checks.
///
/// The workspace may contain symlinks (e.g. `/home/link` → `/real/project`);
/// comparing against the canonical form avoids false denials.
fn workspace_root(workspace: &std::path::Path) -> PathBuf {
    workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
}

/// 解析工具 `path` 参数：**绝对路径原样使用**（显式意图），相对路径相对工作区。
///
/// 约定对全部文件工具一致（2026-09-24 起 write/edit 与 read 对齐）：多仓库
/// 工作流里 agent 的工作区只覆盖一个仓库，显式绝对路径允许工作区外的读写
/// （如 agent 工作在 Core 仓库、同时要改 Panel 仓库）；相对路径必须落在
/// 工作区内——穿越防护见 [`guard_relative_path`]。
fn resolve_tool_path(workspace: &std::path::Path, raw: &str) -> PathBuf {
    if raw.starts_with('/') {
        PathBuf::from(raw)
    } else {
        workspace.join(raw)
    }
}

/// 相对路径的穿越防护：路径（或最近的已存在祖先）canonicalize 后必须落在
/// 工作区内，否则拒绝。**绝对路径不调用本函数**（显式意图，见
/// [`resolve_tool_path`]）。
///
/// 在创建目录**之前**调用：`../..` 逃逸不会在区外留下空目录；同时覆盖
/// 「既有符号链接指向区外」的情况（目标自身或最近祖先解析到区外即拒绝）。
fn guard_relative_path(
    workspace: &std::path::Path,
    path: &std::path::Path,
) -> Result<(), ToolError> {
    let root = workspace_root(workspace);
    // 目标已存在（含符号链接）：其自身 canonical 必须在工作区内。
    if let Ok(canonical) = path.canonicalize() {
        if !canonical.starts_with(&root) {
            return Err(ToolError::Execution(
                "access denied: path outside workspace".into(),
            ));
        }
        return Ok(());
    }
    // 目标尚不存在：以最近的已存在祖先为准（创建前校验）。
    let mut probe = path.parent();
    while let Some(candidate) = probe {
        if candidate.exists() {
            let canonical = candidate
                .canonicalize()
                .map_err(|e| ToolError::Execution(format!("cannot resolve path: {e}")))?;
            if !canonical.starts_with(&root) {
                return Err(ToolError::Execution(
                    "access denied: path outside workspace".into(),
                ));
            }
            return Ok(());
        }
        probe = candidate.parent();
    }
    Err(ToolError::Execution("cannot resolve path".into()))
}

// ── ReadFileTool ──

pub struct ReadFileTool {
    workspace: PathBuf,
}

impl ReadFileTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read the contents of a file in the project. Returns the file content with line numbers. Use this to examine source code, config files, or logs."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file, relative to project root or absolute"}
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path_str = args["path"].as_str().unwrap_or("");
        let path = resolve_tool_path(&self.workspace, path_str);

        // 相对路径的穿越防护；绝对路径是显式意图（与 write/edit 同一约定）。
        if !path_str.starts_with('/') {
            guard_relative_path(&self.workspace, &path)?;
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| ToolError::Execution(format!("file not found: {e}")))?;

        // 大小上限：先查 metadata，超大文件直接拒绝，避免一次性读入
        // 撑爆内存与 LLM 上下文。
        const MAX_READ_BYTES: u64 = 10 * 1024 * 1024; // 10MB
        let size = std::fs::metadata(&canonical)
            .map_err(|e| ToolError::Execution(format!("stat error: {e}")))?
            .len();
        if size > MAX_READ_BYTES {
            return Err(ToolError::Execution(format!(
                "文件过大（{} 字节，超过 10MB 上限），请用 search_code 或分段读取",
                size
            )));
        }

        let content = std::fs::read_to_string(&canonical)
            .map_err(|e| ToolError::Execution(format!("read error: {e}")))?;

        // 行数上限：超长内容截断并标注，防止 LLM 上下文爆炸。
        const MAX_LINES: usize = 100_000;
        let total_lines = content.lines().count();
        let truncated = total_lines > MAX_LINES;
        let mut lines: Vec<String> = content
            .lines()
            .take(MAX_LINES)
            .enumerate()
            .map(|(i, line)| format!("{:>5} │ {}", i + 1, line))
            .collect();
        if truncated {
            lines.push(format!(
                "… [内容已截断：共 {} 行，仅显示前 {} 行，请用 search_code 定位或分段读取]",
                total_lines, MAX_LINES
            ));
        }

        let summary = format!(
            "{} ({}) {} lines{}",
            path_str,
            canonical.display(),
            total_lines,
            if truncated { " (truncated)" } else { "" }
        );
        Ok(format!("{summary}\n{}", lines.join("\n")))
    }
}

// ── ListFilesTool ──

pub struct ListFilesTool {
    workspace: PathBuf,
}

impl ListFilesTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }
    fn description(&self) -> &str {
        "List files in a directory. Shows file names, sizes, and types. Use this to explore the project structure."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory path, relative to project root. Default: root."}
            }
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let rel = args["path"].as_str().unwrap_or(".");
        let dir = resolve_tool_path(&self.workspace, rel);
        // 相对路径穿越防护（与 read/write/edit 同一约定；绝对路径是显式
        // 意图放行）。此前 list/search 漏了这一步——`../..` 能列出区外目录。
        if !rel.starts_with('/') {
            guard_relative_path(&self.workspace, &dir)?;
        }
        if !dir.is_dir() {
            return Err(ToolError::Execution(format!("not a directory: {rel}")));
        }

        let mut entries: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let ft = e
                    .file_type()
                    .map(|t| {
                        if t.is_dir() {
                            "/"
                        } else if t.is_symlink() {
                            "@"
                        } else {
                            ""
                        }
                    })
                    .unwrap_or("");
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                let size_str = if ft == "/" {
                    String::new()
                } else {
                    format!("{:>8}", human_size(size))
                };
                entries.push(format!("  {}{}  {}", size_str, ft, name));
            }
        }
        entries.sort();
        Ok(format!(
            "{} ({} entries):\n{}",
            dir.display(),
            entries.len(),
            entries.join("\n")
        ))
    }
}

// ── SearchCodeTool ──

pub struct SearchCodeTool {
    workspace: PathBuf,
}

impl SearchCodeTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for SearchCodeTool {
    fn name(&self) -> &str {
        "search_code"
    }
    fn description(&self) -> &str {
        "Search for a pattern in source files. Returns matching file paths and line numbers. Use this to find function definitions, imports, or specific code patterns."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Text or regex pattern to search for"},
                "path": {"type": "string", "description": "Directory to search in (default: source/)", "default": "source"}
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let pattern = args["pattern"].as_str().unwrap_or("");
        if pattern.is_empty() {
            return Err(ToolError::InvalidArguments("pattern required".into()));
        }
        let dir_rel = args["path"].as_str().unwrap_or("source");
        let dir = resolve_tool_path(&self.workspace, dir_rel);
        // 同 list_files：相对路径穿越防护（绝对路径按显式意图放行）。
        if !dir_rel.starts_with('/') {
            guard_relative_path(&self.workspace, &dir)?;
        }
        if !dir.exists() {
            return Err(ToolError::Execution(format!(
                "directory not found: {dir_rel}"
            )));
        }

        let mut results: Vec<String> = Vec::new();
        let pattern_lower = pattern.to_lowercase();
        for entry in walkdir::WalkDir::new(&dir)
            .max_depth(10)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !matches!(
                ext,
                "rs" | "toml" | "md" | "json" | "sh" | "py" | "yaml" | "yml"
            ) {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(path) {
                for (i, line) in content.lines().enumerate() {
                    if line.to_lowercase().contains(&pattern_lower) {
                        let rel = path.strip_prefix(&self.workspace).unwrap_or(path);
                        results.push(format!("{}:{}: {}", rel.display(), i + 1, line.trim()));
                        if results.len() >= 50 {
                            break;
                        }
                    }
                }
            }
            if results.len() >= 50 {
                break;
            }
        }
        if results.is_empty() {
            Ok(format!("No matches found for '{}' in {}", pattern, dir_rel))
        } else {
            results.truncate(50);
            Ok(format!(
                "{} matches for '{}':\n{}",
                results.len(),
                pattern,
                results.join("\n")
            ))
        }
    }
}

// ── WriteFileTool ──

pub struct WriteFileTool {
    workspace: PathBuf,
}

impl WriteFileTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Create a new file or overwrite an existing file with new content. Use this to create or update source files, config files, or documentation."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file, relative to project root or absolute"},
                "content": {"type": "string", "description": "The full new content of the file"}
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path_str = args["path"].as_str().unwrap_or("");
        if path_str.is_empty() {
            return Err(ToolError::InvalidArguments("path required".into()));
        }
        let content = args["content"].as_str().unwrap_or("");
        let path = resolve_tool_path(&self.workspace, path_str);

        // 相对路径的穿越防护（创建目录**之前**，不在区外留空目录）；
        // 绝对路径是显式意图（多仓库工作流），不设限。
        if !path_str.starts_with('/') {
            guard_relative_path(&self.workspace, &path)?;
        }

        // Create parent directories if needed.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::Execution(format!("mkdir: {e}")))?;
        }
        std::fs::write(&path, content).map_err(|e| ToolError::Execution(format!("write: {e}")))?;

        let lines = content.lines().count();
        Ok(format!("wrote {} ({} lines)", path_str, lines))
    }
}

// ── EditFileTool ──

pub struct EditFileTool {
    workspace: PathBuf,
}

impl EditFileTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Replace lines in an existing file. Specify start_line and end_line (inclusive) to replace with new_content. Use this to make targeted edits without rewriting the entire file."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file, relative to project root or absolute"},
                "start_line": {"type": "integer", "description": "First line to replace (1-indexed)"},
                "end_line": {"type": "integer", "description": "Last line to replace (1-indexed, inclusive)"},
                "new_content": {"type": "string", "description": "The replacement text (can be multiple lines)"}
            },
            "required": ["path", "start_line", "end_line", "new_content"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path_str = args["path"].as_str().unwrap_or("");
        if path_str.is_empty() {
            return Err(ToolError::InvalidArguments("path required".into()));
        }
        let path = resolve_tool_path(&self.workspace, path_str);
        let start = args["start_line"].as_u64().unwrap_or(0) as usize;
        let end = args["end_line"].as_u64().unwrap_or(0) as usize;
        let new_content = args["new_content"].as_str().unwrap_or("");

        if start == 0 || end == 0 || start > end {
            return Err(ToolError::InvalidArguments(
                "start_line and end_line required, start <= end".into(),
            ));
        }

        // 相对路径的穿越防护；绝对路径是显式意图（与 read_file 一致）。
        if !path_str.starts_with('/') {
            guard_relative_path(&self.workspace, &path)?;
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| ToolError::Execution(format!("file not found: {e}")))?;

        let original = std::fs::read_to_string(&canonical)
            .map_err(|e| ToolError::Execution(format!("read: {e}")))?;
        let lines: Vec<&str> = original.lines().collect();
        let total = lines.len();
        if start > total {
            return Err(ToolError::Execution(format!(
                "start_line {start} exceeds file length {total}"
            )));
        }
        let end = end.min(total);
        // Replace lines [start-1 .. end-1] with new_content.
        let mut new_lines: Vec<String> = lines[..start.saturating_sub(1)]
            .iter()
            .map(|l| l.to_string())
            .collect();
        new_lines.extend(new_content.lines().map(|l| l.to_string()));
        new_lines.extend(lines[end..total].iter().map(|l| l.to_string()));
        std::fs::write(&canonical, new_lines.join("\n") + "\n")
            .map_err(|e| ToolError::Execution(format!("write: {e}")))?;
        Ok(format!(
            "edited {}: replaced lines {}-{} ({} lines)",
            path_str, start, end, total
        ))
    }
}

// ── RemoteAwareTool（P3-1 跨机文件分流层）──

/// 远程调用的超时提示（与 federation::remote_tool::REMOTE_TIMEOUT 一致：
/// 本地工具守卫上限 300s × 1.5 网络余量，封顶 460s）。
const REMOTE_TIMEOUT_HINT: std::time::Duration = std::time::Duration::from_secs(460);

/// 解析 `node://<peer>/<绝对路径>` → `(peer, 绝对路径)`。
///
/// - 非 `node://` 前缀：`None`（本地路径，走原逻辑）；
/// - 前缀存在但形态非法（缺 `/`、空 peer 名）：`Some(Err(..))`；
/// - 余部一律规范为以 `/` 开头的绝对路径——对端执行端的联邦沙箱
///   （`InvokeRouter::handle_invoke` 的 path_within_roots 裁决）只接受
///   落在对端工作区根并集内的绝对路径。
fn parse_node_path(raw: &str) -> Option<Result<(String, String), String>> {
    let rest = raw.strip_prefix("node://")?;
    Some(match rest.split_once('/') {
        Some((peer, path)) if !peer.is_empty() => Ok((peer.to_string(), format!("/{path}"))),
        _ => Err("node:// 路径需形如 node://<peer>/<绝对路径>".into()),
    })
}

/// 远程路径分流层：包在 5 个文件工具外，对 agent 透明（同名同 schema）。
///
/// `path` 参数带 `node://<peer>/…` 前缀、或 `command` 参数带
/// `node://<peer>/<命令>` 前缀（bash）时，经联邦 Invoke 把调用转发到
/// 对端同名工具执行（出口见 [`crate::federation::remote_invoker`]，装配层
/// 注入；对端拒绝——白名单/路径沙箱——原样透传为 `ToolError`）；其余
/// 原样走本地逻辑。
pub struct RemoteAwareTool {
    inner: Arc<dyn Tool>,
    /// 装配期快照；`None` 时 execute 再回查全局出口（联邦后接线场景）。
    invoker: Option<crate::federation::RemoteInvoker>,
    /// inner 描述 + node:// 用法说明（清单生成）。
    description: String,
}

impl RemoteAwareTool {
    pub fn wrap(inner: Arc<dyn Tool>) -> Arc<dyn Tool> {
        let description = format!(
            "{} Path may carry a `node://<peer>/<绝对路径>` prefix to operate on a remote federated node's workspace files directly (executed on the peer inside its sandbox).",
            inner.description()
        );
        Arc::new(Self {
            inner,
            invoker: crate::federation::remote_invoker(),
            description,
        })
    }
}

#[async_trait]
impl Tool for RemoteAwareTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn category(&self) -> &'static str {
        self.inner.category()
    }
    fn parameters(&self) -> Value {
        self.inner.parameters()
    }
    fn timeout_hint(&self, arguments: &Value) -> Option<std::time::Duration> {
        let is_remote = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| p.starts_with("node://"))
            .unwrap_or(false)
            || arguments
                .get("command")
                .and_then(|v| v.as_str())
                .map(|c| c.starts_with("node://"))
                .unwrap_or(false);
        if is_remote {
            Some(REMOTE_TIMEOUT_HINT)
        } else {
            self.inner.timeout_hint(arguments)
        }
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        // 分流判定：文件工具看 `path` 参数；bash 看 `command` 前缀
        // （`node://<peer>/<命令>`——把整条命令发到对端执行）。
        let node_target: Option<Result<(String, String), String>> =
            if let Some(raw_path) = args.get("path").and_then(|v| v.as_str()) {
                parse_node_path(raw_path)
            } else if let Some(raw_cmd) = args.get("command").and_then(|v| v.as_str()) {
                // 命令语义：node://<peer>/<命令>——命令本身不以 / 开头
                // （与路径语义区分；`node://<peer>/` 前缀剥掉后去前导斜杠）。
                raw_cmd
                    .strip_prefix("node://")
                    .map(|rest| match rest.split_once('/') {
                        Some((peer, cmd)) if !peer.is_empty() && !cmd.is_empty() => {
                            Ok((peer.to_string(), cmd.to_string()))
                        }
                        _ => Err("node:// 命令需形如 node://<peer>/<命令>".into()),
                    })
            } else {
                None
            };
        let Some(parsed) = node_target else {
            return self.inner.execute(args).await;
        };
        let (peer, remote) = parsed.map_err(ToolError::InvalidArguments)?;
        let invoker = self
            .invoker
            .clone()
            .or_else(crate::federation::remote_invoker)
            .ok_or_else(|| {
                ToolError::Execution(
                    "联邦未接线：node:// 远程操作不可用（需联邦启用且 peer 在线）".into(),
                )
            })?;
        let mut remote_args = args;
        if remote_args.get("path").is_some() {
            remote_args["path"] = json!(remote);
        } else {
            remote_args["command"] = json!(remote);
        }
        invoker(peer, self.inner.name().to_string(), remote_args)
            .await
            .map_err(ToolError::Execution)
    }
}

// ── RunCommandTool ──

pub struct RunCommandTool {
    workspace: PathBuf,
}

impl RunCommandTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    /// 工具自声明的执行超时（与 execute 内部一致：默认 120s，上限 300s）。
    fn requested_timeout(args: &Value) -> std::time::Duration {
        std::time::Duration::from_secs(args["timeout_secs"].as_u64().unwrap_or(120).min(300))
    }
}

/// RAII：Drop 时向子进程所属的整个进程组发 SIGKILL。
///
/// `sh -c` 的孙进程与直接子进程同组（spawn 时 `process_group(0)`），
/// 因此超时或外层守卫 drop 掉执行 future 时，整组都能被清理，不会留下
/// 孤儿进程继续运行。正常完成后调用 [`ChildGroupGuard::disarm`] 解除。
struct ChildGroupGuard {
    pgid: Option<i32>,
}

impl ChildGroupGuard {
    fn new(child: &tokio::process::Child) -> Self {
        Self {
            pgid: child.id().map(|pid| pid as i32),
        }
    }

    #[cfg(unix)]
    fn kill_group(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: 负 pid 表示向进程组发信号；该组由我们通过
            // process_group(0) 创建，组内都是被托管的命令进程。
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }

    #[cfg(not(unix))]
    fn kill_group(&mut self) {
        // 非 Unix 平台依赖 Command::kill_on_drop 终止直接子进程。
        self.pgid = None;
    }

    fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for ChildGroupGuard {
    fn drop(&mut self) {
        self.kill_group();
    }
}

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell (bash) command in the project workspace. Returns stdout and stderr. Use for git operations, cargo builds, file operations, and other shell commands. Timeout: 120s (max 300s). Long-running polling loops should pass timeout_secs explicitly."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The shell command to run (e.g. 'cargo build', 'git status', 'ls -la')"},
                "timeout_secs": {"type": "integer", "description": "Timeout in seconds (default 120, max 300)"}
            },
            "required": ["command"]
        })
    }
    fn timeout_hint(&self, arguments: &Value) -> Option<std::time::Duration> {
        Some(Self::requested_timeout(arguments))
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let cmd = args["command"].as_str().unwrap_or("");
        if cmd.is_empty() {
            return Err(ToolError::InvalidArguments("command required".into()));
        }
        let timeout = Self::requested_timeout(&args);
        let timeout_secs = timeout.as_secs();

        super::reject_core_self_stop(cmd)?;

        // Block dangerous patterns.
        let lower = cmd.to_lowercase();
        for dangerous in &["rm -rf /", "mkfs.", "dd if=", ":(){ :|:& };:", "> /dev/sda"] {
            if lower.contains(dangerous) {
                return Err(ToolError::Execution(format!(
                    "blocked dangerous command pattern: {dangerous}"
                )));
            }
        }

        // sudo 需要交互式密码输入，bash 工具无法提供 tty；而提权执行
        // 已不再提供（run_sudo 工具于 2026-09 废弃移除）。检测行首 `sudo`
        // 并给出明确拒绝，避免模型反复尝试无 tty 的 sudo 命令。
        if let Some(rest) = cmd.trim_start().strip_prefix("sudo") {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return Err(ToolError::Execution(
                    "sudo 不受支持：bash 工具无交互 tty，且提权执行（run_sudo）已废弃移除。请改用无需 root 的替代方案"
                        .into(),
                ));
            }
        }

        // 独立进程组 + kill_on_drop：无论是内层超时还是外层守卫 drop 掉本
        // future，都能确保 `sh` 及其子进程被终止（进程组由 guard 兜底）。
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.workspace)
            // spawn + wait_with_output 需显式 pipe（.output() 原本隐式设置）。
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command
            .spawn()
            .map_err(|e| ToolError::Execution(format!("command failed: {e}")))?;
        let mut guard = ChildGroupGuard::new(&child);

        let output = tokio::time::timeout(timeout, child.wait_with_output()).await;

        match output {
            Ok(Ok(out)) => {
                guard.disarm();
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let mut result = format!("$ {cmd}\n");
                if !stdout.trim().is_empty() {
                    let truncated: String = stdout.chars().take(4096).collect();
                    result.push_str(&truncated);
                    if stdout.chars().count() > 4096 {
                        result.push_str("\n... (truncated)");
                    }
                }
                if !stderr.trim().is_empty() {
                    let truncated: String = stderr.chars().take(1024).collect();
                    result.push_str(&format!("\n[stderr]\n{truncated}"));
                }
                if out.status.success() {
                    Ok(result)
                } else {
                    Ok(format!(
                        "{result}\n[exit code: {}]",
                        out.status.code().unwrap_or(-1)
                    ))
                }
            }
            Ok(Err(e)) => {
                guard.disarm();
                Err(ToolError::Execution(format!("command failed: {e}")))
            }
            Err(_) => {
                guard.kill_group();
                Err(ToolError::Execution(format!(
                    "timeout after {timeout_secs}s"
                )))
            }
        }
    }
}

// ── Helpers ──

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    }
}

// ── Registration ──

pub fn register_coding_tools(registry: &mut ToolRegistry, workspace: PathBuf) {
    // P3-1：5 个文件工具包远程分流层（node:// 前缀 → 联邦 Invoke）；
    // bash 不包（远程 shell 走 <peer>:bash 代理工具）。
    registry.register(RemoteAwareTool::wrap(Arc::new(ReadFileTool::new(
        workspace.clone(),
    ))));
    registry.register(RemoteAwareTool::wrap(Arc::new(ListFilesTool::new(
        workspace.clone(),
    ))));
    registry.register(RemoteAwareTool::wrap(Arc::new(SearchCodeTool::new(
        workspace.clone(),
    ))));
    registry.register(RemoteAwareTool::wrap(Arc::new(WriteFileTool::new(
        workspace.clone(),
    ))));
    registry.register(RemoteAwareTool::wrap(Arc::new(EditFileTool::new(
        workspace.clone(),
    ))));
    registry.register(RemoteAwareTool::wrap(Arc::new(RunCommandTool::new(
        workspace,
    ))));
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("echo-coding-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bash_timeout_hint_mirrors_execute_parsing() {
        let tool = RunCommandTool::new(PathBuf::from("/tmp"));
        // 默认 120s
        assert_eq!(
            tool.timeout_hint(&json!({"command": "ls"})),
            Some(std::time::Duration::from_secs(120))
        );
        // 显式值
        assert_eq!(
            tool.timeout_hint(&json!({"command": "ls", "timeout_secs": 200})),
            Some(std::time::Duration::from_secs(200))
        );
        // 上限 300s
        assert_eq!(
            tool.timeout_hint(&json!({"command": "ls", "timeout_secs": 9999})),
            Some(std::time::Duration::from_secs(300))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_timeout_kills_the_whole_process_group() {
        let ws = temp_workspace("pgkill");
        let tool = RunCommandTool::new(ws);
        // 独特的 sleep 时长作为标记：`sh -c` 派生的孙进程（后台 sleep）与
        // 前台 sleep 同进程组；若只杀直接子进程，后台 sleep 会继续存活。
        let marker = 410000 + (std::process::id() % 1000) as u64;
        let result = tool
            .execute(json!({
                "command": format!("sleep {marker} & sleep {marker}"),
                "timeout_secs": 1
            }))
            .await;
        assert!(
            matches!(&result, Err(ToolError::Execution(e)) if e.contains("timeout")),
            "expected timeout, got {result:?}"
        );
        let leftover = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(format!("sleep {marker}"))
            .output()
            .expect("pgrep runs");
        assert!(
            !leftover.status.success(),
            "no process-group survivor expected: {}",
            String::from_utf8_lossy(&leftover.stdout)
        );
        let _ = std::fs::remove_dir_all(temp_workspace("pgkill"));
    }

    #[tokio::test]
    async fn write_file_creates_and_reads_back() {
        let ws = temp_workspace("write");
        let tool = WriteFileTool::new(ws.clone());
        let result = tool
            .execute(json!({"path": "sub/file.txt", "content": "hello\nworld"}))
            .await
            .unwrap();
        assert!(result.contains("wrote"));
        assert_eq!(
            std::fs::read_to_string(ws.join("sub/file.txt")).unwrap(),
            "hello\nworld"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn write_file_rejects_path_traversal() {
        let ws = temp_workspace("traversal");
        let tool = WriteFileTool::new(ws.clone());
        let err = tool
            .execute(json!({"path": "../../escape.txt", "content": "x"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("outside workspace"),
            "unexpected error: {err}"
        );
        assert!(
            !std::path::Path::new(
                &ws.parent()
                    .unwrap()
                    .join("escape.txt")
                    .to_string_lossy()
                    .to_string()
            )
            .exists(),
            "file must not be created outside the workspace"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn write_file_supports_absolute_paths() {
        // 显式绝对路径：允许工作区外（多仓库工作流——agent 工作区在 Core
        // 仓库、同时要改 Panel 仓库）。
        let ws = temp_workspace("abs-ws");
        let outside =
            std::env::temp_dir().join(format!("echo-coding-abs-out-{}", std::process::id()));
        let target = outside.join("nested/file.txt");
        let _ = std::fs::remove_dir_all(&outside);
        let tool = WriteFileTool::new(ws.clone());
        let result = tool
            .execute(json!({
                "path": target.to_string_lossy(),
                "content": "absolute\nwrite"
            }))
            .await
            .unwrap();
        assert!(result.contains("wrote"), "result: {result}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "absolute\nwrite",
            "file created at the absolute path, parent dirs included"
        );
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn edit_file_supports_absolute_paths() {
        let ws = temp_workspace("abs-edit-ws");
        let outside =
            std::env::temp_dir().join(format!("echo-coding-abs-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("f.txt");
        std::fs::write(&target, "one\ntwo\nthree\n").unwrap();

        let tool = EditFileTool::new(ws.clone());
        let result = tool
            .execute(json!({
                "path": target.to_string_lossy(),
                "start_line": 2,
                "end_line": 2,
                "new_content": "TWO"
            }))
            .await
            .unwrap();
        assert!(result.contains("edited"), "result: {result}");
        let content = std::fs::read_to_string(&target).unwrap();
        assert!(content.contains("TWO"));
        assert!(!content.contains("two\n"));
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// list_files / search_code 的相对路径穿越防护（2026-09-26 补齐；
    /// 此前四个文件操作里只有 read/write/edit 有 guard，list/search 漏了）。
    #[tokio::test]
    async fn list_and_search_reject_relative_traversal() {
        let ws = temp_workspace("list-search-trav");
        let sibling =
            std::env::temp_dir().join(format!("echo-coding-trav-neighbor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sibling);
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("secret.txt"), "outside").unwrap();

        let list = ListFilesTool::new(ws.clone());
        let err = list
            .execute(json!({"path": "../echo-coding-trav-neighbor-XXXX"}))
            .await
            .expect_err("relative traversal must be rejected");
        assert!(
            matches!(&err, ToolError::Execution(m) if m.contains("access denied")),
            "unexpected error: {err:?}"
        );

        let search = SearchCodeTool::new(ws.clone());
        let err = search
            .execute(json!({"pattern": "outside", "path": "../echo-coding-trav-neighbor-XXXX"}))
            .await
            .expect_err("relative traversal must be rejected");
        assert!(matches!(err, ToolError::Execution(_)), "got {err:?}");

        // 绝对路径仍是显式意图：放行（能找到区外目录，若存在）。
        let abs = sibling.to_string_lossy().into_owned();
        let ok = list.execute(json!({"path": abs})).await;
        assert!(ok.is_ok(), "explicit absolute path must pass: {ok:?}");

        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&sibling);
    }

    #[tokio::test]
    async fn write_file_traversal_leaves_no_dirs_outside() {
        // 穿越路径必须在**创建目录之前**被拦下：目标父目录（区外）不得出现。
        let ws = temp_workspace("trav-dirs");
        let escape_dir =
            std::env::temp_dir().join(format!("echo-coding-trav-escape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&escape_dir);
        let tool = WriteFileTool::new(ws.clone());
        let path = format!(
            "../{}/file.txt",
            escape_dir.file_name().unwrap().to_string_lossy()
        );
        let err = tool
            .execute(json!({"path": path, "content": "x"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("outside workspace"),
            "unexpected error: {err}"
        );
        assert!(
            !escape_dir.exists(),
            "escape parent dir must not be created outside the workspace"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_rejects_symlink_escape() {
        // 工作区内指向区外的符号链接：相对路径写它必须被拒（canonical 防线）。
        let ws = temp_workspace("symlink");
        let outside =
            std::env::temp_dir().join(format!("echo-coding-symlink-out-{}", std::process::id()));
        std::fs::write(&outside, "original").unwrap();
        std::os::unix::fs::symlink(&outside, ws.join("link.txt")).unwrap();

        let tool = WriteFileTool::new(ws.clone());
        let err = tool
            .execute(json!({"path": "link.txt", "content": "pwned"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("outside workspace"),
            "unexpected error: {err}"
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn edit_file_replaces_lines_in_range() {
        let ws = temp_workspace("edit");
        std::fs::write(ws.join("f.txt"), "one\ntwo\nthree\n").unwrap();
        let tool = EditFileTool::new(ws.clone());
        let result = tool
            .execute(json!({
                "path": "f.txt",
                "start_line": 2,
                "end_line": 2,
                "new_content": "TWO"
            }))
            .await
            .unwrap();
        assert!(result.contains("edited"));
        let content = std::fs::read_to_string(ws.join("f.txt")).unwrap();
        assert!(content.contains("TWO"));
        assert!(content.contains("one"));
        assert!(content.contains("three"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn edit_file_rejects_path_traversal() {
        let ws = temp_workspace("edit-trav");
        // An outside file that must stay untouched. One level up from the
        // workspace lands in the temp dir.
        let outside = std::env::temp_dir().join(format!("echo-outside-{}", std::process::id()));
        std::fs::write(&outside, "original").unwrap();
        let tool = EditFileTool::new(ws.clone());
        let rel_escape = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let err = tool
            .execute(json!({
                "path": rel_escape,
                "start_line": 1,
                "end_line": 1,
                "new_content": "pwned"
            }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("outside workspace"),
            "unexpected error: {err}"
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn read_file_supports_absolute_paths() {
        // 与 write/edit 同一约定：显式绝对路径允许工作区外（多仓库工作流）。
        let ws = temp_workspace("abs-read-ws");
        let outside =
            std::env::temp_dir().join(format!("echo-coding-abs-read-{}", std::process::id()));
        std::fs::write(&outside, "outside-content").unwrap();
        let tool = ReadFileTool::new(ws.clone());
        let result = tool
            .execute(json!({"path": outside.to_string_lossy()}))
            .await
            .unwrap();
        assert!(result.contains("outside-content"), "result: {result}");
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn read_file_rejects_traversal_within_workspace() {
        let ws = temp_workspace("read");
        // An existing file just outside the workspace.
        let outside =
            std::env::temp_dir().join(format!("echo-outside-read-{}", std::process::id()));
        std::fs::write(&outside, "secret").unwrap();
        let tool = ReadFileTool::new(ws.clone());
        let rel_escape = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let err = tool.execute(json!({"path": rel_escape})).await.unwrap_err();
        assert!(
            err.to_string().contains("outside workspace"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn bash_blocks_core_self_stop() {
        let ws = temp_workspace("cmd-self-stop");
        let tool = RunCommandTool::new(ws.clone());
        let err = tool
            .execute(json!({
                "command": "systemctl --user stop echo-agent-core.service && cp binary installed && systemctl --user start echo-agent-core.service"
            }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("blocked command"),
            "self-stop not blocked: {err}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn bash_blocks_dangerous_patterns() {
        let ws = temp_workspace("cmd");
        let tool = RunCommandTool::new(ws.clone());
        for dangerous in ["rm -rf /", "mkfs.ext4", "dd if=/dev/zero", ":(){ :|:& };:"] {
            let err = tool
                .execute(json!({"command": dangerous}))
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("blocked dangerous"),
                "command {dangerous:?} not blocked: {err}"
            );
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn bash_rejects_empty() {
        let ws = temp_workspace("cmd-empty");
        let tool = RunCommandTool::new(ws.clone());
        let err = tool.execute(json!({"command": ""})).await.unwrap_err();
        assert!(err.to_string().contains("command required"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn bash_executes_in_workspace() {
        let ws = temp_workspace("cmd-run");
        let tool = RunCommandTool::new(ws.clone());
        let result = tool.execute(json!({"command": "pwd"})).await.unwrap();
        assert!(
            result.contains(&ws.to_string_lossy().to_string()),
            "result: {result}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn search_code_finds_matches_recursively() {
        let ws = temp_workspace("search");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(
            ws.join("src/main.rs"),
            "fn main() {\n    let answer = 42;\n}\n",
        )
        .unwrap();
        std::fs::write(ws.join("README.md"), "# readme\nno match here\n").unwrap();
        // Unsupported extension must be skipped.
        std::fs::write(ws.join("data.bin"), "answer is here too").unwrap();

        let tool = SearchCodeTool::new(ws.clone());
        let out = tool
            .execute(json!({"pattern": "answer", "path": "."}))
            .await
            .unwrap();
        assert!(out.contains("main.rs:2"), "match with line number: {out}");
        assert!(!out.contains("README"), "non-matching file excluded");
        assert!(!out.contains("data.bin"), "unsupported extension skipped");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn search_code_is_case_insensitive() {
        let ws = temp_workspace("search-case");
        std::fs::write(ws.join("a.rs"), "let Value = 1;\n").unwrap();
        let tool = SearchCodeTool::new(ws.clone());
        let out = tool
            .execute(json!({"pattern": "value", "path": "."}))
            .await
            .unwrap();
        assert!(out.contains("a.rs:1"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn search_code_rejects_missing_pattern_and_dir() {
        let ws = temp_workspace("search-err");
        let tool = SearchCodeTool::new(ws.clone());
        let err = tool.execute(json!({"pattern": ""})).await.unwrap_err();
        assert!(err.to_string().contains("pattern required"));
        let err = tool
            .execute(json!({"pattern": "x", "path": "no-such-dir"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("directory not found"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn search_code_caps_at_50_results() {
        let ws = temp_workspace("search-cap");
        let mut content = String::new();
        for i in 0..200 {
            content.push_str(&format!("line {i} contains needle\n"));
        }
        std::fs::write(ws.join("big.rs"), content).unwrap();
        let tool = SearchCodeTool::new(ws.clone());
        let out = tool
            .execute(json!({"pattern": "needle", "path": "."}))
            .await
            .unwrap();
        // First line is the summary; the rest are capped result lines.
        let result_lines = out.lines().count().saturating_sub(1);
        assert!(
            result_lines <= 50,
            "result cap enforced: {result_lines} result lines"
        );
        assert!(out.starts_with("50 matches"), "summary: {out}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn remote_aware_bash_command_prefix_dispatches_to_invoker() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let invoker: crate::federation::RemoteInvoker = Arc::new(move |peer, tool, args| {
            calls2.fetch_add(1, Ordering::SeqCst);
            let peer = peer.to_string();
            let tool = tool.to_string();
            Box::pin(async move {
                assert_eq!(peer, "gpu-box");
                assert_eq!(tool, "bash");
                let cmd = args["command"].as_str().unwrap_or("").to_string();
                assert_eq!(cmd, "cargo build", "node:// 前缀应被剥离: {cmd}");
                Ok(format!("remote-ok:{cmd}"))
            })
        });
        let tool = RemoteAwareTool {
            inner: Arc::new(RunCommandTool::new(PathBuf::from("/tmp"))),
            invoker: Some(invoker),
            description: "t".into(),
        };
        let out = tool
            .execute(json!({"command": "node://gpu-box/cargo build"}))
            .await
            .expect("remote bash");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(out.contains("remote-ok:cargo build"), "{out}");

        // 本地命令不触发 invoker（走本机执行）。
        let out = tool
            .execute(json!({"command": "echo local-ok"}))
            .await
            .expect("local bash");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "local must not invoke");
        assert!(out.contains("local-ok"), "{out}");
    }

    // ── P3-1：远程路径分流层（RemoteAwareTool）──

    /// 同步闭包 → RemoteInvoker（测试用：结果立即就绪）。
    fn test_invoker(
        f: impl Fn(String, String, Value) -> Result<String, String> + Send + Sync + 'static,
    ) -> crate::federation::RemoteInvoker {
        std::sync::Arc::new(move |peer, tool, args| {
            let result = f(peer, tool, args);
            Box::pin(async move { result })
        })
    }

    fn wrapped(
        inner: Arc<dyn Tool>,
        invoker: Option<crate::federation::RemoteInvoker>,
    ) -> Arc<dyn Tool> {
        Arc::new(RemoteAwareTool {
            inner,
            invoker,
            description: String::new(),
        })
    }

    #[test]
    fn node_path_parsing() {
        // 本地路径：不分流。
        assert!(parse_node_path("/abs/local.rs").is_none());
        assert!(parse_node_path("relative/x.rs").is_none());
        // node://<peer>/<绝对路径>：解析出 peer 与绝对路径。
        let (peer, path) = parse_node_path("node://gpu-box/srv/repo/src/main.rs")
            .expect("node:// detected")
            .expect("valid form");
        assert_eq!(peer, "gpu-box");
        assert_eq!(path, "/srv/repo/src/main.rs");
        // 形态非法：缺路径段 / 空 peer 名。
        assert!(parse_node_path("node://gpu-box").unwrap().is_err());
        assert!(parse_node_path("node:///x").unwrap().is_err());
    }

    /// ① node://peer/x 路径解析与分流：转发对端同名工具，路径改写为绝对路径。
    #[tokio::test]
    async fn remote_node_path_routed_to_peer() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let tool = wrapped(
            Arc::new(ReadFileTool::new(PathBuf::from("/nonexistent-ws"))),
            Some(test_invoker(move |peer, tool, args| {
                seen2.lock().unwrap().push((peer, tool, args));
                Ok("remote-content".into())
            })),
        );
        let out = tool
            .execute(json!({"path": "node://gpu-box/srv/repo/a.rs"}))
            .await
            .unwrap();
        assert_eq!(out, "remote-content", "对端输出透明返回");
        let log = seen.lock().unwrap();
        assert_eq!(log.len(), 1, "恰好一次远程调用");
        assert_eq!(log[0].0, "gpu-box");
        assert_eq!(log[0].1, "read_file", "对端同名工具");
        assert_eq!(log[0].2["path"], json!("/srv/repo/a.rs"), "路径去前缀");
    }

    /// ② 对端拒绝（沙箱外路径）：Err 文本原样透传为 ToolError::Execution。
    #[tokio::test]
    async fn remote_rejection_passthrough_as_tool_error() {
        let tool = wrapped(
            Arc::new(WriteFileTool::new(PathBuf::from("/nonexistent-ws"))),
            Some(test_invoker(|_, _, _| {
                Err("absolute path outside workspace roots (federation sandbox)".into())
            })),
        );
        let err = tool
            .execute(json!({"path": "node://core-b/etc/shadow", "content": "x"}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Execution(m) if m.contains("outside workspace roots")),
            "拒绝原因透传: {err:?}"
        );
    }

    /// ③ 本地路径不受影响：不触发 invoker；未接线时 node:// 报明确错误。
    #[tokio::test]
    async fn local_paths_unaffected_by_remote_layer() {
        let ws = temp_workspace("remote-wrap");
        std::fs::write(ws.join("f.txt"), "local").unwrap();
        let tool = wrapped(
            Arc::new(ReadFileTool::new(ws.clone())),
            Some(test_invoker(|_, _, _| panic!("本地路径不得触发远程分流"))),
        );
        let out = tool.execute(json!({"path": "f.txt"})).await.unwrap();
        assert!(out.contains("local"), "result: {out}");
        // 绝对本地路径同样不触发。
        let abs = ws.join("f.txt").to_string_lossy().into_owned();
        assert!(tool.execute(json!({"path": abs})).await.is_ok());

        // 联邦未接线 + node:// → 明确错误（而非静默落到本地）。
        let bare = wrapped(Arc::new(ReadFileTool::new(ws.clone())), None);
        let err = bare
            .execute(json!({"path": "node://peer/x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("联邦未接线"), "unexpected: {err}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// 清单生成：包装后的工具同名，描述带 node:// 用法说明。
    #[test]
    fn remote_aware_tool_advertises_node_prefix() {
        let tool = RemoteAwareTool::wrap(Arc::new(ReadFileTool::new(PathBuf::from("/tmp"))));
        assert_eq!(tool.name(), "read_file");
        assert!(
            tool.description().contains("node://"),
            "description: {}",
            tool.description()
        );
    }
}
