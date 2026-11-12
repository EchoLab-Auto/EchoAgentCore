//! 文档路径的沙箱：把「相对路径」解析成根内某个真实路径。
//!
//! 与 ProDoc CLI 的 `resolveSafeDocPath`（`prodoc-cli/src/write-api.ts:85`）同一
//! 套三重防护：根内前缀 → 只允许 `.md` → 父目录 realpath 仍在根内。第三重是
//! 关键：前两重都只看字符串，根内一个指向 `/etc` 的软链就能把写入引到根外。
//!
//! P1 只用来读，但这层现在就立起来——等 P2 加写盘时，路径校验已经是被测过的
//! 单一入口，不会出现"读走一条、写走另一条"的分叉。

use std::path::{Path, PathBuf};

/// 解析失败的原因（对面板是错误条文案，对人要能看懂是哪种越权）。
#[derive(Debug, Clone, PartialEq)]
pub enum PathError {
    /// 空路径 / 绝对路径 / 纯 `..`：拒绝。
    Invalid(String),
    /// 逃出文档根。
    Escapes,
    /// 不是 `.md`。
    NotMarkdown,
    /// 父目录不存在或不可解析（不能在不存在的子目录里新建）。
    NoParent,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "路径不合法：{message}"),
            Self::Escapes => write!(f, "路径超出文档根"),
            Self::NotMarkdown => write!(f, "只允许 .md 文件"),
            Self::NoParent => write!(f, "父目录不存在"),
        }
    }
}

/// 把 `relative` 解析成 `root` 内的真实路径。
///
/// 必须是**相对路径**：面板拿到的键就是相对的（`guide/intro.md`），传绝对路径
/// 说明调用方搞错了语义，直接拒绝比"按相对路径拼出个怪东西"安全。
pub fn resolve(root: &Path, relative: &str) -> Result<PathBuf, PathError> {
    let relative = relative.trim();
    if relative.is_empty() {
        return Err(PathError::Invalid("空路径".into()));
    }
    if Path::new(relative).is_absolute() {
        return Err(PathError::Invalid("需要相对路径".into()));
    }
    // 反斜杠是 Windows 分隔符，但在 Linux 上它只是文件名字符——统一按 `/`
    // 切分，避免 `a\..\..\etc\passwd` 在跨平台场景下绕过检查。
    if relative.split(['/', '\\']).any(|part| part == "..") {
        return Err(PathError::Escapes);
    }

    let root_real = std::fs::canonicalize(root).map_err(|_| PathError::NoParent)?;
    // 前缀带分隔符：`/docs` 不能匹配 `/docs-old`。
    let prefix = format!("{}{}", root_real.to_string_lossy(), std::path::MAIN_SEPARATOR);
    let full = root_real.join(relative);
    let full_text = full.to_string_lossy().into_owned();
    if !full_text.starts_with(&prefix) {
        return Err(PathError::Escapes);
    }
    if !is_markdown(&full_text) {
        return Err(PathError::NotMarkdown);
    }
    let parent = full.parent().ok_or(PathError::NoParent)?;
    let parent_real = std::fs::canonicalize(parent).map_err(|_| PathError::NoParent)?;
    let parent_text = parent_real.to_string_lossy().into_owned();
    if parent_text != root_real.to_string_lossy() && !parent_text.starts_with(&prefix) {
        return Err(PathError::Escapes);
    }
    Ok(full)
}

fn is_markdown(path: &str) -> bool {
    path.len() > 3 && path[path.len() - 3..].eq_ignore_ascii_case(".md")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "echo-prodoc-path-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("guide")).expect("mkdir");
        std::fs::write(dir.join("guide/intro.md"), "# intro\n").expect("write");
        dir
    }

    #[test]
    fn resolves_inside_root() {
        let dir = temp_dir("inside");
        let resolved = resolve(&dir, "guide/intro.md").expect("ok");
        assert!(resolved.ends_with("guide/intro.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_traversal() {
        let dir = temp_dir("traversal");
        assert_eq!(resolve(&dir, "../etc/passwd.md"), Err(PathError::Escapes));
        assert_eq!(resolve(&dir, "guide/../../x.md"), Err(PathError::Escapes));
        // Windows 分隔符同样要拦。
        assert_eq!(resolve(&dir, "..\\etc\\passwd.md"), Err(PathError::Escapes));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_absolute_and_non_markdown() {
        let dir = temp_dir("abs");
        assert!(matches!(
            resolve(&dir, "/etc/passwd.md"),
            Err(PathError::Invalid(_))
        ));
        assert_eq!(resolve(&dir, "guide/intro.txt"), Err(PathError::NotMarkdown));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 父目录必须已存在——这是 CLI 的既有语义（`realpath` 失败即 403），
    /// 刻意保留：允许隐式建目录会让"手滑写错一层"变成静默生成空目录。
    #[test]
    fn rejects_missing_parent() {
        let dir = temp_dir("noparent");
        assert_eq!(resolve(&dir, "ghost/deep.md"), Err(PathError::NoParent));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 根内软链指向根外 → 必须拦（这正是第三重 realpath 检查的意义）。
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_root() {
        let dir = temp_dir("symlink");
        let outside = dir.parent().unwrap().join(format!(
            "echo-prodoc-outside-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).expect("mkdir outside");
        std::fs::write(outside.join("secret.md"), "# secret\n").expect("write");
        std::os::unix::fs::symlink(&outside, dir.join("link")).expect("symlink");

        assert_eq!(resolve(&dir, "link/secret.md"), Err(PathError::Escapes));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
