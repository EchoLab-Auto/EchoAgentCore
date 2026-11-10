//! 压缩前归档的**读取与切片**（2026-11）。
//!
//! ## 为什么需要
//!
//! 压缩（`apply_compaction`）不是给日志加个标记就完事：它把被替换的前缀事件
//! **从日志里摘掉**，换成一条 `Compaction` 事件（带摘要 + 归档路径）。于是
//! "压缩前到底聊了什么"在投影、显示时间线、面板里都**看不见了**——只有
//! `archives/` 里那份快照还留着全量事件日志。用户要能回看，就得从这里读。
//!
//! ## 这里只做 I/O，不做投影
//!
//! 本模块的职责是"把归档里某会话的前 N 条事件读出来"，投影交给
//! [`super::history::display_history_extended`]——它与在线历史共用同一批展示
//! 规则（工具概要截断 / 定时信封 / 单行折叠），所以归档里看到的形状和在线
//! 看到的一致。这条分工也让派生逻辑可以单测（喂假事件即可，不必造 20 MB
//! JSON）。
//!
//! ## 切片为什么按**事件**数
//!
//! `CompactionEvent.replaced_count` 是**按会话数的事件数**：一次工具调用 +
//! 它的结果 = 2 条事件、派生后只有 1 行。所以边界只能按事件切，"取前 N 条
//! 消息"会切歪。
//!
//! ## 安全边界
//!
//! 只接受**文件名**：路径分隔符、`..` 一律拒绝，且必须能在本团队
//! `archives/` 目录里找到。归档是只读的事实记录，不存在"写回"路径。

use std::sync::{Arc, Mutex};

use echo_session::SessionEvent;

use super::TrunkStore;

/// 归档解析缓存条目上限。
///
/// 一份归档 18–23 MB，解析一次 1–3 s，而同一层会被反复请求（展开→收起→
/// 再展开、分页都会重走派生）。缓存**整份会话事件**（不是投影结果，因为
/// 不同请求的 `count` 可能不同）。一层事件带推理全文可达数 MB，所以只留
/// 最近 4 份——再多是白占内存。
const ARCHIVE_CACHE_ENTRIES: usize = 4;

/// 缓存键：路径 + mtime + 大小 + 会话 + 条数。
///
/// 带上 mtime/大小是为了归档被轮转后同名文件（理论上不会，但轮转策略可能
/// 变成复用文件名）不会被旧缓存命中。
type ArchiveCacheKey = (String, i64, u64, String, usize);

static ARCHIVE_CACHE: Mutex<Vec<(ArchiveCacheKey, Arc<Vec<SessionEvent>>)>> =
    Mutex::new(Vec::new());

/// 归档文件名合法性（只允许本模块自己生成的那种名字）。
fn is_safe_archive_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 256
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && name.ends_with(".json")
}

/// 归档文档里我们关心的部分（其余字段——header / identities / timeline /
/// version——反序列化时忽略）。
#[derive(serde::Deserialize)]
struct ArchiveDocument {
    #[serde(default)]
    events: Vec<SessionEvent>,
}

impl TrunkStore {
    /// 本团队的 `archives/` 目录（`None` = 未启用持久化 / 尚无目录）。
    pub fn archives_dir(&self) -> Option<std::path::PathBuf> {
        let path = self.persist_path()?;
        let dir = path.parent()?.join("archives");
        dir.is_dir().then_some(dir)
    }

