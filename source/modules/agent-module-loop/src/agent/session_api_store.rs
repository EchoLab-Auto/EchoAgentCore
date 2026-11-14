//! 会话级 API 覆盖的**持久化**（`session_id → profile`）。
//!
//! ## 为什么需要它
//!
//! Core 的路由依据一直是内存表 `session_providers`（见
//! [`crate::model_service::ModelService`]）。它有两个缺口，都只影响非工作区
//! 通道：
//!
//! | 通道 | 覆盖存哪 | 重启后 |
//! |---|---|---|
//! | `local:workspace:*` | 工作区模块的对话文档（`api_profile` 字段） | 还在 |
//! | TUI / QQ / shell … | 只有内存表 | **没了** |
//!
//! 于是 Panel 自己用 localStorage 记了一份并重放——第二份真相：两个浏览器
//! 同时开，A 设 deepseek、B 拿旧缓存回放 kimi，谁也没错，而 Core 的路由只可能
//! 是其中一个。
//!
//! 本模块把持久化收归 Core：**所有通道**的覆盖写同一个旁路文件，Panel 只读
//! （`BackendEvent::SessionApiOverrides`）。
//!
//! 下发时是**进程级快照**（跨全部人格合并，`team_id: None`，
//! 见 [`merge_overrides`]）——因为 Panel 对这条事件是「整体替换」语义，
//! 发局部切片会让刷新后的界面把整张表替成空表。
//!
//! ## 为什么是独立文件而不是塞进 `echo-sessions-{id}.json`
//!
//! 会话文件是**事件溯源**格式（append-only 事件日志 + 派生视图），加一个可变
//! 字段要么破格式、要么写迁移。而覆盖是"少量、可变、与事件历史无关"的键值对
//! ——独立小文件最简单，也最容易在出问题时手工修。
//!
//! 文件位置由调用方给（`core` 组合根按 `echo-session-api-{persona}.json` 派生，
//! 与会话文件同目录）。**未注入路径 = 纯内存**，与 `TrunkStore` 的
//! `persist_path` 同一约定——测试与嵌入式用法不必碰磁盘。
//!
//! ## 写盘策略
//!
//! 「临时文件 + 原子 rename」。直接覆写有两个真实风险：进程在写入中途被杀会
//! 留下半截 JSON（下次启动全部覆盖丢失，且没有任何提示）；并发读端可能读到
//! 半截内容。原子 rename 让"文件存在"恒等于"内容完整"。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use agent_module_management::protocol::SessionApiOverrideEntry;

/// 磁盘格式：`{ session_id: profile }`。
///
/// `BTreeMap` 而不是 `HashMap`：写盘结果与内存顺序一致，`git diff` 与人工
/// 排障时不会因为迭代顺序随机而看到无意义的抖动。
type Entries = BTreeMap<String, String>;

pub struct SessionApiStore {
    /// 持久化文件（`None` = 纯内存）。可**晚绑定**：组合根在 `Agent` 构造之后
    /// 才拿到数据目录，而测试与嵌入式用法永远不绑定。
    path: RwLock<Option<PathBuf>>,
    entries: RwLock<Entries>,
}

impl std::fmt::Debug for SessionApiStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionApiStore")
            .field("path", &self.path.read().ok().and_then(|p| p.clone()))
            .field("entries", &self.entries.read().map(|e| e.len()).unwrap_or(0))
            .finish()
    }
}

impl SessionApiStore {
    /// 纯内存（测试 / 未注入路径）。
    pub fn in_memory() -> Self {
        Self {
            path: RwLock::new(None),
            entries: RwLock::new(Entries::new()),
        }
    }

