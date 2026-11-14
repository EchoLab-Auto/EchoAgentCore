//! 入站文件附件的落盘目录（面板拖入 / 平台上传）。
//!
//! # 与 [`crate::media_store`] 的分工
//!
//! | | media_store | 这里 |
//! |---|---|---|
//! | 内容 | **图片**（MIME 白名单） | **任意文件** |
//! | 命名 | 内容哈希 + 扩展名 | 原文件名（消毒后） |
//! | 目录 | 单一平面 | 按会话分目录 |
//! | 展示 | `/media/<id>` 给 `<img>` 强缓存 | 不经 HTTP，只给 agent 的文件工具读 |
//!
//! 文件**不能**复用 `/media/` 路由：那个路由的强缓存前提是"名字含内容哈希、
//! 字节永不变化"，原文件名不满足（同名文件换内容 = 缓存命中旧字节）。
//!
//! # 为什么不用系统临时目录
//!
//! `/tmp` 看起来是"临时文件"的天然去处，但两个真实问题：
//!
//! 1. **存活期不可控**：agent 常在消息之后几分钟才读文件（排队、多轮思考），
//!    而系统清理器不保证这个窗口；
//! 2. **权限是全局的**：多用户机器上 `/tmp` 通常 world-readable。
//!
//! 所以落 Core 自己的数据根，生命周期等于本机数据而非进程：
//! `$ECHO_ATTACHMENT_DIR`，缺省 `~/.local/share/echo-agent-core/attachments`
//! （与 media / 文件下载同一数据根）。
//!
//! # 路径安全
//!
//! 文件名来自用户（浏览器拖入 = 完全不可信）。消毒只保留
//! `[字母数字 . _ - 空格]`（Unicode 字母数字，中文名照常保留），
//! 其余替换为 `_`；`/` `\` 与 `..` 因此不可能穿越。
//! 会话 id 同样消毒后当目录名（`local:workspace:a/b:user` → `local_workspace_a_b_user`）。

use std::path::{Path, PathBuf};

/// 单文件上限。
///
/// 定这个数的依据是链路而非磁盘：字节以 base64 内嵌在 ws 命令帧里
/// （+33%），而两端 ws 的 `max_frame_size` 已显式抬到 32 MiB（默认 16 MiB
/// 会让 20 MB 文件直接被拒）。20 MB × 4/3 ≈ 26.7 MiB，留出信封余量。
pub const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// 文件名主体（不含扩展名）保留的最大字符数——躲开 `ENAMETOOLONG`
/// （多数文件系统单段上限 255 **字节**，UTF-8 中文 3 字节/字）。
const MAX_NAME_CHARS: usize = 80;

/// 附件根目录：`$ECHO_ATTACHMENT_DIR` 优先；否则 HOME 下的 Core 数据根；
/// 再否则工作目录下的 `attachments`。
pub fn attachment_dir() -> PathBuf {
    attachment_dir_with_home(
        std::env::var_os("ECHO_ATTACHMENT_DIR"),
        std::env::var_os("HOME"),
    )
}

fn attachment_dir_with_home(
    override_dir: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(dir) = override_dir.filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    match home {
        Some(home) => PathBuf::from(home).join(".local/share/echo-agent-core/attachments"),
        None => PathBuf::from("attachments"),
    }
}

/// 文件名消毒：去掉路径分隔符、穿越片段与控制字符，保留可读原名。
///
/// 空结果（原名全是非法字符）回退 `file`——落盘失败比名字难看糟糕得多。
pub fn sanitize_file_name(raw: &str) -> String {
    // 先取 basename：`../../etc/passwd` 与 `C:\x\y.txt` 都只剩最后一段。
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim();

    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ') {
                c
            } else {
                // 控制字符、引号、`:`、`*`、`?` 等一并抹掉；也顺手挡掉
                // 双扩展名伪装（`x.rs.exe` 消毒后形状不变，但那不是本层职责）。
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim();

    // 只精确拒绝 `.` / `..` / 空：`/` 与 `\\` 已在上面被抹成 `_`，所以
    // `....` 这类只是普通文件名，不需要额外处理；而 trim 掉前导点会把
    // `.gitignore` 改名成 `gitignore`——那是无谓的信息损失。
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return "file".into();
    }

    // 拆扩展名限长（扩展名本身限 16 字符，超长视为无扩展名）。
    let (stem, ext) = match cleaned.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 && !ext.is_empty() => {
            (stem, Some(ext))
        }
        _ => (cleaned, None),
    };
    let stem: String = stem.chars().take(MAX_NAME_CHARS).collect();
    match ext {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem,
    }
}

/// 会话 id 消毒后当目录名。
///
/// 会话 id 含 `:` 与 `/`（`local:workspace:<workspace_id>:<user_id>`），
/// 直接用会造出嵌套目录甚至穿越。全部折叠成 `_` 后仍保持可辨认
/// （`local_workspace_echo-agent_v_local_user`）。
///
/// 注意 `.` **也**被折叠：它是合法字符但用作目录名时 `..` 会让
/// `root.join("..")` 直接落到父目录——一个不需要的功能带来的洞。
pub fn sanitize_session_dir(session_id: &str) -> String {
    let cleaned: String = session_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('_');
    if cleaned.is_empty() {
        "session".into()
    } else {
        cleaned.chars().take(120).collect()
    }
}

