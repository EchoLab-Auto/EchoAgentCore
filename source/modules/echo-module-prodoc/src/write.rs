//! 单文件读写：与 ProDoc CLI 的 `write-api.ts` 同一套语义。
//!
//! ## 为什么语义要对齐到"逐字比对"，而不是 hash / mtime
//!
//! CLI 的冲突判定是 `current !== base`（整篇字符串全等）。看着笨，但它带来
//! 一个好性质：**不需要维护版本号**，客户端只要把"我看到的内容"原样带回来。
//! 换成 mtime 会在 `git checkout` / `touch` / 网络盘时间抖动上误报；换成 hash
//! 需要额外存储，且同样对"内容没变但时间变了"无能为力。
//!
//! 代价是**原文必须逐字保留**：换行符、BOM、末尾换行一个字节都不能规范。
//! `bundle.rs` 的读取与这里的读盘都直接用 `read_to_string`，不做任何修剪。
//!
//! ## 三路合并不在这里
//!
//! CLI 在冲突时会 `merge3(base, current, content)`，干净就自动写盘。本模块
//! **刻意不实现** diff3：
//!
//! - 那 265 行（hunk 级 diff3 + LCS）复刻到 Rust，必然与 `prodoc-core/merge.ts`
//!   产生语义分叉，结果是"面板说合并成功、CLI 说冲突"；
//! - 前端本来就带着这份实现（vendored 的 `@prodoc/core`）。
//!
//! 所以职责是：**冲突时把磁盘当前内容原样回给前端**（`WriteAck::disk`），由前端
//! 跑 merge3，干净就以 `base = disk` 重发一次。两次往返，语义与 CLI 完全一致。

use std::path::Path;

use crate::paths;
use crate::WriteAck;

/// 一次写盘的结果（内部形态；`WriteAck` 由调用方补 root/path 后发事件）。
pub enum WriteOutcome {
    /// 写成功。
    Written,
    /// 需要前端合并（磁盘内容既不是 base 也不是本次内容）。
    Conflict { disk: Option<String>, error: String },
    /// 失败（路径越权 / IO 错误），不需要合并。
    Failed(String),
}

/// 读单个文档（`ReadProDocFile` 的实现主体）。
pub fn read(root: &Path, relative: &str) -> WriteAck {
    let mut ack = WriteAck {
        root: root.to_string_lossy().into_owned(),
        path: relative.to_string(),
        ..Default::default()
    };
    match paths::resolve(root, relative) {
        Ok(full) => match std::fs::read_to_string(&full) {
            Ok(content) => {
                ack.ok = true;
                ack.content = Some(content);
            }
            Err(error) => ack.error = Some(format!("读取失败：{error}")),
        },
        Err(error) => ack.error = Some(error.to_string()),
    }
    ack
}

/// 保存单个文档。
///
/// `base = None` 是**无条件覆盖**（新建文件走这条）。与 CLI 一致：调用方
/// 明确不带 base，就表示"我知道我在覆盖"。
pub fn save(root: &Path, relative: &str, content: &str, base: Option<&str>) -> WriteOutcome {
    let full = match paths::resolve(root, relative) {
        Ok(full) => full,
        Err(error) => return WriteOutcome::Failed(error.to_string()),
    };
    let current = std::fs::read_to_string(&full).ok();

    if let Some(base) = base {
        // 幂等放行：提交内容与磁盘一致（放弃更改 / 重放保存）直接成功。
        // 少了这条，一次网络重试就会变成"冲突"，而用户什么都没改。
        if current.as_deref() == Some(content) {
            return WriteOutcome::Written;
        }
        match &current {
            // 文件已被外部删除：没有可合并对象，拒绝重建。
            // 重建会把"别人删掉的文件"变回来，且用户看不到这件事。
            None => {
                return WriteOutcome::Conflict {
                    disk: None,
                    error: "文件已从磁盘删除，无法保存（请决定是否重新创建）".into(),
                }
            }
            Some(current) if current != base => {
                return WriteOutcome::Conflict {
                    disk: Some(current.clone()),
                    error: "磁盘上已有其他改动，需要合并".into(),
                }
            }
            Some(_) => {}
        }
    }

    match write_atomic(&full, content) {
        Ok(()) => WriteOutcome::Written,
        Err(error) => WriteOutcome::Failed(error),
    }
}

/// 删除单个文档。
pub fn delete(root: &Path, relative: &str, base: Option<&str>) -> WriteOutcome {
    let full = match paths::resolve(root, relative) {
        Ok(full) => full,
        Err(error) => return WriteOutcome::Failed(error.to_string()),
    };
    let current = match std::fs::read_to_string(&full) {
        Ok(current) => current,
        // 文件已不存在 = 目标达成（与 CLI 的幂等删除一致）。
        Err(_) => return WriteOutcome::Written,
    };
    if let Some(base) = base {
        if current != base {
            return WriteOutcome::Conflict {
                disk: Some(current),
                error: "文件在磁盘上已变更，删除已取消".into(),
            };
        }
    }
    match std::fs::remove_file(&full) {
        Ok(()) => WriteOutcome::Written,
        Err(error) => WriteOutcome::Failed(format!("删除失败：{error}")),
    }
}

