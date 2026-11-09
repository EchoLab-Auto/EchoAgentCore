//! 压缩前归档的**清单与回放**（2026-11）。
//!
//! ## 为什么需要
//!
//! 压缩（`apply_compaction`）不是给日志加个标记就完事：它把被替换的前缀事件
//! **从日志里摘掉**，换成一条 `Compaction` 事件（带摘要 + 归档路径）。于是
//! "压缩前到底聊了什么"在投影、显示时间线、面板里都**看不见了**——只有
//! `archives/` 里那份快照还留着全量事件日志。用户要能回看，就得从这里读。
//!
//! ## 两段式（清单便宜、读取才贵）
//!
//! - 清单：`read_dir` + `metadata`，不解析内容。归档单份可达 20+ MB，
//!   人格目录里可能有十几份。
//! - 读取：解析 JSON 取 `events` 数组，再用**与在线历史完全相同**的派生函数
//!   （[`super::history::display_history_for_session`]）投影——同一批展示规则
//!   （工具概要截断 / 定时信封 / 单行折叠）自然一致，不会出现"归档里看到的
//!   和在线看到的形状不同"。
//!
//! ## 安全边界
//!
//! 只接受**文件名**（面板传回来的 `SessionArchiveEntry.name`）：路径分隔符、
//! `..` 一律拒绝，且必须能在本团队 `archives/` 目录里 stat 到。归档是只读的
//! 事实记录，不存在"写回"路径。

use echo_module_management::protocol::{SessionArchiveEntry, TimelineMessage};

use super::history::display_history_for_session;
use super::TrunkStore;

/// 归档文件名合法性（只允许本模块自己生成的那种名字）。
fn is_safe_archive_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 256
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && name.ends_with(".json")
}

/// 从归档文件名里解析内嵌的 UTC 时间戳（`{stem}-YYYYMMDD-HHMMSS[-SSS][-reason].json`）。
///
/// 只在能完整解析时返回 `Some`（解析不出就返回 0 让面板显示"未知时间"，
/// 不猜）。
fn created_at_of(name: &str, stem: &str) -> (i64, String) {
    let Some(rest) = name.strip_prefix(stem).and_then(|r| r.strip_prefix('-')) else {
        return (0, String::new());
    };
    let Some(stamp) = rest.strip_suffix(".json") else {
        return (0, String::new());
    };
    let mut parts = stamp.split('-');
    let (Some(date), Some(clock)) = (parts.next(), parts.next()) else {
        return (0, String::new());
    };
    if date.len() != 8 || clock.len() != 6 {
        return (0, String::new());
    }
    // 剩余部分：`[毫秒]-[reason]` 或 `[reason]`
    let rest: Vec<&str> = parts.collect();
    let reason = rest
        .iter()
        .find(|part| part.len() != 3 || !part.chars().all(|c| c.is_ascii_digit()))
        .map(|part| part.to_string())
        .unwrap_or_default();
    let Ok(naive) = chrono::NaiveDateTime::parse_from_str(
        &format!("{date}{clock}"),
        "%Y%m%d%H%M%S",
    ) else {
        return (0, reason);
    };
    (naive.and_utc().timestamp(), reason)
}

impl TrunkStore {
    /// 本团队的 `archives/` 目录（`None` = 未启用持久化 / 尚无目录）。
    pub fn archives_dir(&self) -> Option<std::path::PathBuf> {
        let path = self.persist_path()?;
        let dir = path.parent()?.join("archives");
        dir.is_dir().then_some(dir)
    }