    /// 读 `archive` 里 `session_id` 的**前** `count` 条事件（压缩边界切分）。
    ///
    /// 返回 `(events, error)`；`error` 非空时 `events` 为空（面板据此显示
    /// "归档已清理"而不是空列表）。读取/解析失败不 panic——归档是可选的历史，
    /// 坏一份不该让整个历史请求失败。
    ///
    /// 阻塞 I/O + CPU（解析 20 MB JSON），调用方应放到 `spawn_blocking`。
    pub fn archive_prefix_events(
        &self,
        archive: &str,
        session_id: &str,
        count: usize,
    ) -> (Vec<SessionEvent>, Option<String>) {
        if !is_safe_archive_name(archive) {
            return (Vec::new(), Some(format!("归档名不合法：{archive}")));
        }
        let Some(dir) = self.archives_dir() else {
            return (
                Vec::new(),
                Some("该 persona 没有归档目录（未启用持久化？）".into()),
            );
        };
        let path = dir.join(archive);
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                // 归档有**保留上限**（`DEFAULT_ARCHIVE_MAX_FILES`，20 份）：
                // 压缩越频繁，越早的归档越可能已被清理。这条提示要让人知道
                // "不是坏了，是被轮转掉了"。
                let hint = if error.kind() == std::io::ErrorKind::NotFound {
                    "该归档已被清理（archives 只保留最近 20 份），更早的压缩历史不再可读"
                } else {
                    "读取归档失败"
                };
                return (Vec::new(), Some(format!("{hint}：{error}")));
            }
        };
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|delta| delta.as_secs() as i64)
            .unwrap_or(0);
        let key: ArchiveCacheKey = (
            path.to_string_lossy().into_owned(),
            mtime,
            metadata.len(),
            session_id.to_string(),
            count,
        );
        if let Some(hit) = cache_lookup(&key) {
            return ((*hit).clone(), None);
        }

        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) => return (Vec::new(), Some(format!("读取归档失败：{error}"))),
        };
        // 直接反序列化成事件数组（不经 `serde_json::Value` 中转：20 MB 文档
        // 走 Value 会多一份完整语法树，峰值内存翻倍）。
        let document: ArchiveDocument = match serde_json::from_str(&raw) {
            Ok(document) => document,
            Err(error) => {
                return (
                    Vec::new(),
                    Some(format!("归档不是合法的事件文档：{error}")),
                )
            }
        };
        let events: Vec<SessionEvent> = document
            .events
            .into_iter()
            .filter(|event| event.session() == Some(session_id))
            .take(count)
            .collect();
        let events = Arc::new(events);
        cache_store(key, events.clone());
        ((*events).clone(), None)
    }
}

/// 取缓存（命中即提到队尾 = LRU 刷新）。
fn cache_lookup(key: &ArchiveCacheKey) -> Option<Arc<Vec<SessionEvent>>> {
    let mut cache = ARCHIVE_CACHE.lock().ok()?;
    let position = cache.iter().position(|(k, _)| k == key)?;
    let entry = cache.remove(position);
    let value = entry.1.clone();
    cache.push(entry);
    Some(value)
}

/// 写缓存（超出上限丢队首 = 最久未用）。
fn cache_store(key: ArchiveCacheKey, events: Arc<Vec<SessionEvent>>) {
    let Ok(mut cache) = ARCHIVE_CACHE.lock() else {
        return;
    };
    cache.retain(|(k, _)| k != &key);
    cache.push((key, events));
    while cache.len() > ARCHIVE_CACHE_ENTRIES {
        cache.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_name_guard_rejects_path_traversal() {
        assert!(is_safe_archive_name(
            "echo-sessions-x-20261109-022842-820-precompact.json"
        ));
        assert!(!is_safe_archive_name("../etc/passwd"));
        assert!(!is_safe_archive_name("a/b.json"));
        assert!(!is_safe_archive_name("compaction.json.bak"));
        assert!(!is_safe_archive_name(""));
    }

    /// 缓存命中返回同一份内容，且不会因为超出上限而丢最新的。
    #[test]
    fn cache_is_lru_and_bounded() {
        let make = |n: usize| (format!("/tmp/a{n}.json"), 1, 1, format!("s{n}"), n);
        for i in 0..(ARCHIVE_CACHE_ENTRIES + 2) {
            cache_store(make(i), Arc::new(Vec::new()));
        }
        let cache = ARCHIVE_CACHE.lock().unwrap();
        assert_eq!(cache.len(), ARCHIVE_CACHE_ENTRIES);
        // 最早的几份被淘汰，最后一份还在。
        assert!(!cache.iter().any(|(k, _)| k == &make(0)));
        assert!(cache
            .iter()
            .any(|(k, _)| k == &make(ARCHIVE_CACHE_ENTRIES + 1)));
    }
}
