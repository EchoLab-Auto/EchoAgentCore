//! Coding tools — filesystem access for the agent.
//! read_file, list_files, search_code, write_file.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Tool, ToolError, ToolRegistry};

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
/// 「既有符号链接指向区外」与「悬空符号链接」两种情况。
///
/// 关键点：`Path::exists()` / `canonicalize()` 都会把悬空链接当成
/// 「不存在」，旧实现因此回退到父目录（父目录在工作区内）而放行，随后
/// `fs::write` 跟随链接把内容写到区外。这里改用 `symlink_metadata` 判断
/// 节点本身是否存在：悬空链接一律拒绝（目标不可解析，无法证明落在工作区）。
fn guard_relative_path(
    workspace: &std::path::Path,
    path: &std::path::Path,
) -> Result<(), ToolError> {
    let root = workspace_root(workspace);

    // 1) 目标自身（symlink_metadata 不跟随链接，能区分不存在与悬空链接）。
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            canonical_guard_target(&root, path, meta.file_type().is_symlink())?;
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(ToolError::Execution(format!("cannot stat path: {e}"))),
    }

    // 2) 目标尚不存在：最近的已存在祖先必须落在工作区内。逐级用
    //    symlink_metadata 探测——父目录若是悬空链接，`exists()` 会说
    //    false，于是旧实现会一路回到工作区根而误放行。
    let mut probe = path.parent();
    while let Some(candidate) = probe {
        match std::fs::symlink_metadata(candidate) {
            Ok(meta) => {
                canonical_guard_target(&root, candidate, meta.file_type().is_symlink())?;
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => probe = candidate.parent(),
            Err(e) => return Err(ToolError::Execution(format!("cannot stat path: {e}"))),
        }
    }
    Err(ToolError::Execution("cannot resolve path".into()))
}

/// 解析 `path` 的 canonical 形式并判定工作区归属。
///
/// `is_symlink` 来自 `symlink_metadata`：只有节点本身是链接时，
/// `canonicalize` 的 NotFound 才意味着「悬空链接」而不是「路径不存在」。
fn canonical_guard_target(
    root: &std::path::Path,
    path: &std::path::Path,
    is_symlink: bool,
) -> Result<PathBuf, ToolError> {
    match path.canonicalize() {
        Ok(canonical) if canonical.starts_with(root) => Ok(canonical),
        Ok(_) => Err(ToolError::Execution(
            "access denied: path outside workspace".into(),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && is_symlink => {
            Err(ToolError::Execution(format!(
                "access denied: dangling symlink in relative path ({} has no resolvable target)",
                path.display()
            )))
        }
        Err(e) => Err(ToolError::Execution(format!("cannot resolve path: {e}"))),
    }
}

/// 必需字符串参数：缺失或非字符串都立即失败。
///
/// 不能用 `args["x"].as_str().unwrap_or("")`：参数错误会被静默降级成空串，
/// 对 `write_file` 就是「把文件清空」。所有校验在任何文件系统操作之前完成。
fn required_str<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    match args.get(name) {
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(ToolError::InvalidArguments(format!(
            "{name} must be a string"
        ))),
        None => Err(ToolError::InvalidArguments(format!("{name} required"))),
    }
}

/// 可选无符号整数参数：缺失/null → None，其余非整数立即失败。
fn optional_u64(args: &Value, name: &str) -> Result<Option<u64>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number.as_u64().map(Some).ok_or_else(|| {
            ToolError::InvalidArguments(format!("{name} must be a non-negative integer"))
        }),
        Some(_) => Err(ToolError::InvalidArguments(format!(
            "{name} must be an integer"
        ))),
    }
}

/// read_file 单次返回的最大行数（防 LLM 上下文爆炸）。
const READ_MAX_LINES: usize = 100_000;
/// 整读上限；超过后只允许 `start_line`/`end_line` 流式分段。
const READ_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// 分段扫描的硬上限（避免误扫超大日志/磁盘镜像）。
const READ_MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;