/// 原子写：同目录临时文件 + rename。
///
/// 直接 `write` 在进程中断时会留下半截文档——对 `.md` 来说那不只是"读到一半"，
/// 而是**参数区被截断**，下一次扫描会把它当成没有参数区的文件、节点 id 从文件名
/// 推断，图上的连线就全断了。
fn write_atomic(full: &Path, content: &str) -> Result<(), String> {
    let dir = full.parent().ok_or_else(|| "父目录不存在".to_string())?;
    let name = full
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "doc.md".into());
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        // 纳秒时间戳：同一进程内两次写同一文件也不会撞名。
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&tmp, content).map_err(|error| format!("写入失败：{error}"))?;
    match std::fs::rename(&tmp, full) {
        Ok(()) => Ok(()),
        Err(error) => {
            // 失败要把临时文件清掉，否则每次失败都在文档目录里留一个隐藏文件，
            // 而它会被下一次扫描跳过（`.tmp` 不是 `.md`）——但用户 `ls -a` 会看到。
            let _ = std::fs::remove_file(&tmp);
            Err(format!("写入失败（rename）：{error}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "echo-prodoc-write-{tag}-{}-{:?}",
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

    #[test]
    fn read_returns_exact_bytes() {
        let dir = temp_dir("read");
        // 末尾无换行 + BOM 风格内容：必须逐字返回，否则 base 比对永远不等。
        write(&dir, "a.md", "---\nid: a\n---\n正文");
        let ack = read(&dir, "a.md");
        assert!(ack.ok);
        assert_eq!(ack.content.as_deref(), Some("---\nid: a\n---\n正文"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_without_base_overwrites() {
        let dir = temp_dir("overwrite");
        write(&dir, "a.md", "old");
        assert!(matches!(
            save(&dir, "a.md", "new", None),
            WriteOutcome::Written
        ));
        assert_eq!(std::fs::read_to_string(dir.join("a.md")).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_with_matching_base_writes() {
        let dir = temp_dir("base-ok");
        write(&dir, "a.md", "v1");
        assert!(matches!(
            save(&dir, "a.md", "v2", Some("v1")),
            WriteOutcome::Written
        ));
        assert_eq!(std::fs::read_to_string(dir.join("a.md")).unwrap(), "v2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 幂等：提交内容与磁盘已一致时不报冲突（网络重试 / 重放保存）。
    #[test]
    fn save_same_content_is_idempotent() {
        let dir = temp_dir("idem");
        write(&dir, "a.md", "same");
        assert!(matches!(
            save(&dir, "a.md", "same", Some("older-base")),
            WriteOutcome::Written
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 磁盘变了 → 冲突，且**带回磁盘当前内容**（前端要靠它跑 merge3）。
    #[test]
    fn save_conflict_carries_disk_content() {
        let dir = temp_dir("conflict");
        write(&dir, "a.md", "disk-version");
        match save(&dir, "a.md", "mine", Some("my-base")) {
            WriteOutcome::Conflict { disk, .. } => {
                assert_eq!(disk.as_deref(), Some("disk-version"));
            }
            other => panic!("expected conflict, got {other:?}", other = match other {
                WriteOutcome::Written => "Written",
                WriteOutcome::Failed(_) => "Failed",
                WriteOutcome::Conflict { .. } => "Conflict",
            }),
        }
        // 冲突时**不能**写盘。
        assert_eq!(
            std::fs::read_to_string(dir.join("a.md")).unwrap(),
            "disk-version"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 带 base 但文件被删了 → 冲突且 disk = None（不静默重建）。
    #[test]
    fn save_with_base_after_external_delete_conflicts() {
        let dir = temp_dir("deleted");
        match save(&dir, "a.md", "mine", Some("base")) {
            WriteOutcome::Conflict { disk, error } => {
                assert!(disk.is_none());
                assert!(error.contains("删除"), "{error}");
            }
            _ => panic!("expected conflict"),
        }
        assert!(!dir.join("a.md").exists(), "不该重建被删的文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_is_idempotent() {
        let dir = temp_dir("delete-idem");
        assert!(matches!(
            delete(&dir, "ghost.md", None),
            WriteOutcome::Written
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_conflicts_when_disk_changed() {
        let dir = temp_dir("delete-conflict");
        write(&dir, "a.md", "changed");
        assert!(matches!(
            delete(&dir, "a.md", Some("old")),
            WriteOutcome::Conflict { .. }
        ));
        assert!(dir.join("a.md").exists(), "冲突时不该删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_file() {
        let dir = temp_dir("delete-ok");
        write(&dir, "a.md", "x");
        assert!(matches!(delete(&dir, "a.md", Some("x")), WriteOutcome::Written));
        assert!(!dir.join("a.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 越权路径一律拒绝（写盘比读取更危险，这条必须有）。
    #[test]
    fn write_outside_root_is_rejected() {
        let dir = temp_dir("escape");
        assert!(matches!(
            save(&dir, "../escape.md", "x", None),
            WriteOutcome::Failed(_)
        ));
        assert!(matches!(
            save(&dir, "a.txt", "x", None),
            WriteOutcome::Failed(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 原子写不留临时文件（失败也不留——否则文档目录里会积隐藏垃圾）。
    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let dir = temp_dir("atomic");
        write(&dir, "a.md", "v1");
        let _ = save(&dir, "a.md", "v2", None);
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
