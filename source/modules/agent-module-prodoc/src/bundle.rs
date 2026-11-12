//! 文档群扫描：一棵目录 → `相对路径 → 原文` 的映射。
//!
//! 与 ProDoc CLI 的 `loadMarkdownFiles`（`prodoc-cli/src/server.ts:85`）**保持
//! 同一套语义**：跳过的目录名、大小写不敏感的 `.md` 后缀、realpath 去重防符号
//! 链接环、相对路径统一用 `/`。差异只有上限（见模块文档）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Config;

/// 扫描时跳过的目录名（与 ProDoc CLI 逐字一致）。
///
/// 这五个是**产物目录**，里面不会有作者的文档；漏掉任何一个（尤其 `target`
/// 与 `node_modules`）都会让扫描在大型仓库里跑上千个文件、直接撞上限。
pub const SKIP_DIRS: [&str; 5] = ["node_modules", ".git", ".cache", "dist", "build"];

/// 另有 `target`：Rust 项目里动辄几十万文件，单独列出便于注释其分量。
const SKIP_TARGET: &str = "target";

/// 一份文档群快照。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Bundle {
    /// 归属人格（面板按它过滤事件）。
    #[serde(default)]
    pub team_id: Option<String>,
    /// 文档根（绝对路径，已规范化）。
    pub root: String,
    /// `相对路径（`/` 分隔）→ 完整原文`，**含**框架参数区。
    pub files: std::collections::BTreeMap<String, String>,
    /// 被跳过的文件 / 目录及原因（超限、非 UTF-8、读失败）。
    ///
    /// 不静默：面板要能把"有文件没进来"显示出来，否则文档图缺节点时
    /// 作者会以为是自己的 `link` 写错了。
    #[serde(default)]
    pub skipped: Vec<Skipped>,
    /// 统计（面板显示"12 篇 / 184 KB"这类摘要）。
    #[serde(default)]
    pub stats: Stats,
}

/// 一个被跳过的条目。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Skipped {
    /// 相对路径。
    pub path: String,
    /// 原因（人话，直接显示）。
    pub reason: String,
}

/// 扫描统计。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Stats {
    /// 收进来的篇数。
    pub documents: usize,
    /// 收进来的总字节数。
    pub bytes: u64,
    /// 跳过的条目数。
    pub skipped: usize,
    /// 是否因上限提前停止（`true` 时 `skipped` 里至少有一条「已达上限」）。
    #[serde(default)]
    pub truncated: bool,
}

/// 扫描失败的原因（调用方据此决定是回错误条还是空画布）。
#[derive(Debug, Clone, PartialEq)]
pub enum ScanError {
    /// 根目录不存在 / 不是目录 / 无权限。
    RootUnavailable(String),
    /// 一篇 `.md` 都没找到——**这是错误**，不是"空文档群"。
    ///
    /// 与 CLI 一致（`server.ts:311`）：空画布会让人以为渲染坏了，
    /// 而实际是目录选错了。
    Empty,
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RootUnavailable(message) => write!(f, "{message}"),
            Self::Empty => write!(f, "目录下没有任何 .md 文件"),
        }
    }
}