    /// 从 `path` 打开：文件不存在 = 空表（首次启动的正常情形），不报错。
    ///
    /// 内容损坏时**记 warn 后按空表继续**，而不是让 Core 起不来：这份文件只是
    /// "上次的偏好"，丢了会退回 persona 默认配置，不影响任何数据正确性。相反的
    /// 选择（启动失败）会让一个坏字符挡住整个服务。
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let entries = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Entries>(&text) {
                Ok(map) => map,
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "会话级 API 覆盖文件内容损坏，按空表继续（各会话回落到 persona 配置）"
                    );
                    Entries::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Entries::new(),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "读不到会话级 API 覆盖文件，按空表继续"
                );
                Entries::new()
            }
        };
        Self {
            path: RwLock::new(Some(path)),
            entries: RwLock::new(entries),
        }
    }

    /// 晚绑定持久化文件。
    ///
    /// 磁盘内容与**已有的内存条目合并**，内存条目优先：装配顺序是
    /// `Agent::new`（空表）→ 可能被测试注入几条 → 组合根接线。文件里有的是
    /// "上次进程的意图"，内存里有的是"本次进程已经做出的决定"，后者更新。
    pub fn attach(&self, path: impl Into<PathBuf>) {
        let path = path.into();
        let loaded = Self::open(&path);
        let merged = loaded.snapshot();
        *self
            .path
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
        for entry in merged {
            // 只补内存里没有的：已有条目代表本次进程已做出的决定。
            if self.lock_read().contains_key(&entry.session_id) {
                continue;
            }
            self.lock_write()
                .insert(entry.session_id, entry.profile);
        }
        self.flush();
    }

    /// 当前文件路径（`None` = 纯内存；测试与诊断用）。
    pub fn path(&self) -> Option<PathBuf> {
        self.path
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 某会话的覆盖（`None` = 跟随 persona 级）。
    pub fn get(&self, session_id: &str) -> Option<String> {
        self.lock_read().get(session_id).cloned()
    }

    /// 全部覆盖（名称排序，供 `SessionApiOverrides` 事件）。
    pub fn snapshot(&self) -> Vec<SessionApiOverrideEntry> {
        self.lock_read()
            .iter()
            .map(|(session_id, profile)| SessionApiOverrideEntry {
                session_id: session_id.clone(),
                profile: profile.clone(),
            })
            .collect()
    }

    /// 设一条覆盖（`profile` 空串 = 清除）。返回是否真的变了。
    ///
    /// 变化才写盘：`SetSessionApi` 与「已是目标值」的幂等路径都会走到这里，
    /// 每次点击都重写文件既无意义也会让 mtime 抖动。
    pub fn set(&self, session_id: &str, profile: &str) -> bool {
        let changed = {
            let mut entries = self.lock_write();
            if profile.is_empty() {
                entries.remove(session_id).is_some()
            } else if entries.get(session_id).map(String::as_str) == Some(profile) {
                false
            } else {
                entries.insert(session_id.to_string(), profile.to_string());
                true
            }
        };
        if changed {
            self.flush();
        }
        changed
    }

    /// 清除某会话的覆盖。返回是否确有其事。
    pub fn clear(&self, session_id: &str) -> bool {
        self.set(session_id, "")
    }

    /// 把当前内存表写回磁盘（原子替换）。
    ///
    /// 失败只记 warn：覆盖是偏好而不是数据，一次写盘失败不该让命令调用方看到
    /// 错误、更不该让 turn 中断——本次会话的路由仍然生效（内存表已更新）。
    fn flush(&self) {
        let Some(path) = self.path() else {
            return;
        };
        let path = path.as_path();
        let payload = {
            let entries = self.lock_read();
            match serde_json::to_vec_pretty(&*entries) {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::warn!(%error, "会话级 API 覆盖序列化失败");
                    return;
                }
            }
        };
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                tracing::warn!(dir = %parent.display(), %error, "创建目录失败");
                return;
            }
        }
        let tmp = path.with_extension("json.tmp");
        if let Err(error) = std::fs::write(&tmp, &payload) {
            tracing::warn!(path = %tmp.display(), %error, "写临时文件失败");
            return;
        }
        if let Err(error) = std::fs::rename(&tmp, path) {
            tracing::warn!(path = %path.display(), %error, "原子替换失败");
            let _ = std::fs::remove_file(&tmp);
        }
    }

    fn lock_read(&self) -> std::sync::RwLockReadGuard<'_, Entries> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_write(&self) -> std::sync::RwLockWriteGuard<'_, Entries> {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// 把多个人格的覆盖合并成**一份进程级快照**（`SessionApiOverrides` 的载荷）。
///
/// # 为什么必须合并
///
/// Panel 对这条事件采取「整体替换」语义（某会话不再出现 = 它已经没有覆盖），
/// 而 Core 过去只发**发命令那个 agent 自己的**切片。两者对不上时，面板刷新
/// 拿到的是一份局部表：其他人格的条目、连同当前会话刚设的那条一起消失，表现
/// 为「换完会话模型一刷新就变回去」（2026-11 用户报障）。更糟的是处理
/// `RequestState` 的那个 agent 恰好没有 team_id 也没有任何覆盖，于是刷新 = 清空。
///
/// 合并后的事件是**进程级**的（`team_id: None`），与 Panel「缺省 = 全进程」
/// 的判断一致，也与路由真源（每个 agent 各自的内存表）一致。
///
/// # 冲突
///
/// 两个 agent 对同一个 session_id 各有覆盖：保留先到的并记 warn。同一对话
/// 同时属于两个人格本不该发生；为此让整份快照报错，界面只会更糟。
pub fn merge_overrides(
    slices: impl IntoIterator<Item = (String, Vec<SessionApiOverrideEntry>)>,
) -> Vec<SessionApiOverrideEntry> {
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for (owner, entries) in slices {
        for entry in entries {
            match merged.get(&entry.session_id) {
                Some(existing) if existing != &entry.profile => tracing::warn!(
                    owner = %owner,
                    session = %entry.session_id,
                    kept = %existing,
                    ignored = %entry.profile,
                    "同一会话在两个人格下各有 API 覆盖，保留先到的"
                ),
                Some(_) => {}
                None => {
                    merged.insert(entry.session_id, entry.profile);
                }
            }
        }
    }
    merged
        .into_iter()
        .map(|(session_id, profile)| SessionApiOverrideEntry {
            session_id,
            profile,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "echo-session-api-{}-{tag}.json",
            std::process::id()
        ))
    }

    #[test]
    fn set_get_clear_roundtrip_in_memory() {
        let store = SessionApiStore::in_memory();
        assert!(store.get("s1").is_none());
        assert!(store.set("s1", "kimi"));
        assert_eq!(store.get("s1").as_deref(), Some("kimi"));
        // 幂等：同值不算变化
        assert!(!store.set("s1", "kimi"));
        assert!(store.clear("s1"));
        assert!(store.get("s1").is_none());
        // 清一个不存在的键
        assert!(!store.clear("s1"));
    }

    #[test]
    fn snapshot_is_sorted_by_session_id() {
        let store = SessionApiStore::in_memory();
        store.set("b", "p1");
        store.set("a", "p2");
        let snapshot = store.snapshot();
        assert_eq!(
            snapshot
                .iter()
                .map(|e| e.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"],
            "快照顺序必须稳定（BTreeMap），否则每次事件内容都在抖"
        );
    }

    #[test]
    fn survives_reopen() {
        let path = temp_path("reopen");
        let _ = std::fs::remove_file(&path);
        {
            let store = SessionApiStore::open(&path);
            store.set("local:tui::u", "deepseek");
            store.set("local:workspace:ws::u", "kimi");
        }
        let reopened = SessionApiStore::open(&path);
        assert_eq!(
            reopened.get("local:tui::u").as_deref(),
            Some("deepseek"),
            "非工作区通道的覆盖必须跨重启存活（这正是本模块存在的理由）"
        );
        assert_eq!(
            reopened.get("local:workspace:ws::u").as_deref(),
            Some("kimi")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_degrades_to_empty_instead_of_failing() {
        let path = temp_path("corrupt");
        std::fs::write(&path, b"{ not json").unwrap();
        let store = SessionApiStore::open(&path);
        assert!(
            store.get("any").is_none(),
            "内容损坏时按空表继续——一次坏字符不该挡住整个服务启动"
        );
        // 仍然可用
        store.set("s1", "kimi");
        assert_eq!(store.get("s1").as_deref(), Some("kimi"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        let store = SessionApiStore::open(&path);
        assert!(store.snapshot().is_empty());
    }

    /// 合并必须**跨人格**：只取当前 agent 的切片会让面板刷新后看到空表。
    ///
    /// 这正是用户报的「换完会话模型一刷新就变回去」——处理 `RequestState` 的
    /// agent 自己没有任何覆盖，于是整张表被替成空的。
    #[test]
    fn merge_overrides_keeps_entries_from_every_persona() {
        let merged = merge_overrides(vec![
            (
                "EchoCode".to_string(),
                vec![SessionApiOverrideEntry {
                    session_id: "local:workspace:echo-agent/v:local_user".into(),
                    profile: "opencode".into(),
                }],
            ),
            (
                "Alice".to_string(),
                vec![SessionApiOverrideEntry {
                    session_id: "local:tui::local_user".into(),
                    profile: "kimi".into(),
                }],
            ),
            ("__core".to_string(), Vec::new()),
        ]);
        assert_eq!(merged.len(), 2, "两个人格的条目都要在：{merged:?}");
        // 名称排序（BTreeMap）：输出稳定，便于 diff 与人工核对。
        assert_eq!(merged[0].session_id, "local:tui::local_user");
        assert_eq!(merged[1].session_id, "local:workspace:echo-agent/v:local_user");
    }

    /// 同一会话被两个人格各设一次：保留第一个，不丢整张表。
    #[test]
    fn merge_overrides_keeps_first_on_conflict() {
        let merged = merge_overrides(vec![
            (
                "A".to_string(),
                vec![SessionApiOverrideEntry {
                    session_id: "s".into(),
                    profile: "one".into(),
                }],
            ),
            (
                "B".to_string(),
                vec![SessionApiOverrideEntry {
                    session_id: "s".into(),
                    profile: "two".into(),
                }],
            ),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].profile, "one");
    }

}