/// 原始文件名 → 落盘路径（同名时追加 `-1`、`-2` …）。
///
/// 覆盖而不是加序号会让第二条消息的附件悄悄换掉第一条的内容——agent 已经
/// 读过的路径在下一轮变了含义，这种"同名不同物"没有任何线索可查。
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    for index in 1..1000 {
        let candidate = dir.join(format!("{stem}-{index}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // 极端情况（同名 1000 份）：退回时间戳，仍不会覆盖。
    dir.join(format!(
        "{stem}-{}{ext}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ))
}

/// 原子上写：临时文件 + rename，避免 agent 读到半截文件。
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create attachment dir failed: {e}"))?;
    }
    let temporary = path.with_extension(format!(
        "{}.part{}",
        path.extension().and_then(|e| e.to_str()).unwrap_or("bin"),
        std::process::id()
    ));
    std::fs::write(&temporary, bytes).map_err(|e| format!("write attachment failed: {e}"))?;
    std::fs::rename(&temporary, path).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        format!("commit attachment failed: {e}")
    })
}

/// base64 载荷解码（面板内嵌上传的线格式）。
pub fn decode_base64(payload: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    // 容忍 `data:<mime>;base64,` 前缀：前端有时直接给 data URI。
    let payload = payload
        .split_once(";base64,")
        .map(|(_, rest)| rest)
        .unwrap_or(payload);
    base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|e| format!("base64 decode failed: {e}"))
}

/// 落盘一份附件，返回绝对路径。
pub fn save_attachment(session_id: &str, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    save_attachment_in(&attachment_dir(), session_id, name, bytes)
}

/// 同 [`save_attachment`]，落盘到指定根目录（测试注入用）。
pub fn save_attachment_in(
    root: &Path,
    session_id: &str,
    name: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    if bytes.is_empty() {
        return Err("empty file payload".into());
    }
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "file too large ({} bytes > {MAX_ATTACHMENT_BYTES})",
            bytes.len()
        ));
    }
    let dir = root.join(sanitize_session_dir(session_id));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create attachment dir failed: {e}"))?;
    let path = unique_path(&dir, &sanitize_file_name(name));
    write_atomic(&path, bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "echo-attach-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn sanitize_keeps_readable_names() {
        assert_eq!(sanitize_file_name("report.pdf"), "report.pdf");
        // 中文名照常保留（Unicode 字母数字）。
        assert_eq!(sanitize_file_name("季度报告.pdf"), "季度报告.pdf");
        // 空格保留，括号折叠成 `_`。
        assert_eq!(sanitize_file_name("my file (1).txt"), "my file _1_.txt");
        // 前导点保留：`.gitignore` 改名成 `gitignore` 是无谓的信息损失。
        assert_eq!(sanitize_file_name(".gitignore"), ".gitignore");
    }

    #[test]
    fn sanitize_blocks_traversal() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("..\\..\\windows\\system32\\x.dll"), "x.dll");
        assert_eq!(sanitize_file_name(".."), "file");
        assert_eq!(sanitize_file_name(""), "file");
        assert_eq!(sanitize_file_name("   "), "file");
        // 绝对路径只留最后一段。
        assert_eq!(sanitize_file_name("/etc/shadow"), "shadow");
    }

    #[test]
    fn sanitize_limits_length() {
        let long = format!("{}.txt", "a".repeat(300));
        let out = sanitize_file_name(&long);
        assert!(out.len() <= MAX_NAME_CHARS + 4, "got {} bytes", out.len());
        assert!(out.ends_with(".txt"));
    }

    #[test]
    fn session_dir_is_flat_and_safe() {
        assert_eq!(
            sanitize_session_dir("local:workspace:echo-agent/v:local_user"),
            "local_workspace_echo-agent_v_local_user"
        );
        assert_eq!(sanitize_session_dir("../../etc"), "etc");
        assert_eq!(sanitize_session_dir(":::"), "session");
        // 回归：`..` 曾是有效返回值，`root.join("..")` 会写进父目录。
        assert_eq!(sanitize_session_dir(".."), "session");
        assert_eq!(sanitize_session_dir("."), "session");
    }

    #[test]
    fn save_creates_nested_session_dir() {
        let root = temp_root("save");
        let path = save_attachment_in(&root, "local:workspace:ws/a:user", "a.txt", b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(path.starts_with(root.join("local_workspace_ws_a_user")));
        assert!(path.is_absolute());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_does_not_overwrite_same_name() {
        let root = temp_root("dup");
        let first = save_attachment_in(&root, "s", "a.txt", b"one").unwrap();
        let second = save_attachment_in(&root, "s", "a.txt", b"two").unwrap();
        assert_ne!(first, second);
        // 第一条的内容必须原样还在：agent 可能已经读过那个路径。
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        assert!(second.to_string_lossy().contains("a-1.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_rejects_empty_and_oversize() {
        let root = temp_root("limits");
        assert!(save_attachment_in(&root, "s", "a.txt", b"").is_err());
        let big = vec![0u8; MAX_ATTACHMENT_BYTES + 1];
        let err = save_attachment_in(&root, "s", "a.bin", &big).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn decode_accepts_plain_and_data_uri() {
        // "hello" 的 base64。
        let plain = "aGVsbG8=";
        assert_eq!(decode_base64(plain).unwrap(), b"hello");
        assert_eq!(
            decode_base64("data:text/plain;base64,aGVsbG8=").unwrap(),
            b"hello"
        );
        assert!(decode_base64("not base64!!").is_err());
    }

    #[test]
    fn dir_override_wins() {
        let dir = attachment_dir_with_home(
            Some(std::ffi::OsString::from("/custom/att")),
            Some(std::ffi::OsString::from("/home/x")),
        );
        assert_eq!(dir, PathBuf::from("/custom/att"));
        let fallback = attachment_dir_with_home(None, Some(std::ffi::OsString::from("/home/x")));
        assert_eq!(
            fallback,
            PathBuf::from("/home/x/.local/share/echo-agent-core/attachments")
        );
    }
}