/// 流式读取 `[start_line, end_line]`（1-indexed，inclusive），返回
/// `(总行数, 起始行, 结束行, 带行号的行)`。
///
/// 内存只与返回段大小相关（最多 [`READ_MAX_LINES`] 行），因此 >10MB 的
/// 文件也能分段读，不必退回 bash。`end_line` 未给时读到末尾；超过
/// `start + READ_MAX_LINES - 1` 的部分按上限截断（由调用方提示）。
fn read_file_range(
    path: &std::path::Path,
    start_line: Option<u64>,
    end_line: Option<u64>,
) -> Result<(usize, usize, usize, Vec<String>), ToolError> {
    use std::io::BufRead;

    let start = match start_line {
        Some(0) => {
            return Err(ToolError::InvalidArguments(
                "start_line must be >= 1".into(),
            ))
        }
        Some(value) => value as usize,
        None => 1,
    };
    if let Some(end) = end_line {
        if (end as usize) < start {
            return Err(ToolError::InvalidArguments(format!(
                "end_line ({end}) must be >= start_line ({start})"
            )));
        }
    }
    let end_limit = end_line
        .map(|value| value as usize)
        .unwrap_or(usize::MAX)
        .min(start.saturating_add(READ_MAX_LINES - 1));

    let file =
        std::fs::File::open(path).map_err(|e| ToolError::Execution(format!("read error: {e}")))?;
    let mut total = 0usize;
    let mut lines = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line.map_err(|e| ToolError::Execution(format!("read error: {e}")))?;
        total += 1;
        if total >= start && total <= end_limit {
            lines.push(format!("{:>5} │ {}", total, line));
        }
    }
    if total == 0 {
        if start != 1 {
            return Err(ToolError::Execution(format!(
                "start_line {start} exceeds file length 0"
            )));
        }
    } else if start > total {
        return Err(ToolError::Execution(format!(
            "start_line {start} exceeds file length {total}"
        )));
    }
    Ok((total, start, end_limit.min(total), lines))
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
    // ── 远程面声明（2026-11 S8）──
    /// 本工具是"无会话依赖的纯操作"，可被别的节点直接调用。
    fn remote_capable(&self) -> bool {
        true
    }
    /// 沙箱裁决的参数：文件系统路径（绝对路径必须落在执行端工作区根内）。
    fn path_arguments(&self) -> &'static [&'static str] {
        &["path"]
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file, relative to project root or absolute"},
                "start_line": {"type": "integer", "description": "Optional first line to read (1-indexed, inclusive). Omit to start at line 1"},
                "end_line": {"type": "integer", "description": "Optional last line to read (1-indexed, inclusive). Omit to read to the end (capped at 100000 lines)"}
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path_str = args["path"].as_str().unwrap_or("");
        let start_line = optional_u64(&args, "start_line")?;
        let end_line = optional_u64(&args, "end_line")?;
        let path = resolve_tool_path(&self.workspace, path_str);

        // 相对路径的穿越防护；绝对路径是显式意图（与 write/edit 同一约定）。
        if !path_str.starts_with('/') {
            guard_relative_path(&self.workspace, &path)?;
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| ToolError::Execution(format!("file not found: {e}")))?;

        let size = std::fs::metadata(&canonical)
            .map_err(|e| ToolError::Execution(format!("stat error: {e}")))?
            .len();
        let segmented = start_line.is_some() || end_line.is_some();
        if size > READ_MAX_SCAN_BYTES {
            return Err(ToolError::Execution(format!(
                "文件过大（{} 字节，超过 256MB 扫描上限），请用 search_code 定位或 bash 分段读取",
                size
            )));
        }
        if size > READ_MAX_BYTES && !segmented {
            return Err(ToolError::Execution(format!(
                "文件过大（{} 字节，超过 10MB 整读上限），请改用 start_line/end_line 分段读取或 search_code 定位",
                size
            )));
        }

        let (total_lines, start, end, mut lines) = if segmented {
            // 分段读：流式扫描，不把整个文件读进内存（>10MB 也可用）。
            read_file_range(&canonical, start_line, end_line)?
        } else {
            // 不传分段参数：行为与旧版一致（整读前 100000 行）。
            let content = std::fs::read_to_string(&canonical)
                .map_err(|e| ToolError::Execution(format!("read error: {e}")))?;
            let total = content.lines().count();
            let end = total.min(READ_MAX_LINES);
            let lines = content
                .lines()
                .take(READ_MAX_LINES)
                .enumerate()
                .map(|(index, line)| format!("{:>5} │ {}", index + 1, line))
                .collect();
            (total, 1usize, end, lines)
        };

        // 只有「因单次行数上限而少显示」才算截断；用户显式要求的
        // start_line/end_line 子区间不算。
        let truncated = if segmented {
            end < total_lines && end < end_line.map(|value| value as usize).unwrap_or(usize::MAX)
        } else {
            end < total_lines
        };
        if truncated {
            lines.push(format!(
                "… [内容已截断：共 {} 行，当前显示 {}-{} 行；可用 start_line/end_line 继续分段读取]",
                total_lines, start, end
            ));
        }

        let range = if segmented {
            if end >= start {
                format!(", showing {start}-{end}")
            } else {
                ", showing 0 lines".to_string()
            }
        } else {
            String::new()
        };
        let summary = format!(
            "{} ({}) {} lines{}{}",
            path_str,
            canonical.display(),
            total_lines,
            range,
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
    // ── 远程面声明（2026-11 S8）──
    /// 本工具是"无会话依赖的纯操作"，可被别的节点直接调用。
    fn remote_capable(&self) -> bool {
        true
    }
    /// 沙箱裁决的参数：文件系统路径（绝对路径必须落在执行端工作区根内）。
    fn path_arguments(&self) -> &'static [&'static str] {
        &["path"]
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
        "Search project source files for a pattern. Default mode is literal, case-insensitive text; set regex=true for a regular expression. Returns matching file paths and line numbers. Skips node_modules/target/.git/dist and files larger than 2MB."
    }
    // ── 远程面声明（2026-11 S8）──
    /// 本工具是"无会话依赖的纯操作"，可被别的节点直接调用。
    fn remote_capable(&self) -> bool {
        true
    }
    /// 沙箱裁决的参数：文件系统路径（绝对路径必须落在执行端工作区根内）。
    fn path_arguments(&self) -> &'static [&'static str] {
        &["path"]
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Literal text (default) or regular expression (regex=true) to search for. Literal search is case-insensitive"},
                "path": {"type": "string", "description": "Directory to search in (default: source/)", "default": "source"},
                "regex": {"type": "boolean", "description": "Treat pattern as a regular expression (default: false = literal). Regex mode is case-sensitive unless the pattern uses (?i)", "default": false}
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let pattern = args["pattern"].as_str().unwrap_or("");
        if pattern.is_empty() {
            return Err(ToolError::InvalidArguments("pattern required".into()));
        }
        if pattern.chars().count() > 4096 {
            return Err(ToolError::InvalidArguments(
                "pattern too long (max 4096 chars)".into(),
            ));
        }
        let use_regex = match args.get("regex") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "regex must be a boolean".into(),
                ))
            }
        };
        let dir_rel = match args.get("path") {
            None | Some(Value::Null) => "source",
            Some(Value::String(value)) => value.as_str(),
            Some(_) => return Err(ToolError::InvalidArguments("path must be a string".into())),
        };
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
        if !dir.is_dir() {
            return Err(ToolError::Execution(format!("not a directory: {dir_rel}")));
        }

        /// 默认字面量（沿用旧行为的大小写不敏感）；regex=true 才按正则解释。
        enum Matcher {
            Literal(String),
            Regex(regex::Regex),
        }
        impl Matcher {
            fn is_match(&self, line: &str) -> bool {
                match self {
                    Matcher::Literal(needle) => line.to_lowercase().contains(needle),
                    Matcher::Regex(regex) => regex.is_match(line),
                }
            }
        }
        let matcher = if use_regex {
            let regex = regex::RegexBuilder::new(pattern)
                .size_limit(2 * 1024 * 1024)
                .build()
                .map_err(|e| ToolError::InvalidArguments(format!("invalid regex: {e}")))?;
            Matcher::Regex(regex)
        } else {
            Matcher::Literal(pattern.to_lowercase())
        };

        const MAX_RESULTS: usize = 50;
        // 单文件上限 2MB：打包产物/生成代码不扫。
        const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
        // 只搜项目源码/配置类型（旧白名单缺 ts/tsx/js/jsx/vue）。
        const SOURCE_EXTENSIONS: &[&str] = &[
            "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "vue", "svelte", "py", "rb", "go",
            "java", "kt", "kts", "c", "h", "cc", "cpp", "cxx", "hpp", "cs", "php", "swift",
            "scala", "sh", "bash", "zsh", "sql", "html", "htm", "css", "scss", "sass", "less",
            "json", "jsonc", "toml", "yaml", "yml", "xml", "md", "markdown",
        ];
        // 构建产物/依赖目录不扫（`depth() == 0` 是用户显式指定的根，
        // 即使名字叫 target 也照搜——尊重显式 path）。
        fn is_ignored_dir(entry: &walkdir::DirEntry) -> bool {
            entry.depth() > 0
                && entry.file_type().is_dir()
                && matches!(
                    entry.file_name().to_str(),
                    Some(
                        "node_modules"
                            | "target"
                            | ".git"
                            | "dist"
                            | "build"
                            | ".next"
                            | ".venv"
                            | "__pycache__"
                            | "coverage"
                            | "vendor"
                    )
                )
        }

        let mut results: Vec<String> = Vec::new();
        let mut truncated = false;
        let mut skipped_large = 0usize;
        'walk: for entry in walkdir::WalkDir::new(&dir)
            .max_depth(10)
            .into_iter()
            .filter_entry(|entry| !is_ignored_dir(entry))
            .filter_map(|entry| entry.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let is_source = path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
                .unwrap_or(false);
            if !is_source {
                continue;
            }
            // 大文件（打包产物/生成代码）直接跳过；metadata 取不到也跳过。
            if entry
                .metadata()
                .map(|meta| meta.len() > MAX_FILE_BYTES)
                .unwrap_or(true)
            {
                skipped_large += 1;
                continue;
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            for (index, line) in content.lines().enumerate() {
                if !matcher.is_match(line) {
                    continue;
                }
                if results.len() >= MAX_RESULTS {
                    truncated = true;
                    break 'walk;
                }
                let rel = path.strip_prefix(&self.workspace).unwrap_or(path);
                results.push(format!("{}:{}: {}", rel.display(), index + 1, line.trim()));
            }
        }

        if results.is_empty() {
            let mut message = format!("No matches found for '{}' in {}", pattern, dir_rel);
            if skipped_large > 0 {
                message.push_str(&format!("\n（已跳过 {skipped_large} 个超过 2MB 的文件）"));
            }
            Ok(message)
        } else {
            let mut summary = format!("{} matches for '{}'", results.len(), pattern);
            if truncated {
                summary.push_str(&format!(
                    " (结果已截断：仅显示前 {MAX_RESULTS} 条，请缩小 path 或使用更精确的模式)"
                ));
            }
            if skipped_large > 0 {
                summary.push_str(&format!("（跳过 {skipped_large} 个超过 2MB 的文件）"));
            }
            Ok(format!("{summary}:\n{}", results.join("\n")))
        }
    }
}