/// 扫描根目录下的全部 `.md`。
///
/// 阻塞 IO：调用方负责 `spawn_blocking`（一个几千文件的目录要几百毫秒，
/// 直接放在 async 上下文里会卡住整条命令通道）。
pub fn scan(root: &Path, config: &Config) -> Result<Bundle, ScanError> {
    let root_real = std::fs::canonicalize(root).map_err(|error| {
        ScanError::RootUnavailable(format!("文档根不可访问：{}（{error}）", root.display()))
    })?;
    if !root_real.is_dir() {
        return Err(ScanError::RootUnavailable(format!(
            "文档根不是目录：{}",
            root_real.display()
        )));
    }

    let mut bundle = Bundle {
        root: root_real.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut total_bytes: u64 = 0;
    let mut truncated = false;

    // 显式栈而不是递归：目录深度不受进程栈限制，且超限时能干净地停下。
    let mut queue: Vec<(PathBuf, String)> = vec![(root_real.clone(), String::new())];
    while let Some((dir, prefix)) = queue.pop() {
        // realpath 去重：符号链接成环时不会无限展开（CLI 同款做法）。
        let Ok(dir_real) = std::fs::canonicalize(&dir) else {
            continue;
        };
        if !visited.insert(dir_real.clone()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir_real) else {
            bundle.skipped.push(Skipped {
                path: if prefix.is_empty() { ".".into() } else { prefix.clone() },
                reason: "目录不可读".into(),
            });
            continue;
        };
        // 按名字排序：`read_dir` 顺序随文件系统变化，排序后事件载荷稳定，
        // 面板不必因为"顺序变了"重绘。
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                bundle.skipped.push(Skipped { path: relative, reason: "元数据读取失败".into() });
                continue;
            };
            // `metadata()` 跟随符号链接；指向目录的软链会被当作目录入队，
            // 由 realpath 去重兜住环。
            if metadata.is_dir() {
                if SKIP_DIRS.contains(&name.as_str()) || name == SKIP_TARGET {
                    continue;
                }
                // 隐藏目录（`.obsidian`、`.vscode` 之类）不可能是文档群的一部分。
                if name.starts_with('.') {
                    continue;
                }
                queue.push((path, relative));
                continue;
            }
            if !metadata.is_file() || !is_markdown(&name) {
                continue;
            }
            if bundle.files.len() >= config.max_files {
                bundle.skipped.push(Skipped {
                    path: relative,
                    reason: format!("已达篇数上限 {}，其余文件未读", config.max_files),
                });
                truncated = true;
                break;
            }
            if metadata.len() > config.max_file_bytes {
                bundle.skipped.push(Skipped {
                    path: relative,
                    reason: format!(
                        "单文件 {} KB 超过上限 {} KB",
                        metadata.len() / 1024,
                        config.max_file_bytes / 1024
                    ),
                });
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    if total_bytes + content.len() as u64 > config.max_total_bytes {
                        bundle.skipped.push(Skipped {
                            path: relative,
                            reason: format!(
                                "已达总字节上限 {} MB，其余文件未读",
                                config.max_total_bytes / (1024 * 1024)
                            ),
                        });
                        truncated = true;
                        break;
                    }
                    total_bytes += content.len() as u64;
                    bundle.files.insert(relative, content);
                }
                // 非 UTF-8 的 `.md` 在真实仓库里存在（GBK、带 BOM 的旧文件）。
                // 报出来比静默丢弃好：作者会想知道为什么这篇不出现在图上。
                Err(error) => bundle.skipped.push(Skipped {
                    path: relative,
                    reason: format!("读取失败（{error}）"),
                }),
            }
        }
        if truncated {
            break;
        }
    }

    if bundle.files.is_empty() {
        return Err(ScanError::Empty);
    }
    bundle.stats = Stats {
        documents: bundle.files.len(),
        bytes: total_bytes,
        skipped: bundle.skipped.len(),
        truncated,
    };
    Ok(bundle)
}