    /// 压缩前归档清单（按文件名倒序 = 时间倒序，最新的在前）。
    pub fn list_archives(&self) -> Vec<SessionArchiveEntry> {
        let Some(dir) = self.archives_dir() else {
            return Vec::new();
        };
        let stem = self
            .persist_path()
            .and_then(|path| path.file_stem().and_then(|s| s.to_str()).map(str::to_string))
            .unwrap_or_default();
        let Ok(read) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut entries: Vec<SessionArchiveEntry> = read
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_string();
                if !is_safe_archive_name(&name) {
                    return None;
                }
                // 只列**本团队**的归档（stem 锚定：别把其他人格的归档混进来）
                if !stem.is_empty() && !name.starts_with(&format!("{stem}-")) {
                    return None;
                }
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let (created_at, reason) = created_at_of(&name, &stem);
                Some(SessionArchiveEntry {
                    name,
                    created_at,
                    size,
                    reason,
                })
            })
            .collect();
        entries.sort_by(|a, b| b.name.cmp(&a.name));
        entries
    }

    /// 读一份归档并按会话投影成显示历史（`archive_history` 名字已被
    /// 「归档当前会话」占用，故加 `read_` 前缀）。
    ///
    /// 返回 `(messages, total, truncated, error)`。读取/解析失败不 panic
    /// （面板要显示原因，而不是让命令悬空）。
    pub fn read_archive_history(
        &self,
        archive: &str,
        session_id: &str,
        limit: usize,
    ) -> (Vec<TimelineMessage>, usize, bool, Option<String>) {
        if !is_safe_archive_name(archive) {
            return (
                Vec::new(),
                0,
                false,
                Some(format!("归档名不合法：{archive}")),
            );
        }
        let Some(dir) = self.archives_dir() else {
            return (
                Vec::new(),
                0,
                false,
                Some("该 persona 没有归档目录（未启用持久化？）".into()),
            );
        };
        let path = dir.join(archive);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) => {
                // 归档有**保留上限**（`DEFAULT_ARCHIVE_MAX_FILES`，20 份）：
                // 压缩越频繁，越早的归档越可能已被清理。这条提示要让人知道
                // "不是坏了，是被轮转掉了"，以及"哪些还在"。
                let hint = if error.kind() == std::io::ErrorKind::NotFound {
                    "该归档已被清理（archives 只保留最近 20 份），更早的压缩历史不再可读"
                } else {
                    "读取归档失败"
                };
                return (Vec::new(), 0, false, Some(format!("{hint}：{error}")));
            }
        };
        // 归档是 trunk 文档（`{ events, header, identities, timeline, version }`）：
        // 只要 `events`——那是压缩前的全量事件日志，投影的真源。
        let document: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(error) => {
                return (
                    Vec::new(),
                    0,
                    false,
                    Some(format!("归档不是合法 JSON：{error}")),
                )
            }
        };
        let Some(events_value) = document.get("events") else {
            return (
                Vec::new(),
                0,
                false,
                Some("归档里没有 events 字段（格式不符）".into()),
            );
        };
        let events: Vec<echo_session::SessionEvent> = match serde_json::from_value(
            events_value.clone(),
        ) {
            Ok(events) => events,
            Err(error) => {
                return (
                    Vec::new(),
                    0,
                    false,
                    Some(format!("归档事件反序列化失败：{error}")),
                )
            }
        };
        let (messages, total, truncated) =
            display_history_for_session(&events, session_id, limit);
        (messages, total, truncated, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_name_guard_rejects_path_traversal() {
        assert!(is_safe_archive_name("echo-sessions-x-20261109-022842-820-precompact.json"));
        assert!(!is_safe_archive_name("../etc/passwd"));
        assert!(!is_safe_archive_name("a/b.json"));
        assert!(!is_safe_archive_name("compaction.json.bak"));
        assert!(!is_safe_archive_name(""));
    }

    #[test]
    fn created_at_parses_filename_stamp_and_reason() {
        let stem = "echo-sessions-EchoCode";
        let (ts, reason) =
            created_at_of("echo-sessions-EchoCode-20261109-022842-820-precompact.json", stem);
        assert!(ts > 0, "时间戳应解析成功: {ts}");
        assert_eq!(reason, "precompact");

        // 无 reason、带毫秒
        let (ts2, reason2) =
            created_at_of("echo-sessions-EchoCode-20261104-035520-063.json", stem);
        assert!(ts2 > 0);
        assert_eq!(reason2, "");

        // 日期段非法 → 0（不猜）
        let (ts3, _) = created_at_of("echo-sessions-EchoCode-XXXX0403-035520.json", stem);
        assert_eq!(ts3, 0);
    }
}