/// 临时文件清理守卫：rename 成功前 Drop 掉临时文件，避免失败留下垃圾。
struct PendingFile(Option<PathBuf>);

impl PendingFile {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 原子上写：同目录临时文件 + `rename`，避免写到一半留下截断文件。
///
/// 语义与边界：
/// - 先完整写入临时文件并 `sync_all`，最后一步才 `rename`；任何中途失败
///   都不会动原文件（旧实现 `fs::write` 会先截断再写）。
/// - 目标是符号链接时写到其真实目标：`rename` 会把链接本身替换成普通文件，
///   必须避免丢掉链接语义；悬空链接无法解析目标，明确报错。
/// - 既有文件保留权限位（Unix mode）；新建文件沿用 umask 默认权限。
/// - 非普通文件（目录/FIFO 等）拒绝，避免 rename 把设备节点替换掉。
/// - 不保留硬链接关系与扩展属性（rename 换代 inode，与常见编辑器原子保存
///   一致）；父目录必须可写（原子提交的固有限制）。
fn write_file_atomic(path: &std::path::Path, content: &str) -> Result<(), ToolError> {
    let write_target = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => path.canonicalize().map_err(|e| {
            ToolError::Execution(format!("write: dangling symlink {}: {e}", path.display()))
        })?,
        Ok(_) => path.to_path_buf(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(ToolError::Execution(format!("stat: {e}"))),
    };

    // 既有目标：必须是普通文件，且当前用户确实有写权限（只读文件应像旧版
    // 一样失败；`rename` 本身只看父目录权限，不能代替这项检查）。
    let existing_permissions = match std::fs::metadata(&write_target) {
        Ok(meta) => {
            if !meta.is_file() {
                return Err(ToolError::Execution(format!(
                    "write: not a regular file: {}",
                    write_target.display()
                )));
            }
            std::fs::OpenOptions::new()
                .write(true)
                .open(&write_target)
                .map_err(|e| ToolError::Execution(format!("write: {e}")))?;
            Some(meta.permissions())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(ToolError::Execution(format!("stat: {e}"))),
    };

    let parent = write_target
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    // 同目录短名临时文件（不拼原文件名，避免接近 NAME_MAX 时超长）；
    // create_new 保证并发写不会共用同一个临时文件。
    let mut temp_path: Option<PathBuf> = None;
    let mut file: Option<std::fs::File> = None;
    for attempt in 0..100u32 {
        let candidate = parent.join(format!(
            ".echo-tmp-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // 既有文件先以 0600 打开，写完再恢复原权限，避免临时文件在
            // 写入过程中比目标更宽松。
            if existing_permissions.is_some() {
                options.mode(0o600);
            }
        }
        match options.open(&candidate) {
            Ok(opened) => {
                temp_path = Some(candidate);
                file = Some(opened);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(ToolError::Execution(format!(
                    "write: cannot create temp file in {}: {e}",
                    parent.display()
                )))
            }
        }
    }
    let (Some(temp_path), Some(mut file)) = (temp_path, file) else {
        return Err(ToolError::Execution(
            "write: cannot allocate a temp file".into(),
        ));
    };
    let mut cleanup = PendingFile(Some(temp_path.clone()));

    let prepared = (|| -> std::io::Result<()> {
        if let Some(permissions) = existing_permissions {
            file.set_permissions(permissions)?;
        }
        std::io::Write::write_all(&mut file, content.as_bytes())?;
        file.sync_all()
    })();
    drop(file);
    if let Err(e) = prepared {
        return Err(ToolError::Execution(format!("write: {e}")));
    }

    std::fs::rename(&temp_path, &write_target).map_err(|e| {
        ToolError::Execution(format!(
            "write: cannot commit {}: {e}",
            write_target.display()
        ))
    })?;
    cleanup.disarm();

    // rename 的持久化依赖父目录落盘；best effort，失败不影响已写入的内容。
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
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
    // ── 远程面声明（2026-11 S8）──
    /// 本工具是"无会话依赖的纯操作"，可被别的节点直接调用。
    fn remote_capable(&self) -> bool {
        true
    }
    /// 沙箱裁决的参数：文件系统路径（绝对路径必须落在执行端工作区根内）。
    fn path_arguments(&self) -> &'static [&'static str] {
        &["path"]
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
        let path_str = required_str(&args, "path")?;
        if path_str.is_empty() {
            return Err(ToolError::InvalidArguments("path required".into()));
        }
        // content 必须在**任何文件系统操作之前**解析：缺失/非字符串是参数
        // 错误，不能 `unwrap_or("")` 静默变成「清空文件」；合法空串保留。
        let content = required_str(&args, "content")?;
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
        // 创建目录后再校验一次：缩小「guard 之后、写入之前父目录被换成
        // 符号链接」的 TOCTOU 窗口（原子 rename 本身不会跟随目标链接）。
        if !path_str.starts_with('/') {
            guard_relative_path(&self.workspace, &path)?;
        }
        write_file_atomic(&path, content)?;

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
    // ── 远程面声明（2026-11 S8）──
    /// 本工具是"无会话依赖的纯操作"，可被别的节点直接调用。
    fn remote_capable(&self) -> bool {
        true
    }
    /// 沙箱裁决的参数：文件系统路径（绝对路径必须落在执行端工作区根内）。
    fn path_arguments(&self) -> &'static [&'static str] {
        &["path"]
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
        let path_str = required_str(&args, "path")?;
        if path_str.is_empty() {
            return Err(ToolError::InvalidArguments("path required".into()));
        }
        let start = optional_u64(&args, "start_line")?.unwrap_or(0) as usize;
        let end = optional_u64(&args, "end_line")?.unwrap_or(0) as usize;
        // new_content 与 write_file 的 content 同一口径：缺失/非字符串立即
        // 失败（旧实现会当成空串，等价于把目标行删掉）；合法空串仍表示
        // 「替换为空」。
        let new_content = required_str(&args, "new_content")?;

        if start == 0 || end == 0 || start > end {
            return Err(ToolError::InvalidArguments(
                "start_line and end_line required, start <= end".into(),
            ));
        }

        // 相对路径的穿越防护；绝对路径是显式意图（与 read_file 一致）。
        let path = resolve_tool_path(&self.workspace, path_str);
        if !path_str.starts_with('/') {
            guard_relative_path(&self.workspace, &path)?;
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| ToolError::Execution(format!("file not found: {e}")))?;
        // 相对路径再确认 canonical 结果仍在工作区内：guard 与 canonicalize
        // 之间若符号链接被替换，这里兜住（绝对路径仍是显式意图）。
        if !path_str.starts_with('/') && !canonical.starts_with(workspace_root(&self.workspace)) {
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
        write_file_atomic(&canonical, &(new_lines.join("\n") + "\n"))?;
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
        // 重复斜杠归一（`node://peer//srv` → `/srv`，2026-10）。
        Some((peer, path)) if !peer.is_empty() => Ok((
            peer.to_string(),
            format!("/{}", path.trim_start_matches('/')),
        )),
        _ => Err("node:// 路径需形如 node://<peer>/<绝对路径>".into()),
    })
}

/// 远程路径分流层：包在 5 个文件工具外，对 agent 透明（同名同 schema）。
///
/// `path` 参数带 `node://<peer>/…` 前缀、或 `command` 参数带
/// `node://<peer>/<命令>` 前缀（bash）时，经联邦 Invoke 把调用转发到
/// 对端同名工具执行（出口见 [`crate::RemoteInvoker`]，装配层
/// 注入；对端拒绝——白名单/路径沙箱——原样透传为 `ToolError`）；其余
/// 原样走本地逻辑。
pub struct RemoteAwareTool {
    inner: Arc<dyn Tool>,
    /// 装配期快照；`None` 时 execute 再回查全局出口（联邦后接线场景）。
    invoker: Option<crate::RemoteInvoker>,
    /// inner 描述 + node:// 用法说明（清单生成）。
    description: String,
}

impl RemoteAwareTool {
    /// 包装一个本地工具，附上 `node://` 远程分流能力。
    ///
    /// `invoker`：组合根的联邦出口（`None` = 联邦未接线，带 `node://` 前缀
    /// 的调用直接报「联邦未接线」，本机路径照常走 `inner`）。
    pub fn wrap(inner: Arc<dyn Tool>, invoker: Option<crate::RemoteInvoker>) -> Arc<dyn Tool> {
        let description = format!(
            "{} Path may carry a `node://<peer>/<绝对路径>` prefix to operate on a remote federated node's workspace files directly (executed on the peer inside its sandbox).",
            inner.description()
        );
        Arc::new(Self {
            inner,
            invoker,
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
    // ── 声明类方法必须**全部**转发（2026-11 S8）──
    //
    // 包装器漏转发一条 = 被包的工具静默失能：`state_key` 丢了面板不再刷新，
    // `remote_capable` 丢了联邦里根本看不见这个工具，`path_arguments` 丢了
    // 远程调用绕过沙箱（安全性回归，且没有任何报错）。Rust 的默认实现让
    // 漏转发**编译通过**，所以这份清单必须与 `Tool` 的声明方法一一对应；
    // `declarations_survive_wrapping` 测试替未来兜底（新增声明方法时它会红）。
    fn produces_visible_output(&self) -> bool {
        self.inner.produces_visible_output()
    }
    fn snapshot(&self) -> Option<Value> {
        self.inner.snapshot()
    }
    fn snapshot_for_session(&self, session_id: &str) -> Option<Value> {
        self.inner.snapshot_for_session(session_id)
    }
    fn snapshot_machine_fields(&self) -> &'static [&'static str] {
        self.inner.snapshot_machine_fields()
    }
    fn state_key(&self) -> Option<&'static str> {
        self.inner.state_key()
    }
    fn remote_capable(&self) -> bool {
        self.inner.remote_capable()
    }
    fn path_arguments(&self) -> &'static [&'static str] {
        self.inner.path_arguments()
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
        let invoker = self.invoker.clone().ok_or_else(|| {
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
    /// 执行前的命令护栏（宿主注入；见 [`crate::CommandGuard`]）。
    guard: Option<crate::CommandGuard>,
}

impl RunCommandTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            guard: None,
        }
    }

    /// 注入命令护栏（组合根装配期；见 [`crate::CommandGuard`]）。
    pub fn with_guard(mut self, guard: Option<crate::CommandGuard>) -> Self {
        self.guard = guard;
        self
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
    // ── 远程面声明（2026-11 S8）──
    /// 可远程：命令是自包含的（`node://` 分流另有机理，见 `RemoteAwareTool`）。
    ///
    /// 注意这里**没有** `path_arguments`：`bash` 的 `command` 是命令串不是
    /// 路径，无法按参数裁决沙箱——所以远程 bash 的边界是 peer 策略
    /// （`allow_tools`）而不是目录。这条诚实说明比假装有沙箱重要。
    fn remote_capable(&self) -> bool {
        true
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

        if let Some(guard) = &self.guard {
            guard(cmd)?;
        }

        // Block dangerous patterns.
        // 2026-10 审计修复：rm 改为词元/路径语义判定（此前是子串匹配，
        // 正常子路径清理全被误拦）；其余模式保持子串匹配不变。
        if let Some(target) = dangerous_rm_root_target(cmd) {
            return Err(ToolError::Execution(format!(
                "blocked dangerous command pattern: rm -rf {target}（受保护的系统路径）"
            )));
        }
        let lower = cmd.to_lowercase();
        for dangerous in &["mkfs.", "dd if=", ":(){ :|:& };:", "> /dev/sda"] {
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

/// 注册编码工具（5 个文件工具 + `bash`）。
///
/// - `workspace`：路径沙箱根（所有读写都必须落在其内）；
/// - `invoker`：`node://` 远程分流出口（None = 远程前缀报「联邦未接线」）；
/// - `guard`：`bash` 执行前的命令护栏（None = 只做内建的危险模式检查）。
///
/// 为什么把这些做成参数而不是全局读取：本 crate 不依赖业务/组合根 crate，
/// 全局态又受 `no_global_state` 门禁约束——显式注入是唯一干净的方向。
pub fn register_coding_tools(
    registry: &mut ToolRegistry,
    workspace: PathBuf,
    guard: Option<crate::CommandGuard>,
    invoker: Option<crate::RemoteInvoker>,
) {
    // P3-1：5 个文件工具 + bash 都包一层远程分流（node:// 前缀 → 联邦
    // Invoke）。invoker 在注册期即注入——组合根先装配联邦、后构造
    // Agent，所以注册时拿到的就是运行期要用的那份。
    let wrap = |tool: Arc<dyn Tool>| RemoteAwareTool::wrap(tool, invoker.clone());
    registry.register(wrap(Arc::new(ReadFileTool::new(workspace.clone()))));
    registry.register(wrap(Arc::new(ListFilesTool::new(workspace.clone()))));
    registry.register(wrap(Arc::new(SearchCodeTool::new(workspace.clone()))));
    registry.register(wrap(Arc::new(WriteFileTool::new(workspace.clone()))));
    registry.register(wrap(Arc::new(EditFileTool::new(workspace.clone()))));
    registry.register(wrap(Arc::new(
        RunCommandTool::new(workspace).with_guard(guard),
    )));
}

/// 递归+强制 rm 的根目录目标判定（返回命中的目标用于报错；None = 放行）。
///
/// 2026-10 审计修复：旧实现用**子串匹配**判（"rm -rf " + "/" 连写——
/// 任何含该片段的命令都被拦，正常清理也被误伤，实测 16+ 次误报阻断）。
/// 现在按 shell 词元扫描 rm 调用：仅当「同时带递归 (-r/-R/--recursive)
/// 与强制 (-f/--force) 标志，或显式 --no-preserve-root」且目标为根目录、
/// 根级通配 /*、或根下一级系统目录时才判定危险；带引号的写法
/// （如 bash -c '…'）剥引号后同规则处理。
fn dangerous_rm_root_target(cmd: &str) -> Option<String> {
    // 根下一级受保护目录（整目录删除视为灾难性；其子路径不受限）。
    const PROTECTED_ROOT_DIRS: &[&str] = &[
        "/bin", "/boot", "/dev", "/etc", "/home", "/lib", "/lib64", "/opt", "/proc", "/root",
        "/run", "/sbin", "/srv", "/sys", "/usr", "/var",
    ];
    for segment in cmd.split([';', '|', '&', '\n', '\r']) {
        let mut words = segment
            .split_whitespace()
            .map(|word| word.trim_matches(|c| c == '\'' || c == '"'))
            .peekable();
        while let Some(word) = words.next() {
            if word != "rm" && !word.ends_with("/rm") {
                continue;
            }
            let mut recursive = false;
            let mut force = false;
            let mut no_preserve_root = false;
            let mut targets: Vec<&str> = Vec::new();
            for word in words.by_ref() {
                match word {
                    "--no-preserve-root" => no_preserve_root = true,
                    "--recursive" => recursive = true,
                    "--force" => force = true,
                    "-" => {}
                    _ if word.starts_with('-') && word.len() > 1 => {
                        // 组合短标志（-rf / -fr / -r -f …）；未知长标志
                        // （如 --verbose）不携带 r/f 语义，按前缀排除。
                        if !word.starts_with("--") {
                            recursive |= word.contains('r') || word.contains('R');
                            force |= word.contains('f');
                        }
                    }
                    _ => targets.push(word),
                }
            }
            if !no_preserve_root && !(recursive && force) {
                continue;
            }
            for target in targets {
                let trimmed = target.trim_end_matches('/');
                let normalized = if trimmed.is_empty() { "/" } else { trimmed };
                if no_preserve_root
                    || normalized == "/"
                    || normalized == "/*"
                    || PROTECTED_ROOT_DIRS.contains(&normalized)
                {
                    return Some(target.to_string());
                }
            }
        }
    }
    None
}
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
    async fn injected_command_guard_blocks_execution() {
        let ws = temp_workspace("cmd-self-stop");
        let guard: crate::CommandGuard = Arc::new(|command| {
            if command.contains("systemctl --user stop echo-agent-core") {
                Err(ToolError::Execution("blocked command".into()))
            } else {
                Ok(())
            }
        });
        let tool = RunCommandTool::new(ws.clone()).with_guard(Some(guard));
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

    /// 词元/路径语义回归（2026-10 审计修复）：正常子路径清理放行，
    /// 根目录 / 根级通配 / 根下一级系统目录仍拦截；本用例只对判定
    /// 函数断言，不实际执行任何命令。
    #[test]
    fn rm_guard_distinguishes_root_targets_from_subpaths() {
        for blocked in [
            "rm -rf /",
            "rm -rf /*",
            "rm -rf /etc",
            "rm -fr /usr/",
            "rm --recursive --force /home",
            "rm -rf --no-preserve-root /",
            "bash -c 'rm -rf /'",
            "rm -rf /tmp/keep-data /",
            "cd / && rm -r -f /var",
        ] {
            assert!(
                dangerous_rm_root_target(blocked).is_some(),
                "must block: {blocked}"
            );
        }
        for allowed in [
            "rm -rf /tmp/fbx_test_out",
            "cd /tmp && rm -rf x",
            "rm -rf /home/evence/.cache/echo-wt",
            "rm -rf /var/log/myapp",
            "rm -rf ./build && cargo build",
            "rm -f /tmp/a.txt",
            "rm /",
            "find / -maxdepth 1 -name 'x' -delete",
        ] {
            assert!(
                dangerous_rm_root_target(allowed).is_none(),
                "must allow: {allowed}"
            );
        }
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
        let invoker: crate::RemoteInvoker = Arc::new(move |peer, tool, args| {
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
    ) -> crate::RemoteInvoker {
        std::sync::Arc::new(move |peer, tool, args| {
            let result = f(peer, tool, args);
            Box::pin(async move { result })
        })
    }

    fn wrapped(inner: Arc<dyn Tool>, invoker: Option<crate::RemoteInvoker>) -> Arc<dyn Tool> {
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
        // 重复斜杠归一：node://peer//srv → /srv（2026-10）
        let (_, path) = parse_node_path("node://gpu-box//srv/repo")
            .expect("node:// detected")
            .expect("valid form");
        assert_eq!(path, "/srv/repo");
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

    /// **注册期注入的 invoker 端到端生效**（P3：node:// 分流的唯一入口）。
    ///
    /// 这条锁的是一个真实故障：改用 kernel cell 之后组合根只 set 了
    /// echo-agent 那份、工具读的是本 crate 那份（从未被 set），于是所有
    /// `node://` 调用恒报「联邦未接线」。改成注册参数后，注册进来的
    /// invoker 必须能被 `read_file` 真正用上。
    #[tokio::test]
    async fn register_coding_tools_wires_invoker_into_file_tools() {
        let ws = std::env::temp_dir().join(format!("echo-coding-invoker-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let invoker: crate::RemoteInvoker = {
            let seen = seen.clone();
            Arc::new(move |peer, tool, args| {
                seen.lock().unwrap().push((peer, tool));
                let path = args["path"].as_str().unwrap_or_default().to_string();
                Box::pin(async move { Ok(format!("remote:{path}")) })
            })
        };
        let mut registry = ToolRegistry::new();
        register_coding_tools(&mut registry, ws.clone(), None, Some(invoker));

        let out = registry
            .execute("read_file", json!({"path": "node://gpu-box/srv/repo/a.rs"}))
            .await
            .expect("remote read should succeed via injected invoker");
        assert_eq!(out, "remote:/srv/repo/a.rs");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[("gpu-box".to_string(), "read_file".to_string())]
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// 包装器**不得吞掉声明**（2026-11 S8）。
    ///
    /// Rust 的 trait 默认实现让"漏转发"编译通过，后果是静默失能：`state_key`
    /// 丢了面板不刷新、`remote_capable` 丢了联邦看不见、`path_arguments` 丢了
    /// 远程调用绕过沙箱（安全性回归）。这条测试逐个断言转发到位——新增声明
    /// 方法时把这里也补上，漏了会红。
    #[test]
    fn declarations_survive_wrapping() {
        // 逐个包装所有声明了远程面的工具，断言声明一致。
        let inner: Vec<Arc<dyn Tool>> = vec![
            Arc::new(ReadFileTool::new(PathBuf::from("/tmp"))),
            Arc::new(ListFilesTool::new(PathBuf::from("/tmp"))),
            Arc::new(SearchCodeTool::new(PathBuf::from("/tmp"))),
            Arc::new(WriteFileTool::new(PathBuf::from("/tmp"))),
            Arc::new(EditFileTool::new(PathBuf::from("/tmp"))),
            Arc::new(RunCommandTool::new(PathBuf::from("/tmp"))),
        ];
        assert_eq!(inner.len(), 6, "工具族变了就同步这条测试");
        for tool in &inner {
            let wrapped = RemoteAwareTool::wrap(tool.clone(), None);
            assert_eq!(wrapped.name(), tool.name());
            assert!(
                wrapped.remote_capable(),
                "{}: 包装后丢了远程面声明（联邦将看不见它）",
                tool.name()
            );
            assert_eq!(
                wrapped.path_arguments(),
                tool.path_arguments(),
                "{}: 包装后丢了路径参数声明（远程沙箱会失效）",
                tool.name()
            );
            assert_eq!(
                wrapped.produces_visible_output(),
                tool.produces_visible_output()
            );
            assert_eq!(wrapped.state_key(), tool.state_key());
            assert_eq!(
                wrapped.snapshot_machine_fields(),
                tool.snapshot_machine_fields(),
                "{}: 包装后丢了快照机器字段声明（UUID / epoch 会被当秘密洗掉）",
                tool.name()
            );
        }
    }

    /// 包装器必须**真的**转发非空声明（上一条测试里的工具默认都为空，
    /// 只断言相等无法区分"转发"和"硬编码空"）。同时兜住 `state_key` /
    /// `remote_capable`：漏转发会让面板不刷新、联邦看不见工具。
    #[test]
    fn remote_aware_forwards_nonempty_declarations() {
        struct MachineFieldTool;
        #[async_trait::async_trait]
        impl Tool for MachineFieldTool {
            fn name(&self) -> &str {
                "machine-field-stub"
            }
            fn description(&self) -> &str {
                "stub"
            }
            async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
                Ok("ok".into())
            }
            fn produces_visible_output(&self) -> bool {
                true
            }
            fn snapshot(&self) -> Option<Value> {
                Some(json!({"id": "stable-id", "text": "user text"}))
            }
            fn snapshot_machine_fields(&self) -> &'static [&'static str] {
                &["id"]
            }
            fn state_key(&self) -> Option<&'static str> {
                Some("machine-state")
            }
            fn remote_capable(&self) -> bool {
                true
            }
            fn path_arguments(&self) -> &'static [&'static str] {
                &["path"]
            }
        }
        let inner: Arc<dyn Tool> = Arc::new(MachineFieldTool);
        let wrapped = RemoteAwareTool::wrap(inner.clone(), None);
        assert_eq!(
            wrapped.snapshot_machine_fields(),
            &["id"],
            "包装后丢了非空机器字段声明"
        );
        assert_eq!(wrapped.snapshot(), inner.snapshot());
        assert_eq!(wrapped.state_key(), Some("machine-state"));
        assert!(wrapped.remote_capable());
        assert_eq!(wrapped.path_arguments(), &["path"]);
        assert!(wrapped.produces_visible_output());
    }
    /// 工具族自身的远程面声明（S8）：6 个编码工具可远程，文件类带 `path`。
    ///
    /// 这些声明取代了组合根与联邦模块里两份写死的名字表——名字改了、工具
    /// 挪了位置，远程面自动跟着走。
    #[test]
    fn coding_tools_declare_their_remote_surface() {
        let registry: Arc<ToolRegistry> = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(ReadFileTool::new(PathBuf::from("/tmp"))));
        registry.register_shared(Arc::new(ListFilesTool::new(PathBuf::from("/tmp"))));
        registry.register_shared(Arc::new(SearchCodeTool::new(PathBuf::from("/tmp"))));
        registry.register_shared(Arc::new(WriteFileTool::new(PathBuf::from("/tmp"))));
        registry.register_shared(Arc::new(EditFileTool::new(PathBuf::from("/tmp"))));
        registry.register_shared(Arc::new(RunCommandTool::new(PathBuf::from("/tmp"))));

        assert_eq!(
            registry.remote_capable_names(),
            vec![
                "bash",
                "edit_file",
                "list_files",
                "read_file",
                "search_code",
                "write_file"
            ],
            "远程面 = 6 个编码工具（清单类工具按会话隔离，不在本模块，也不许宣告）"
        );
        for file_tool in ["read_file", "list_files", "write_file", "edit_file"] {
            assert_eq!(
                registry.path_arguments_of(file_tool),
                ["path"],
                "{file_tool} 必须声明路径参数（否则远程调用绕过沙箱）"
            );
        }
        // `bash` 的边界是 peer 策略而不是目录（command 是命令串，不是路径）。
        assert!(registry.path_arguments_of("bash").is_empty());
    }

    /// 清单生成：包装后的工具同名，描述带 node:// 用法说明。
    #[test]
    fn remote_aware_tool_advertises_node_prefix() {
        let tool = RemoteAwareTool::wrap(Arc::new(ReadFileTool::new(PathBuf::from("/tmp"))), None);
        assert_eq!(tool.name(), "read_file");
        assert!(
            tool.description().contains("node://"),
            "description: {}",
            tool.description()
        );
    }
    // ── 2026-11 文件工具审计回归 ────────────────────────────────────────
    // 下面这组用例锁的是四类真实故障：
    //   1. 悬空符号链接绕过 guard（canonicalize 失败后回退父目录）；
    //   2. content/new_content 参数错误被 `unwrap_or("")` 静默降级成清空；
    //   3. search_code 声称 regex 实际 contains，且漏掉前端源码类型；
    //   4. read_file 只能整读（>100000 行只能退回 bash）。

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_rejects_dangling_symlink_escape() {
        // 旧实现：link.txt 的 canonicalize 失败（悬空）→ 回退父目录（工作区
        // 内）→ 放行 → fs::write 跟随链接在区外创建目标文件。
        let ws = temp_workspace("dangling-link");
        let outside =
            std::env::temp_dir().join(format!("echo-coding-dangling-out-{}", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        std::os::unix::fs::symlink(&outside, ws.join("link.txt")).unwrap();

        let tool = WriteFileTool::new(ws.clone());
        let err = tool
            .execute(json!({"path": "link.txt", "content": "pwned"}))
            .await
            .expect_err("dangling symlink must be rejected");
        assert!(
            err.to_string().contains("dangling symlink"),
            "unexpected error: {err}"
        );
        assert!(
            !outside.exists(),
            "target outside workspace must not be created"
        );
        assert_eq!(
            std::fs::read_link(ws.join("link.txt")).unwrap(),
            outside,
            "the dangling link itself must stay untouched"
        );
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_rejects_dangling_directory_symlink() {
        let ws = temp_workspace("dangling-dir-link");
        let outside_dir =
            std::env::temp_dir().join(format!("echo-coding-dangling-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside_dir);
        std::os::unix::fs::symlink(&outside_dir, ws.join("outdir")).unwrap();

        let tool = WriteFileTool::new(ws.clone());
        let err = tool
            .execute(json!({"path": "outdir/file.txt", "content": "x"}))
            .await
            .expect_err("dangling directory symlink must be rejected");
        assert!(
            err.to_string().contains("dangling symlink"),
            "unexpected error: {err}"
        );
        assert!(!outside_dir.exists(), "outside dir must not be created");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn write_file_bad_content_does_not_truncate_or_create() {
        let ws = temp_workspace("write-content-args");
        let target = ws.join("keep.txt");
        std::fs::write(&target, "keep me\n").unwrap();
        let tool = WriteFileTool::new(ws.clone());

        for args in [
            json!({"path": "keep.txt"}),
            json!({"path": "keep.txt", "content": null}),
            json!({"path": "keep.txt", "content": 42}),
            json!({"path": "keep.txt", "content": {"text": "x"}}),
            json!({"path": "keep.txt", "content": ["x"]}),
        ] {
            let err = tool.execute(args.clone()).await.unwrap_err();
            assert!(
                matches!(&err, ToolError::InvalidArguments(_)),
                "args {args}: {err:?}"
            );
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "keep me\n",
                "existing file must stay intact for {args}"
            );
        }

        // 缺失 content 也不能先创建空文件。
        let err = tool
            .execute(json!({"path": "brand-new.txt"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        assert!(!ws.join("brand-new.txt").exists(), "no side-effect file");

        // 合法空串是显式「写空文件」，不能被误拒。
        tool.execute(json!({"path": "keep.txt", "content": ""}))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn edit_file_bad_new_content_does_not_delete_lines() {
        let ws = temp_workspace("edit-content-args");
        let target = ws.join("f.txt");
        std::fs::write(&target, "one\ntwo\nthree\n").unwrap();
        let tool = EditFileTool::new(ws.clone());

        for args in [
            json!({"path": "f.txt", "start_line": 2, "end_line": 2}),
            json!({"path": "f.txt", "start_line": 2, "end_line": 2, "new_content": null}),
            json!({"path": "f.txt", "start_line": 2, "end_line": 2, "new_content": 7}),
        ] {
            let err = tool.execute(args.clone()).await.unwrap_err();
            assert!(
                matches!(&err, ToolError::InvalidArguments(_)),
                "args {args}: {err:?}"
            );
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "one\ntwo\nthree\n",
                "lines must not be deleted for {args}"
            );
        }

        // 合法空串仍表示「把目标行替换为空」（删除该行）。
        tool.execute(json!({
            "path": "f.txt",
            "start_line": 2,
            "end_line": 2,
            "new_content": ""
        }))
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "one\nthree\n");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_preserves_permissions_and_symlink_semantics() {
        use std::os::unix::fs::PermissionsExt;
        let ws = temp_workspace("write-perms");
        let real = ws.join("real.txt");
        std::fs::write(&real, "old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = ws.join("link.txt");
        std::os::unix::fs::symlink("real.txt", &link).unwrap();

        let tool = WriteFileTool::new(ws.clone());
        tool.execute(json!({"path": "link.txt", "content": "new"}))
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "atomic rename must not replace the symlink itself"
        );
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o640,
            "existing mode bits must survive the atomic replace"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn read_file_supports_line_ranges() {
        let ws = temp_workspace("read-range");
        let content: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        std::fs::write(ws.join("f.txt"), content).unwrap();
        let tool = ReadFileTool::new(ws.clone());

        let out = tool
            .execute(json!({"path": "f.txt", "start_line": 3, "end_line": 4}))
            .await
            .unwrap();
        assert!(out.contains("3 │ line3"), "{out}");
        assert!(out.contains("4 │ line4"), "{out}");
        assert!(!out.contains("2 │ line2"), "before range leaked: {out}");
        assert!(!out.contains("5 │ line5"), "after range leaked: {out}");
        assert!(out.contains("10 lines, showing 3-4"), "{out}");

        // 只给 start_line：读到末尾。
        let out = tool
            .execute(json!({"path": "f.txt", "start_line": 8}))
            .await
            .unwrap();
        assert!(out.contains("8 │ line8"), "{out}");
        assert!(out.contains("10 │ line10"), "{out}");
        assert!(!out.contains("7 │ line7"), "{out}");

        // 只给 end_line：从第 1 行开始。
        let out = tool
            .execute(json!({"path": "f.txt", "end_line": 2}))
            .await
            .unwrap();
        assert!(
            out.contains("1 │ line1") && out.contains("2 │ line2"),
            "{out}"
        );
        assert!(!out.contains("3 │ line3"), "{out}");

        // 非法区间明确报参数错误。
        let err = tool
            .execute(json!({"path": "f.txt", "start_line": 5, "end_line": 4}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)), "{err:?}");
        let err = tool
            .execute(json!({"path": "f.txt", "start_line": 0}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)), "{err:?}");

        // 不传分段参数：旧行为整读（输出包含全部 10 行）。
        let out = tool.execute(json!({"path": "f.txt"})).await.unwrap();
        assert!(out.contains("1 │ line1") && out.contains("10 │ line10"));
        let _ = std::fs::remove_dir_all(&ws);
    }
    #[tokio::test]
    async fn read_file_range_works_beyond_10mb() {
        use std::io::Write;
        let ws = temp_workspace("read-range-big");
        let path = ws.join("big.log");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        let filler = "x".repeat(64 * 1024);
        for i in 1..=200 {
            writeln!(file, "{i:04} {filler}").unwrap();
        }
        file.flush().unwrap();
        assert!(
            std::fs::metadata(&path).unwrap().len() > 10 * 1024 * 1024,
            "fixture must exceed the whole-read cap"
        );

        let tool = ReadFileTool::new(ws.clone());
        // 整读仍明确拒绝（不把 12MB+ 塞进上下文）。
        let err = tool.execute(json!({"path": "big.log"})).await.unwrap_err();
        assert!(err.to_string().contains("10MB"), "{err}");
        // 分段读流式扫描，>10MB 也能用。
        let out = tool
            .execute(json!({"path": "big.log", "start_line": 100, "end_line": 100}))
            .await
            .unwrap();
        assert!(out.contains("100 │ 0100 "), "line 100 missing");
        assert!(out.contains("200 lines, showing 100-100"), "{out}");
        assert!(!out.contains("99 │ 0099"), "before range leaked");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn search_code_literal_default_and_optional_regex() {
        let ws = temp_workspace("search-regex");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src/main.rs"), "fn main() {\n    let axb = 1;\n}\n").unwrap();
        let tool = SearchCodeTool::new(ws.clone());

        // 默认字面量：`\s` 没有特殊含义。
        let out = tool
            .execute(json!({"pattern": r"fn\s+main", "path": "."}))
            .await
            .unwrap();
        assert!(out.starts_with("No matches"), "{out}");

        // regex=true 才按正则解释。
        let out = tool
            .execute(json!({"pattern": r"fn\s+main", "path": ".", "regex": true}))
            .await
            .unwrap();
        assert!(out.contains("src/main.rs:1"), "{out}");

        // 默认仍大小写不敏感（旧行为兼容）。
        let out = tool
            .execute(json!({"pattern": "AXB", "path": "."}))
            .await
            .unwrap();
        assert!(out.contains("main.rs:2"), "{out}");

        // 默认 `.` 是字面量；regex=true 才是通配。
        let out = tool
            .execute(json!({"pattern": "a.b", "path": "."}))
            .await
            .unwrap();
        assert!(out.starts_with("No matches"), "dot must be literal: {out}");
        let out = tool
            .execute(json!({"pattern": "a.b", "path": ".", "regex": true}))
            .await
            .unwrap();
        assert!(out.contains("main.rs:2"), "{out}");

        let err = tool
            .execute(json!({"pattern": "(", "path": ".", "regex": true}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::InvalidArguments(m) if m.contains("invalid regex")),
            "{err:?}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn search_code_covers_frontend_source_types() {
        let ws = temp_workspace("search-ext");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        for name in ["a.ts", "b.tsx", "c.js", "d.jsx", "e.vue"] {
            std::fs::write(ws.join("src").join(name), "const needle = 1;\n").unwrap();
        }
        let tool = SearchCodeTool::new(ws.clone());
        let out = tool
            .execute(json!({"pattern": "needle", "path": "."}))
            .await
            .unwrap();
        for name in ["a.ts", "b.tsx", "c.js", "d.jsx", "e.vue"] {
            assert!(out.contains(name), "{name} missing from: {out}");
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn search_code_skips_vendor_and_build_dirs() {
        let ws = temp_workspace("search-ignore");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src/app.ts"), "needle\n").unwrap();
        for (dir, file) in [
            ("node_modules/pkg", "index.js"),
            ("target/debug", "generated.rs"),
            (".git/hooks", "hook.sh"),
            ("dist", "bundle.js"),
        ] {
            std::fs::create_dir_all(ws.join(dir)).unwrap();
            std::fs::write(ws.join(dir).join(file), "needle\n").unwrap();
        }

        let tool = SearchCodeTool::new(ws.clone());
        let out = tool
            .execute(json!({"pattern": "needle", "path": "."}))
            .await
            .unwrap();
        assert!(out.contains("src/app.ts"), "{out}");
        for dir in ["node_modules", "target", ".git", "dist"] {
            assert!(!out.contains(dir), "must skip {dir}: {out}");
        }

        // 显式把 target 作为搜索根时仍要搜（filter 只看 depth > 0）。
        let out = tool
            .execute(json!({"pattern": "needle", "path": "target"}))
            .await
            .unwrap();
        assert!(out.contains("target/debug/generated.rs"), "{out}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn search_code_skips_files_over_2mb() {
        let ws = temp_workspace("search-large");
        let mut big = String::from("needle\n");
        big.push_str(&"x".repeat(3 * 1024 * 1024));
        std::fs::write(ws.join("big.ts"), big).unwrap();
        std::fs::write(ws.join("small.ts"), "needle\n").unwrap();

        let tool = SearchCodeTool::new(ws.clone());
        let out = tool
            .execute(json!({"pattern": "needle", "path": "."}))
            .await
            .unwrap();
        assert!(out.contains("small.ts"), "{out}");
        assert!(
            !out.contains("big.ts"),
            "oversized file must be skipped: {out}"
        );
        assert!(out.contains("跳过 1 个"), "skip hint expected: {out}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn search_code_truncation_is_explicit() {
        let ws = temp_workspace("search-trunc");
        let mut content = String::new();
        for i in 0..80 {
            content.push_str(&format!("needle {i}\n"));
        }
        std::fs::write(ws.join("many.rs"), content).unwrap();

        let tool = SearchCodeTool::new(ws.clone());
        let out = tool
            .execute(json!({"pattern": "needle", "path": "."}))
            .await
            .unwrap();
        assert!(out.starts_with("50 matches"), "{out}");
        assert!(out.contains("结果已截断"), "truncation hint missing: {out}");
        assert_eq!(out.lines().count().saturating_sub(1), 50);
        let _ = std::fs::remove_dir_all(&ws);
    }
}