/// 是否 ProDoc 文档（`.md`，大小写不敏感——Windows/macOS 上 `.MD` 很常见）。
fn is_markdown(name: &str) -> bool {
    name.len() > 3 && name[name.len() - 3..].eq_ignore_ascii_case(".md")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例一个独立临时目录（并行跑测试时不能共用）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "echo-prodoc-scan-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(path, content).expect("write");
    }

    /// 基本扫描：相对路径用 `/`、嵌套目录、只收 `.md`。
    #[test]
    fn collects_markdown_with_slash_paths() {
        let dir = temp_dir("basic");
        write(&dir, "project.md", "---\nid: p\n---\n# 根\n");
        write(&dir, "guide/intro.md", "# 入门\n");
        write(&dir, "guide/notes.txt", "忽略我");
        write(&dir, "image.png", "不是文本");

        let bundle = scan(&dir, &Config::default()).expect("scan");
        let mut keys: Vec<&str> = bundle.files.keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["guide/intro.md", "project.md"]);
        assert_eq!(bundle.stats.documents, 2);
        // 原文必须逐字保留（含框架参数区与末尾换行）——面板要靠它做 base 比对。
        assert_eq!(
            bundle.files["project.md"],
            "---\nid: p\n---\n# 根\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `.MD` 也算（macOS/Windows 上常见）。
    #[test]
    fn markdown_extension_is_case_insensitive() {
        let dir = temp_dir("case");
        write(&dir, "A.MD", "# A\n");
        let bundle = scan(&dir, &Config::default()).expect("scan");
        assert!(bundle.files.contains_key("A.MD"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 产物目录与隐藏目录一律不下探（否则 `node_modules` 会吃满上限）。
    #[test]
    fn skips_build_and_hidden_dirs() {
        let dir = temp_dir("skip");
        write(&dir, "ok.md", "# ok\n");
        for skipped in ["node_modules", ".git", "dist", "build", "target", ".obsidian"] {
            write(&dir, &format!("{skipped}/deep.md"), "# 不该出现\n");
        }
        let bundle = scan(&dir, &Config::default()).expect("scan");
        assert_eq!(bundle.files.len(), 1, "files: {:?}", bundle.files.keys().collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 篇数上限：到顶后停下并**明确报告**，不是静默少给。
    #[test]
    fn file_cap_reports_truncation() {
        let dir = temp_dir("cap");
        for index in 0..5 {
            write(&dir, &format!("doc{index}.md"), "# d\n");
        }
        let config = Config {
            max_files: 3,
            ..Config::default()
        };
        let bundle = scan(&dir, &config).expect("scan");
        assert_eq!(bundle.stats.documents, 3);
        assert!(bundle.stats.truncated, "超限必须标记 truncated");
        assert!(bundle
            .skipped
            .iter()
            .any(|item| item.reason.contains("篇数上限")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 单文件上限：只跳那一个，其余照收。
    #[test]
    fn oversized_file_is_skipped_alone() {
        let dir = temp_dir("big");
        write(&dir, "small.md", "# s\n");
        write(&dir, "big.md", &"x".repeat(4096));
        let config = Config {
            max_file_bytes: 1024,
            ..Config::default()
        };
        let bundle = scan(&dir, &config).expect("scan");
        assert!(bundle.files.contains_key("small.md"));
        assert!(!bundle.files.contains_key("big.md"));
        assert!(!bundle.stats.truncated, "单文件超限不是整体截断");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一篇都没有 → 明确的 `Empty` 错误（不是空快照）。
    #[test]
    fn empty_directory_is_an_error() {
        let dir = temp_dir("empty");
        write(&dir, "readme.txt", "no docs");
        assert_eq!(scan(&dir, &Config::default()), Err(ScanError::Empty));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 根不存在 → 可读错误。
    #[test]
    fn missing_root_is_reported() {
        let dir = temp_dir("missing");
        let missing = dir.join("nope");
        match scan(&missing, &Config::default()) {
            Err(ScanError::RootUnavailable(message)) => {
                assert!(message.contains("不可访问"), "{message}");
            }
            other => panic!("expected RootUnavailable, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 符号链接成环不该无限展开（realpath 去重）。
    #[cfg(unix)]
    #[test]
    fn symlink_loop_terminates() {
        let dir = temp_dir("loop");
        write(&dir, "a/doc.md", "# a\n");
        std::os::unix::fs::symlink(&dir, dir.join("a/back")).expect("symlink");
        let bundle = scan(&dir, &Config::default()).expect("scan");
        assert_eq!(bundle.files.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
