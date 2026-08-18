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
        let path = if path_str.starts_with('/') {
            PathBuf::from(path_str)
        } else {
            self.workspace.join(path_str)
        };

        // Safety: prevent path traversal outside workspace.
        let canonical = path
            .canonicalize()
            .map_err(|e| ToolError::Execution(format!("file not found: {e}")))?;
        let root = workspace_root(&self.workspace);
        if !canonical.starts_with(&root) && !path_str.starts_with('/') {
            return Err(ToolError::Execution(
                "access denied: path outside workspace".into(),
            ));
        }

        let content = std::fs::read_to_string(&canonical)
            .map_err(|e| ToolError::Execution(format!("read error: {e}")))?;

        let lines: Vec<String> = content
            .lines()
            .enumerate()
            .map(|(i, line)| format!("{:>5} │ {}", i + 1, line))
            .collect();

        let summary = format!(
            "{} ({}) {} lines",
            path_str,
            canonical.display(),
            lines.len()
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
        let dir = if rel.starts_with('/') {
            PathBuf::from(rel)
        } else {
            self.workspace.join(rel)
        };
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
        let dir = if dir_rel.starts_with('/') {
            PathBuf::from(dir_rel)
        } else {
            self.workspace.join(dir_rel)
        };
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
                "path": {"type": "string", "description": "Path to the file, relative to project root"},
                "content": {"type": "string", "description": "The full new content of the file"}
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path_str = args["path"].as_str().unwrap_or("");
        if path_str.is_empty() || path_str.starts_with('/') {
            return Err(ToolError::InvalidArguments("relative path required".into()));
        }
        let content = args["content"].as_str().unwrap_or("");
        let path = self.workspace.join(path_str);

        // Create parent directories if needed (also makes them canonicalisable).
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::Execution(format!("mkdir: {e}")))?;
        }
        // Path-traversal guard: the resolved parent must stay inside the
        // workspace, so `../..` cannot escape it.
        let canonical_parent = path
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .ok_or_else(|| ToolError::Execution("cannot resolve path parent".into()))?;
        if !canonical_parent.starts_with(workspace_root(&self.workspace)) {
            return Err(ToolError::Execution(
                "access denied: path outside workspace".into(),
            ));
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
                "path": {"type": "string", "description": "Path to the file, relative to project root"},
                "start_line": {"type": "integer", "description": "First line to replace (1-indexed)"},
                "end_line": {"type": "integer", "description": "Last line to replace (1-indexed, inclusive)"},
                "new_content": {"type": "string", "description": "The replacement text (can be multiple lines)"}
            },
            "required": ["path", "start_line", "end_line", "new_content"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path_str = args["path"].as_str().unwrap_or("");
        if path_str.is_empty() || path_str.starts_with('/') {
            return Err(ToolError::InvalidArguments("relative path required".into()));
        }
        let path = self.workspace.join(path_str);
        let start = args["start_line"].as_u64().unwrap_or(0) as usize;
        let end = args["end_line"].as_u64().unwrap_or(0) as usize;
        let new_content = args["new_content"].as_str().unwrap_or("");

        if start == 0 || end == 0 || start > end {
            return Err(ToolError::InvalidArguments(
                "start_line and end_line required, start <= end".into(),
            ));
        }

        // Path-traversal guard: the resolved file must stay inside the
        // workspace, so `../..` cannot escape it.
        let canonical = path
            .canonicalize()
            .map_err(|e| ToolError::Execution(format!("file not found: {e}")))?;
        if !canonical.starts_with(workspace_root(&self.workspace)) {
            return Err(ToolError::Execution(
                "access denied: path outside workspace".into(),
            ));
        }

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

// ── RunCommandTool ──

pub struct RunCommandTool {
    workspace: PathBuf,
}

impl RunCommandTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }
    fn description(&self) -> &str {
        "Run a terminal command in the project workspace. Returns stdout and stderr. Use for git operations, cargo builds, file operations, and other shell commands. Timeout: 30s."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The shell command to run (e.g. 'cargo build', 'git status', 'ls -la')"},
                "timeout_secs": {"type": "integer", "description": "Timeout in seconds (default 30, max 120)"}
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let cmd = args["command"].as_str().unwrap_or("");
        if cmd.is_empty() {
            return Err(ToolError::InvalidArguments("command required".into()));
        }
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30).min(120);

        // Block dangerous patterns.
        let lower = cmd.to_lowercase();
        for dangerous in &["rm -rf /", "mkfs.", "dd if=", ":(){ :|:& };:", "> /dev/sda"] {
            if lower.contains(dangerous) {
                return Err(ToolError::Execution(format!(
                    "blocked dangerous command pattern: {dangerous}"
                )));
            }
        }

        // sudo needs an interactive password; the user must authorize it via
        // the run_sudo tool instead. Detect a leading `sudo` token so the
        // model learns the right tool rather than hitting a tty-less failure.
        if let Some(rest) = cmd.trim_start().strip_prefix("sudo") {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return Err(ToolError::Execution(
                    "sudo 需要交互授权：请改用 run_sudo 工具（用户会在 Panel 中输入密码，密码不会出现在上下文中）"
                        .into(),
                ));
            }
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(&self.workspace)
                .output(),
        )
        .await;

        match output {
            Ok(Ok(out)) => {
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
            Ok(Err(e)) => Err(ToolError::Execution(format!("command failed: {e}"))),
            Err(_) => Err(ToolError::Execution(format!(
                "timeout after {timeout_secs}s"
            ))),
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
    registry.register(Arc::new(ReadFileTool::new(workspace.clone())));
    registry.register(Arc::new(ListFilesTool::new(workspace.clone())));
    registry.register(Arc::new(SearchCodeTool::new(workspace.clone())));
    registry.register(Arc::new(WriteFileTool::new(workspace.clone())));
    registry.register(Arc::new(EditFileTool::new(workspace.clone())));
    registry.register(Arc::new(RunCommandTool::new(workspace)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp workspace dir that is cleaned up on drop.
    fn temp_workspace(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("echo-coding-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
    async fn write_file_rejects_absolute_paths() {
        let ws = temp_workspace("abs");
        let tool = WriteFileTool::new(ws.clone());
        let err = tool
            .execute(json!({"path": "/tmp/abs-escape.txt", "content": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("relative path required"));
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
    async fn run_command_blocks_dangerous_patterns() {
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
    async fn run_command_rejects_empty() {
        let ws = temp_workspace("cmd-empty");
        let tool = RunCommandTool::new(ws.clone());
        let err = tool.execute(json!({"command": ""})).await.unwrap_err();
        assert!(err.to_string().contains("command required"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn run_command_executes_in_workspace() {
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
}
