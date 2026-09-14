//! Multi-session conversation contexts with per-source identities.
//!
//! 每个来源（本地 TUI / QQ 私聊 / QQ 群 / …）是一个 `Session`，拥有**独立
//! 的模型上下文**（按事件归属从同一份 append-only 事件日志投影而来，
//! 2026-09 多会话改造）。消息到达时从该会话上下文的时点快照 fork 一条临时
//! 分支；分支结束后按序合并回事件日志（见
//! [`crate::agent::Agent::process_message`]）。
//!
//! 展示时间线（timeline）仍是全部会话的合并视图（条目自带 session 归属），
//! 由前端按当前选中会话过滤。

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::event::SessionInfo;
use crate::llm::{estimate_history_tokens, estimate_message_tokens, ChatMessage};

/// 默认 QQ 实例名（legacy 单实例）：作为会话 id 后缀被省略。
pub const DEFAULT_QQ_INSTANCE: &str = "qq";

/// Structured session identity, replacing the old `user_{qq_number}` format.
///
/// ```text
/// format: {platform}:{scope}:{scope_id}:{user_id}[@{account}]
///
/// Examples:
///   qq:dm::123456           — QQ direct message, user 123456
///   qq:group:987654:123456  — QQ group 987654, user 123456
///   qq:dm::123456@alix-2    — 同上，但来自 QQ 实例 "alix-2"（多实例维度）
///   local:tui::local_user   — local TUI interaction
/// ```
///
/// `account` 是 **QQ 实例名**（`[adapters.qq.instances.<id>]`）。单实例
/// （实例名恰为 `qq`）时不产生后缀，历史会话 id 全部保持有效。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    /// Platform (e.g., "qq", "local").
    pub platform: String,
    /// Scope: "dm", "group", "tui".
    pub scope: String,
    /// Scope identifier (group_id for groups, empty for dm).
    pub scope_id: String,
    /// User identifier (platform-specific user ID as string).
    pub user_id: String,
    /// QQ 实例名（多实例维度；None/`qq` = 默认单实例，不写入 id）。
    pub account: Option<String>,
}

impl SessionKey {
    /// Build the canonical session ID string.
    ///
    /// 多实例（account 非 `qq`）时以 `@{account}` 结尾，与旧格式天然区分：
    /// 单实例部署的既有会话 id 一字不变。
    pub fn to_session_id(&self) -> String {
        let base = format!(
            "{}:{}:{}:{}",
            self.platform, self.scope, self.scope_id, self.user_id
        );
        match self.account.as_deref() {
            Some(account) if !account.is_empty() && account != DEFAULT_QQ_INSTANCE => {
                format!("{base}@{account}")
            }
            _ => base,
        }
    }

    /// 实例名（None/默认实例 → None）。
    pub fn account(&self) -> Option<&str> {
        self.account
            .as_deref()
            .filter(|a| !a.is_empty() && *a != DEFAULT_QQ_INSTANCE)
    }

    /// Create a session key for local TUI use.
    pub fn local_tui() -> Self {
        SessionKey {
            platform: "local".into(),
            scope: "tui".into(),
            scope_id: String::new(),
            user_id: "local_user".into(),
            account: None,
        }
    }

    /// Parse a session ID string back into a key. Returns `None` on legacy
    /// `user_{id}` format (falls back to `local:tui:`) or malformed strings.
    pub fn parse(session_id: &str) -> Option<Self> {
        // Legacy format: user_{digits}
        if session_id.starts_with("user_") {
            let user_id = session_id.strip_prefix("user_")?;
            return Some(SessionKey {
                platform: "local".into(),
                scope: "tui".into(),
                scope_id: String::new(),
                user_id: user_id.to_string(),
                account: None,
            });
        }
        let parts: Vec<&str> = session_id.splitn(4, ':').collect();
        if parts.len() != 4 {
            return None;
        }
        // 多实例维度：user_id 尾部 `@<account>`（默认实例不写后缀）。
        let (user_id, account) = match parts[3].split_once('@') {
            Some((user, account)) if !account.is_empty() => (user, Some(account.to_string())),
            _ => (parts[3], None),
        };
        Some(SessionKey {
            platform: parts[0].to_string(),
            scope: parts[1].to_string(),
            scope_id: parts[2].to_string(),
            user_id: user_id.to_string(),
            account,
        })
    }
}

/// A source identity with its **own conversation context**（多会话, 2026-09）.
///
/// Each session (local TUI / QQ DM / QQ group / …) owns an independent
/// model-facing history projected from the shared append-only event log
/// (events carry a `session` attribution). Sessions share only the display
/// timeline and the write locks; contexts never bleed across sources.
/// Cloneable: clones share the same history and activity clock.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    /// Owning team id (None = default/legacy).
    pub team_id: Option<String>,
    pub session_key: SessionKey,
    pub nickname: String,
    pub group_name: Option<String>,
    last_active: Arc<AtomicI64>,
    /// 本会话的模型上下文投影（由事件日志按 `session` 归属过滤导出，
    /// 受 token 预算 `memory_limit_tokens` 约束）。
    pub history: Arc<Mutex<Vec<ChatMessage>>>,
    /// Serialises trunk snapshot creation and result merges; branch execution never holds this lock.
    pub turn_lock: Arc<tokio::sync::Mutex<()>>,
    /// 单会话模式的 turn 排队闸门（FIFO，tokio Mutex 保证公平）：
    /// 同一会话同一时刻只跑一个 turn，其余 turn 在获得许可后才取上下文
    /// 快照（因此排队的 turn 能看到前一轮的回复）。并行多会话模式下不使
    /// 用（`Agent::loop_mode` = `Parallel`）。按会话共享。
    pub turn_queue: Arc<tokio::sync::Mutex<()>>,
}

impl Session {
    fn with_trunk(
        key: &SessionKey,
        team_id: Option<String>,
        nickname: String,
        group_name: Option<String>,
        history: Arc<Mutex<Vec<ChatMessage>>>,
        turn_lock: Arc<tokio::sync::Mutex<()>>,
        turn_queue: Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        Self {
            id: key.to_session_id(),
            team_id,
            session_key: key.clone(),
            nickname,
            group_name,
            last_active: Arc::new(AtomicI64::new(chrono::Utc::now().timestamp())),
            history,
            turn_lock,
            turn_queue,
        }
    }

    /// Record activity (shared across clones).
    pub fn touch(&self) {
        self.last_active
            .store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
    }

    pub fn last_active(&self) -> i64 {
        self.last_active.load(Ordering::Relaxed)
    }

    pub fn info(&self, last_message: String) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            team_id: self.team_id.clone(),
            platform: self.session_key.platform.clone(),
            scope: self.session_key.scope.clone(),
            user_id: self.session_key.user_id.clone(),
            nickname: self.nickname.clone(),
            group_name: self.group_name.clone(),
            last_active: self.last_active(),
            last_message,
        }
    }
}

/// Registry of source identities with per-session conversation contexts.
pub struct TrunkStore {
    /// Owning team id (None = default). Set once by the Agent.
    team_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    identities: Arc<DashMap<String, Session>>,
    memory_limit_tokens: usize,
    /// 每会话的上下文投影缓存（session_id → history）。事件日志是唯一
    /// 事实来源，这里只是它的按会话投影（见 `reproject_session`）。
    session_histories: Arc<DashMap<String, Arc<Mutex<Vec<ChatMessage>>>>>,
    trunk_turn_lock: Arc<tokio::sync::Mutex<()>>,
    /// 单会话模式的 turn 排队闸门（见 [`Session::turn_queue`]）。
    trunk_turn_queue: Arc<tokio::sync::Mutex<()>>,
    /// Display timeline (persisted, survives TUI restarts). Independent of the
    /// token-bounded LLM trunk: bounded by entry count, keeps richer metadata
    /// (source provenance, tool calls, reasoning) for the TUI history view.
    timeline: Arc<Mutex<Vec<crate::event::TimelineMessage>>>,
    /// Timeline 单调序号：每次新增/更新条目递增。前端用它做增量同步
    ///（RequestTrunkTimeline.since_seq + TrunkTimeline.seq），避免切换
    /// agent 时全量重传。
    timeline_seq: Arc<std::sync::atomic::AtomicU64>,
    /// The append-only session event log — the **single source of truth** for
    /// the model-facing context. `trunk_history` is its in-memory projection
    /// cache (see `append_event`); persistence writes this log, and older
    /// formats migrate into it (dsh "model-visible means logged").
    event_log: echo_session::EventLog,
    /// Fork/resume metadata (lineage, origin, delegation depth). The global
    /// trunk model keeps one header for the store.
    header: std::sync::Mutex<Option<echo_session::SessionHeader>>,
    persist_path: std::sync::Mutex<Option<std::path::PathBuf>>,
    dirty: std::sync::atomic::AtomicBool,
}

impl Clone for TrunkStore {
    fn clone(&self) -> Self {
        Self {
            team_id: Arc::clone(&self.team_id),
            identities: Arc::clone(&self.identities),
            memory_limit_tokens: self.memory_limit_tokens,
            session_histories: Arc::clone(&self.session_histories),
            trunk_turn_lock: Arc::clone(&self.trunk_turn_lock),
            trunk_turn_queue: Arc::clone(&self.trunk_turn_queue),
            timeline: Arc::clone(&self.timeline),
            timeline_seq: Arc::clone(&self.timeline_seq),
            event_log: self.event_log.clone(),
            header: std::sync::Mutex::new(self.header.lock().expect("header poisoned").clone()),
            persist_path: std::sync::Mutex::new(
                self.persist_path.lock().expect("poisoned").clone(),
            ),
            dirty: std::sync::atomic::AtomicBool::new(
                self.dirty.load(std::sync::atomic::Ordering::Relaxed),
            ),
        }
    }
}

impl std::fmt::Debug for TrunkStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrunkStore")
            .field("identities", &self.len())
            .field("trunk_len", &self.trunk_len())
            .finish()
    }
}

/// Identity idle TTL before eviction (24 hours, in milliseconds). Only the
/// source label is evicted; the trunk history is never touched.
const IDENTITY_IDLE_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Persistence format version.
///
/// - v5 = event-sourced: the session event log is the source of truth;
///   `trunk_history`/`identities`/`timeline` are projections.
/// - v6（2026-09）= **multi-session contexts**: every event carries a
///   `session` attribution; per-session projections are persisted as
///   `trunk_histories` (map) instead of the old flat `trunk_history`.
///   v5 files load fine — events are attributed on load (migration) and the
///   next save writes v6.
const PERSIST_VERSION: u32 = 6;

/// Upper bound on persisted display timeline entries.
const TRUNK_TIMELINE_MAX: usize = 1024;

impl TrunkStore {
    /// Create a store owning the single global conversation trunk.
    pub fn new(memory_limit_tokens: usize) -> Self {
        Self {
            team_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            identities: Arc::new(DashMap::new()),
            memory_limit_tokens,
            session_histories: Arc::new(DashMap::new()),
            trunk_turn_lock: Arc::new(tokio::sync::Mutex::new(())),
            trunk_turn_queue: Arc::new(tokio::sync::Mutex::new(())),
            timeline: Arc::new(Mutex::new(Vec::new())),
            timeline_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            event_log: echo_session::EventLog::new(),
            header: std::sync::Mutex::new(None),
            persist_path: std::sync::Mutex::new(None),
            dirty: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Set the file path for session persistence.
    pub fn set_persist_path(&self, path: impl Into<std::path::PathBuf>) {
        *self.persist_path.lock().expect("persist path poisoned") = Some(path.into());
    }

    /// Mark sessions as needing save.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Append one entry to the display timeline, dropping the oldest entries
    /// beyond the cap. Marks the store dirty so the entry is persisted.
    ///
    /// 条目 seq 在锁内赋值并与计数器同步推进（仅在条目真正入列时递增），
    /// 保证 `timeline_snapshot_since` 的按 seq 过滤语义成立。
    pub fn push_timeline(&self, mut message: crate::event::TimelineMessage) {
        let mut timeline = match self.timeline.try_lock() {
            Ok(guard) => guard,
            // A concurrent save/record is holding the lock; skip rather than
            // block the event loop (emit runs on the hot path).
            Err(_) => return,
        };
        message.seq = self
            .timeline_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        timeline.push(message);
        if timeline.len() > TRUNK_TIMELINE_MAX {
            let excess = timeline.len() - TRUNK_TIMELINE_MAX;
            timeline.drain(..excess);
        }
        drop(timeline);
        self.mark_dirty();
    }

    /// 推进 timeline 序号并返回新值（就地更新场景：条目内容变化但位置
    /// 不变，调用方把返回的 seq 写到被更新的条目上，增量同步即可重新
    /// 投递该条目）。
    pub fn bump_timeline_seq(&self) -> u64 {
        self.timeline_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    /// Point-in-time snapshot of the display timeline (oldest first).
    pub fn timeline_snapshot(&self) -> Vec<crate::event::TimelineMessage> {
        self.timeline
            .try_lock()
            .map(|timeline| timeline.clone())
            .unwrap_or_default()
    }

    /// 当前 timeline 单调序号（增量同步游标）。
    pub fn timeline_seq(&self) -> u64 {
        self.timeline_seq.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 增量快照：返回 seq > since_seq 的条目 + 最新 seq。
    /// `since_seq == 0` 时为全量快照。注意：timeline 本身有 1024 条目上限，
    /// 若增量窗口内的条目已被滚出（since_seq 落后太多），返回 None 表示
    /// 无法增量对齐，调用方应回退全量。`since_seq > current`（前端游标来自
    /// 本 core 重启之前）同样返回 None——序号在重启后重新从持久化条目
    /// 重建，无法与旧游标对齐。
    pub fn timeline_snapshot_since(
        &self,
        since_seq: u64,
    ) -> Option<(Vec<crate::event::TimelineMessage>, u64)> {
        let current = self.timeline_seq();
        if since_seq > current {
            // 游标超前于当前序号（如对端缓存自 core 重启前）：无法对齐。
            return None;
        }
        if since_seq == current {
            return Some((Vec::new(), current));
        }
        let timeline = self.timeline.try_lock().ok()?;
        // 缺口检测：淘汰只发生在头部；若现存最老条目的 seq 已越过
        // since_seq + 1，说明窗口内有条目被滚出，增量不再完整。
        // （seq == 0 的旧版/重启前条目不参与缺口判断，直接按全量处理。）
        if since_seq > 0 {
            match timeline.first() {
                Some(oldest) if oldest.seq > 0 && oldest.seq > since_seq + 1 => return None,
                Some(oldest) if oldest.seq == 0 => return None,
                None => return None,
                _ => {}
            }
        }
        let messages: Vec<_> = timeline
            .iter()
            .filter(|m| m.seq > since_seq)
            .cloned()
            .collect();
        Some((messages, current))
    }

    /// Mutable access to the display timeline for in-place updates (e.g.
    /// attaching a tool result to its running entry). Returns `None` when the
    /// lock is contended — callers must then fall back to an append.
    pub fn timeline_mut(
        &self,
    ) -> Option<tokio::sync::MutexGuard<'_, Vec<crate::event::TimelineMessage>>> {
        self.timeline.try_lock().ok()
    }

    /// Save sessions to JSON file if dirty.
    pub async fn maybe_save(&self) {
        if !self.dirty.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.write_to_disk().await;
    }

    /// Force a save now, ignoring the dirty flag (used at shutdown).
    pub async fn save_now(&self) {
        self.write_to_disk().await;
    }

    /// Write sessions to disk. On failure the dirty flag stays set so the
    /// periodic task retries; failures are logged, not silently dropped.
    async fn write_to_disk(&self) {
        let path = {
            self.persist_path
                .lock()
                .expect("persist path poisoned")
                .clone()
        };
        let Some(ref path) = path else {
            self.dirty
                .store(false, std::sync::atomic::Ordering::Relaxed);
            return;
        };
        let Some(data) = self.serialize() else {
            // Serialization failure — nothing was written, retry next tick.
            return;
        };
        let tmp = format!("{}.tmp", path.display());
        if let Err(e) = tokio::fs::write(&tmp, &data).await {
            tracing::warn!(error = %e, path = %path.display(), "session save: tmp write failed, will retry");
            return;
        }
        if let Err(e) = tokio::fs::rename(&tmp, path).await {
            tracing::warn!(error = %e, path = %path.display(), "session save: rename failed, will retry");
            let _ = tokio::fs::remove_file(&tmp).await;
            return;
        }
        self.dirty
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Load sessions from JSON file. Returns count of restored sessions.
    pub async fn load_from_file(&self) -> usize {
        let path = {
            self.persist_path
                .lock()
                .expect("persist path poisoned")
                .clone()
        };
        let Some(ref path) = path else {
            return 0;
        };
        let data = match tokio::fs::read_to_string(path).await {
            Ok(d) => d,
            Err(_) => return 0,
        };
        self.deserialize(&data)
    }

    /// Serialize the trunk and identity labels. Returns `None` on failure so
    /// the caller can skip the write instead of silently replacing the file
    /// with garbage.
    fn serialize(&self) -> Option<String> {
        let timeline = self.timeline.try_lock().ok()?;
        let identities = self.all().iter().map(identity_metadata).collect::<Vec<_>>();
        let events = self.event_log.log();
        // 每会话投影（v6）：调试/兼容读者可读；事件日志仍是唯一事实来源。
        let mut trunk_histories = serde_json::Map::new();
        for entry in self.session_histories.iter() {
            if let Ok(history) = entry.value().try_lock() {
                trunk_histories.insert(
                    entry.key().clone(),
                    serde_json::Value::Array(serialize_messages(&history)),
                );
            }
        }
        let header = self.header.lock().expect("header poisoned").clone();
        serde_json::to_string_pretty(&serde_json::json!({
            "version": PERSIST_VERSION,
            // The event log is the source of truth; the projection fields are
            // persisted for forward compatibility with older readers.
            "header": header,
            "events": events,
            "trunk_histories": trunk_histories,
            "identities": identities,
            "timeline": &*timeline,
        }))
        .map_err(|error| {
            tracing::error!(%error, "trunk serialization failed, skipping save");
            error
        })
        .ok()
    }

    /// Restore a persisted file.
    ///
    /// v5 (event-sourced): the event log is the source of truth and re-derives
    /// the trunk projection. v4 and older (v1 per-session array, v2 shared
    /// context, v3 single trunk): the document is migrated into the event log
    /// (`echo_session::legacy`), preserving old-history compatibility. Returns
    /// the number of restored identities.
    fn deserialize(&self, data: &str) -> usize {
        let Ok(root) = serde_json::from_str::<serde_json::Value>(data) else {
            return 0;
        };

        // v5/v6: event log is authoritative. Re-project per session from it.
        if let Some(events) = root["events"].as_array() {
            if let Ok(header) = serde_json::from_value::<Option<echo_session::SessionHeader>>(
                root["header"].clone(),
            ) {
                *self.header.lock().expect("header poisoned") = header;
            }
            let mut events: Vec<echo_session::SessionEvent> = events
                .iter()
                .filter_map(|event| serde_json::from_value(event.clone()).ok())
                .collect();
            // 旧版（v5 及更早）事件没有会话归属：加载期归因迁移，否则
            // 多会话投影会把整段历史丢弃（或互相泄漏）。
            let attributed = attribute_legacy_events(&mut events);
            let version = root["version"].as_u64();
            if !events.is_empty() || version == Some(5) || version == Some(6) {
                self.event_log.extend(events.clone());
                if attributed {
                    tracing::info!(
                        events = events.len(),
                        "legacy session events attributed to per-session contexts"
                    );
                    self.mark_dirty();
                }
                let count = self.restore_identity_labels(&root);
                // 按会话重建投影（身份 + 事件中出现的所有会话）。
                self.ensure_histories_for_events(&events);
                self.reproject_all();
                let timeline = root["timeline"]
                    .as_array()
                    .map(|entries| {
                        entries
                            .iter()
                            .filter_map(|entry| {
                                serde_json::from_value::<crate::event::TimelineMessage>(
                                    entry.clone(),
                                )
                                .ok()
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                self.restore_timeline(timeline);
                return count;
            }
        }

        // v4 and older: migrate into the event log (compatibility read).
        if let Ok(migrated) = echo_session::legacy::migrate_v4_document(data) {
            let mut all_events: Vec<echo_session::SessionEvent> = Vec::new();
            let mut first_events: Option<Vec<echo_session::SessionEvent>> = None;
            for session in &migrated {
                // v2–v4 共享 trunk：每个身份携带同一份事件，只保留首份
                //（归首个身份）。v1 是每身份各自历史，逐个归入自身会话。
                let duplicate_shared = first_events
                    .as_ref()
                    .is_some_and(|first| *first == session.events);
                if duplicate_shared {
                    continue;
                }
                if first_events.is_none() {
                    first_events = Some(session.events.clone());
                }
                let mut events = session.events.clone();
                // 合成 "trunk"（无身份标签的早期档案）留给启发式归因。
                if session.session_id != "trunk" {
                    for event in events.iter_mut() {
                        if event.session().is_none() {
                            *event.session_mut() = Some(session.session_id.clone());
                        }
                    }
                }
                all_events.extend(events);
            }
            all_events.dedup();
            // 剩余未归因（合成 trunk 等）：归因迁移补全。
            let attributed = attribute_legacy_events(&mut all_events);
            if attributed {
                self.mark_dirty();
            }
            self.event_log.extend(all_events.clone());
            // Fall through to the identity/timeline restore below.
            let count = self.restore_identity_labels(&root);
            self.ensure_histories_for_events(&all_events);
            self.reproject_all();
            let restored_timeline = root["timeline"]
                .as_array()
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| {
                            serde_json::from_value::<crate::event::TimelineMessage>(entry.clone())
                                .ok()
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            self.restore_timeline(restored_timeline);
            return count;
        }
        self.deserialize_legacy(root)
    }

    /// Restore the persisted display timeline and rehydrate the seq counter
    /// from the highest entry seq. 旧版文件的条目没有 seq（反序列化为 0），
    /// 计数器保持为 0，新条目从 1 开始——此时增量同步的缺口检测会以
    /// 最老条目 seq == 0 为依据回退全量，语义仍然正确。
    ///
    /// 顺带清理僵尸 running 工具条目：进程被杀（如自更新重启）时
    /// run_tool 的 ToolResult 永远不会到达，条目 output 永远为 null，
    /// 前端会一直显示"运行中"。恢复时把这类条目标注为中断并分配新
    /// seq（增量同步会把完成态重新投递给已连接的前端）。
    fn restore_timeline(&self, timeline: Vec<crate::event::TimelineMessage>) {
        let mut max_seq = timeline.iter().map(|entry| entry.seq).max().unwrap_or(0);
        let mut timeline = timeline;
        let mut interrupted = 0usize;
        for entry in timeline.iter_mut() {
            if entry.kind != "tool" {
                continue;
            }
            let Some(tool) = entry.tool.as_mut() else {
                continue;
            };
            if tool.output.is_none() {
                tool.output = Some("[已中断] Core 服务重启导致本次调用未返回，可重试".into());
                tool.failed = true;
                max_seq += 1;
                entry.seq = max_seq;
                interrupted += 1;
            }
        }
        if interrupted > 0 {
            tracing::info!(
                interrupted,
                "dangling running tool entries marked interrupted on restore"
            );
            self.mark_dirty();
        }
        if let Ok(mut guard) = self.timeline.try_lock() {
            *guard = timeline;
        }
        self.timeline_seq
            .store(max_seq, std::sync::atomic::Ordering::Relaxed);
    }

    /// Restore identity labels from a v4/v5 document root.
    fn restore_identity_labels(&self, root: &serde_json::Value) -> usize {
        let identity_values = match root {
            serde_json::Value::Array(sessions) => sessions.as_slice(),
            serde_json::Value::Object(_) => root["identities"]
                .as_array()
                .or_else(|| root["sessions"].as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            _ => &[],
        };
        let mut count = 0;
        for s in identity_values {
            let Some(_id) = s["id"].as_str() else {
                continue;
            };
            // 旧档里的 id 可能带 `@account` 后缀（多实例）：以 id 为准解析，
            // 缺字段的旧记录按默认实例处理。
            let key = SessionKey::parse(_id).unwrap_or(SessionKey {
                platform: s["platform"].as_str().unwrap_or("local").into(),
                scope: s["scope"].as_str().unwrap_or("tui").into(),
                scope_id: s["scope_id"].as_str().unwrap_or("").into(),
                user_id: s["user_id"].as_str().unwrap_or("0").into(),
                account: None,
            });
            let nickname = s["nickname"].as_str().unwrap_or("").into();
            let group_name = s["group_name"].as_str().map(|g| g.to_string());
            let session = self.get_or_create(&key, nickname, group_name);
            if let Some(last_active) = s["last_active"].as_i64() {
                session.last_active.store(last_active, Ordering::Relaxed);
            }
            count += 1;
        }
        count
    }

    /// Restore a persisted file. Accepts v1 (per-session array), v2 (shared
    /// context object) and v3 (single trunk) formats; older formats migrate
    /// into the single trunk. Returns the number of restored identities.
    fn deserialize_legacy(&self, root: serde_json::Value) -> usize {
        let (identity_values, persisted_trunk) = match &root {
            serde_json::Value::Array(sessions) => (sessions.as_slice(), None),
            serde_json::Value::Object(_) => {
                let Some(sessions) = root["identities"]
                    .as_array()
                    .or_else(|| root["sessions"].as_array())
                else {
                    return 0;
                };
                (
                    sessions.as_slice(),
                    root["trunk_history"]
                        .as_array()
                        .or_else(|| root["shared_history"].as_array()),
                )
            }
            _ => return 0,
        };
        let mut count = 0;
        // 最老格式（v1/v2，2026-09 多会话改造）：把消息转成**带归属**的事件
        // ——v1 的每身份历史归入各自会话；v2/v3 的共享 trunk 归入首个身份
        // （没有身份时归本地会话），此后一切走事件日志投影。
        let mut events: Vec<echo_session::SessionEvent> = Vec::new();
        let shared_owner: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
        for s in identity_values {
            let Some(id) = s["id"].as_str() else {
                continue;
            };
            // 旧档里的 id 可能带 `@account` 后缀（多实例）：以 id 为准解析，
            // 缺字段的旧记录按默认实例处理。
            let key = SessionKey::parse(id).unwrap_or(SessionKey {
                platform: s["platform"].as_str().unwrap_or("local").into(),
                scope: s["scope"].as_str().unwrap_or("tui").into(),
                scope_id: s["scope_id"].as_str().unwrap_or("").into(),
                user_id: s["user_id"].as_str().unwrap_or("0").into(),
                account: None,
            });
            let session = self.get_or_create(&key, String::new(), None);
            if let Some(last_active) = s["last_active"].as_i64() {
                session.last_active.store(last_active, Ordering::Relaxed);
            }
            if shared_owner.lock().ok().is_some_and(|o| o.is_none()) {
                *shared_owner.lock().expect("owner") = Some(session.id.clone());
            }
            if persisted_trunk.is_none() {
                if let Some(history) = s["history"].as_array() {
                    events.extend(events_from_messages(
                        &deserialize_messages(history),
                        Some(session.id.clone()),
                    ));
                }
            }
            count += 1;
        }
        if let Some(shared) = persisted_trunk {
            let owner = shared_owner
                .lock()
                .ok()
                .and_then(|o| o.clone())
                .unwrap_or_else(|| SessionKey::local_tui().to_session_id());
            events.extend(events_from_messages(
                &deserialize_messages(shared),
                Some(owner),
            ));
        }
        if !events.is_empty() {
            self.event_log.extend(events.clone());
            self.ensure_histories_for_events(&events);
            self.reproject_all();
            self.mark_dirty();
        }

        // Restore the display timeline (v4+). Older files have no timeline —
        // an empty history is the correct fallback.
        let restored_timeline = root["timeline"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        serde_json::from_value::<crate::event::TimelineMessage>(entry.clone()).ok()
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Ok(mut timeline) = self.timeline.try_lock() {
            *timeline = restored_timeline;
        }
        count
    }

    /// Evict identity labels idle longer than TTL. Called periodically from a
    /// background task; only the source label is dropped, the trunk survives.
    pub fn evict_idle(&self) {
        let now = chrono::Utc::now().timestamp_millis();
        self.identities
            .retain(|_, s| now - s.last_active() * 1000 < IDENTITY_IDLE_TTL_MS);
    }

    /// Get or create an identity label for the given key. Every identity is
    /// bound to the same global trunk.
    ///
    /// Uses entry API so concurrent callers for the same key see only one identity.
    /// Assign the owning team id; every session created afterwards is
    /// tagged with it (call once at boot before any session is created).
    pub fn set_team_id(&self, team_id: Option<String>) {
        *self.team_id.lock().unwrap() = team_id;
    }

    pub fn team_id(&self) -> Option<String> {
        self.team_id.lock().unwrap().clone()
    }

    pub fn get_or_create(
        &self,
        key: &SessionKey,
        nickname: String,
        group_name: Option<String>,
    ) -> Session {
        let session_id = key.to_session_id();
        match self.identities.entry(session_id.clone()) {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                e.get().touch();
                e.get().clone()
            }
            dashmap::mapref::entry::Entry::Vacant(v) => {
                let session = Session::with_trunk(
                    key,
                    self.team_id(),
                    nickname,
                    group_name,
                    self.history_or_create(&session_id),
                    Arc::clone(&self.trunk_turn_lock),
                    Arc::clone(&self.trunk_turn_queue),
                );
                v.insert(session.clone());
                session
            }
        }
    }

    /// 某会话的上下文投影句柄（不存在时惰性创建空投影）。
    pub fn history_or_create(&self, session_id: &str) -> Arc<Mutex<Vec<ChatMessage>>> {
        if let Some(existing) = self.session_histories.get(session_id) {
            return Arc::clone(existing.value());
        }
        self.session_histories
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(Vec::new())))
            .value()
            .clone()
    }

    /// 某会话的上下文投影句柄（只读；None = 该会话从未有过历史）。
    pub fn history_for(&self, session_id: &str) -> Option<Arc<Mutex<Vec<ChatMessage>>>> {
        self.session_histories
            .get(session_id)
            .map(|entry| Arc::clone(entry.value()))
    }

    /// 已登记上下文的全部会话 id。
    pub fn history_sessions(&self) -> Vec<String> {
        self.session_histories
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    pub fn get(&self, session_id: &str) -> Option<Session> {
        self.identities.get(session_id).map(|s| s.clone())
    }

    pub fn all(&self) -> Vec<Session> {
        self.identities.iter().map(|e| e.value().clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.identities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.identities.is_empty()
    }

    pub fn memory_limit_tokens(&self) -> usize {
        self.memory_limit_tokens
    }

    /// 全部会话投影的条目总数（调试/监控聚合口径）。
    pub fn trunk_len(&self) -> usize {
        self.session_histories
            .iter()
            .map(|entry| entry.value().try_lock().map(|h| h.len()).unwrap_or(0))
            .sum()
    }

    /// 全部会话投影的估算 token 总数。
    pub fn trunk_tokens(&self) -> usize {
        self.session_histories
            .iter()
            .map(|entry| {
                entry
                    .value()
                    .try_lock()
                    .map(|h| estimate_history_tokens(&h))
                    .unwrap_or(0)
            })
            .sum()
    }

    /// Clone one session's model-facing history (point-in-time snapshot)。
    /// 会话不存在（从未收发过消息）时返回空列表。
    pub async fn snapshot_for(&self, session_id: &str) -> Vec<ChatMessage> {
        match self.history_for(session_id) {
            Some(history) => history.lock().await.clone(),
            None => Vec::new(),
        }
    }

    /// Erase all conversation memory: the durable event log, the in-memory
    /// trunk projection and the display timeline, then persist immediately so
    /// a restart cannot resurrect the cleared history. Identity labels
    /// (provenance metadata) and the session header are kept — they carry no
    /// message content.
    pub async fn clear_history(&self) {
        self.event_log.clear();
        for entry in self.session_histories.iter() {
            entry.value().lock().await.clear();
        }
        self.timeline.lock().await.clear();
        self.mark_dirty();
        self.save_now().await;
    }

    /// 归档会话：先把当前持久化文件复制到 `archives/`，再清空历史
    /// （归档后从头开始）。返回归档文件路径。
    pub async fn archive_history(&self) -> Result<String, String> {
        let path = {
            self.persist_path
                .lock()
                .expect("persist path poisoned")
                .clone()
        };
        if let Some(ref p) = path {
            if p.exists() {
                let data = std::fs::read_to_string(p)
                    .map_err(|e| format!("read session file failed: {e}"))?;
                let dir = p.parent().unwrap_or_else(|| std::path::Path::new("."));
                let archive_dir = dir.join("archives");
                std::fs::create_dir_all(&archive_dir)
                    .map_err(|e| format!("create archives dir failed: {e}"))?;
                let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("session");
                let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                let archive = archive_dir.join(format!("{stem}-{ts}.json"));
                std::fs::write(&archive, data).map_err(|e| format!("write archive failed: {e}"))?;
                self.clear_history().await;
                self.timeline
                    .lock()
                    .await
                    .push(crate::event::TimelineMessage::system(
                        format!("历史已归档：{}", archive.display()),
                        String::new(),
                        chrono::Utc::now().timestamp(),
                    ));
                return Ok(archive.display().to_string());
            }
        }
        Err("没有可归档的会话文件".into())
    }

    /// 压缩历史（多会话，2026-09）：**按会话分别**把该会话前
    /// `keep_recent` 条之外的事件替换为一条规则摘要（Compaction 事件，
    /// 带会话归属），投影随之更新并立即持久化。未归因的旧事件单独成组。
    pub async fn compact_history(&self, keep_recent: usize) -> Result<String, String> {
        let events = self.event_log.log();
        if events.len() <= keep_recent + 1 {
            return Err(format!("历史不足（{} 条事件，无需压缩）", events.len()));
        }
        // 按会话分组：保持会话首次出现顺序（None 组排在末尾）。
        let mut order: Vec<Option<String>> = Vec::new();
        let mut groups: std::collections::HashMap<Option<String>, Vec<echo_session::SessionEvent>> =
            std::collections::HashMap::new();
        for event in events {
            let key = event.session().map(str::to_string);
            if !groups.contains_key(&key) {
                order.push(key.clone());
            }
            groups.entry(key).or_default().push(event);
        }
        let mut new_log: Vec<echo_session::SessionEvent> = Vec::new();
        let mut total_replaced = 0usize;
        for key in order {
            let Some(group) = groups.remove(&key) else {
                continue;
            };
            if group.len() <= keep_recent + 1 {
                new_log.extend(group);
                continue;
            }
            let replaced_count = group.len() - keep_recent;
            let (tools, users) = {
                let tools = group[..replaced_count]
                    .iter()
                    .filter(|e| matches!(e, echo_session::SessionEvent::ToolCall(_)))
                    .count();
                let users = group[..replaced_count]
                    .iter()
                    .filter(|e| matches!(e, echo_session::SessionEvent::UserMessage(_)))
                    .count();
                (tools, users)
            };
            let summary = format!(
                "[历史摘要] 已压缩 {replaced_count} 条历史事件（{users} 条用户消息，{tools} 次工具调用），保留最近 {keep_recent} 条。"
            );
            new_log.push(echo_session::SessionEvent::Compaction(
                echo_session::CompactionEvent {
                    replaced_count,
                    summary,
                    session: key.clone(),
                },
            ));
            new_log.extend(group[replaced_count..].to_vec());
            total_replaced += replaced_count;
        }
        if total_replaced == 0 {
            return Err(format!(
                "历史不足（{} 条事件，无需压缩）",
                self.event_log.len()
            ));
        }
        self.event_log.clear();
        self.event_log.extend(new_log);
        self.reproject_all();
        self.timeline
            .lock()
            .await
            .push(crate::event::TimelineMessage::system(
                format!("历史已压缩：{total_replaced} 条事件 → 摘要（保留最近 {keep_recent} 条）"),
                String::new(),
                chrono::Utc::now().timestamp(),
            ));
        self.mark_dirty();
        self.save_now().await;
        Ok(format!(
            "已压缩 {total_replaced} 条历史事件，保留最近 {keep_recent} 条"
        ))
    }

    // ── Event-sourced session log (Phase 3) ────────────────────────────────

    /// Append one durable session event and update the in-memory trunk
    /// projection cache under the same lock. The event log is the source of
    /// truth; `trunk_history` is its projection, trimmed to the token budget.
    ///
    /// The caller must hold `session.turn_lock` (or otherwise serialize
    /// writers) so the event order matches the projected message order.
    pub(crate) fn append_event(&self, event: echo_session::SessionEvent) {
        let session = event.session().map(str::to_string);
        self.event_log.append(event);
        self.reproject(&session);
        self.mark_dirty();
    }

    /// 事件日志变化后刷新投影缓存：`Some(id)` 只刷新该会话（其余会话的
    /// 投影不受影响）；`None` 刷新全部会话（未归因事件的兜底路径）。
    fn reproject(&self, session: &Option<String>) {
        let log = self.event_log.log();
        match session {
            Some(id) => {
                // 归因事件会物化其会话的上下文句柄（查询/快照可达）。
                let _ = self.history_or_create(id);
                self.reproject_one(&log, id);
            }
            None => {
                for entry in self.session_histories.iter() {
                    let id = entry.key().clone();
                    self.reproject_one(&log, &id);
                }
            }
        }
    }

    fn reproject_one(&self, log: &[echo_session::SessionEvent], session_id: &str) {
        // 先在锁外完成投影（try_lock 语义：竞争时跳过本轮，下轮再投影）。
        let projected = echo_session::derive::derive_messages_for(
            log,
            Some(session_id),
            self.memory_limit_tokens,
        );
        let Some(entry) = self.session_histories.get(session_id) else {
            return;
        };
        let handle = Arc::clone(entry.value());
        drop(entry);
        if let Ok(mut guard) = handle.try_lock() {
            *guard = projected;
        };
    }

    /// 全量重投影（加载/压缩后调用）。
    fn reproject_all(&self) {
        let log = self.event_log.log();
        let ids: Vec<String> = self
            .session_histories
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for id in ids {
            self.reproject_one(&log, &id);
        }
    }

    /// 为事件里出现的每个会话登记投影句柄（加载期：事件可能引用已删除的
    /// 身份，仍需可查询的历史）。
    fn ensure_histories_for_events(&self, events: &[echo_session::SessionEvent]) {
        for event in events {
            if let Some(id) = event.session() {
                self.history_or_create(id);
            }
        }
    }

    /// The full event log (oldest first) — the durable source of truth.
    pub fn event_log(&self) -> Vec<echo_session::SessionEvent> {
        self.event_log.log()
    }

    /// Insert an event after the last user event carrying `sequence`
    /// (concurrent-branch merge), then re-project the trunk cache.
    pub(crate) fn insert_event_after_sequence(
        &self,
        sequence: u64,
        event: echo_session::SessionEvent,
    ) {
        let session = event.session().map(str::to_string);
        self.event_log.insert_after_sequence(sequence, event);
        self.reproject(&session);
        self.mark_dirty();
    }

    // ── Session header (fork/resume metadata) ───────────────────────────────

    /// Set the store's session header (lineage, origin, delegation depth).
    pub fn set_header(&self, header: echo_session::SessionHeader) {
        *self.header.lock().expect("header poisoned") = Some(header);
    }

    /// The store's session header, if set.
    pub fn header(&self) -> Option<echo_session::SessionHeader> {
        self.header.lock().expect("header poisoned").clone()
    }
}

/// Trim a history by the token budget: drop messages from the head until the
/// total fits. Always keeps the newest message so the latest input survives.
///
/// If even the newest message alone exceeds the budget, its content is
/// truncated (head preserved) so the trunk can never exceed the budget.
pub fn trim_by_tokens(history: &mut Vec<ChatMessage>, token_limit: usize) {
    let mut total: usize = estimate_history_tokens(history);
    let mut head = 0usize;
    while history.len() - head > 1 && total > token_limit {
        total = total.saturating_sub(estimate_message_tokens(&history[head]));
        head += 1;
    }
    if head > 0 {
        history.drain(..head);
    }
    // The last remaining message may still exceed the budget on its own.
    if let Some(last) = history.last_mut() {
        if estimate_message_tokens(last) > token_limit {
            crate::llm::truncate_message_to_tokens(last, token_limit);
        }
    }
}

fn identity_metadata(session: &Session) -> serde_json::Value {
    serde_json::json!({
        "id": session.id,
        "platform": session.session_key.platform,
        "scope": session.session_key.scope,
        "scope_id": session.session_key.scope_id,
        "user_id": session.session_key.user_id,
        "nickname": session.nickname,
        "group_name": session.group_name,
        "last_active": session.last_active(),
    })
}

fn serialize_messages(history: &[ChatMessage]) -> Vec<serde_json::Value> {
    history
        .iter()
        .map(|message| {
            serde_json::json!({
                "role": match message.role {
                    crate::llm::ChatRole::System => "system",
                    crate::llm::ChatRole::User => "user",
                    crate::llm::ChatRole::Assistant => "assistant",
                    crate::llm::ChatRole::Tool => "tool",
                },
                "content": message.content,
                "reasoning_content": message.reasoning_content,
            })
        })
        .collect()
}

fn deserialize_messages(history: &[serde_json::Value]) -> Vec<ChatMessage> {
    history
        .iter()
        .filter_map(|message| {
            Some(ChatMessage {
                role: match message["role"].as_str()? {
                    "system" => crate::llm::ChatRole::System,
                    "user" => crate::llm::ChatRole::User,
                    "assistant" => crate::llm::ChatRole::Assistant,
                    "tool" => crate::llm::ChatRole::Tool,
                    _ => return None,
                },
                content: message["content"].as_str()?.into(),
                reasoning_content: message["reasoning_content"].as_str().map(str::to_string),
                tool_calls: None,
                tool_call_id: None,
                images: message["images"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| m.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// 从结构化 hook 内容推导会话 id（旧版事件归因迁移用）。
///
/// 支持的封装（见 `input_marker`）：
/// - `<{platform}_message_hook>{json}</…>`：从载荷解析 platform/channel/sender
///   （`channel.type == "group"` 时 scope=group，scope_id=群号；`adapter` 为
///   非默认 QQ 实例名时产生 `@account` 后缀）；
/// - `<backend_message_hook>`：本地后端消息 → 本地 TUI 会话；
/// - 其余（timer/后台任务事件等）无归属信息 → None（由调用方粘滞/兜底）。
fn session_id_from_hook_content(content: &str) -> Option<String> {
    let trimmed = content.trim_start();
    let close = trimmed.find('>')?;
    let marker = trimmed.strip_prefix('<')?;
    if close == 0 || close > marker.len() {
        return None;
    }
    let marker = &marker[..close - 1];
    let platform_marker = marker.strip_suffix("_message_hook")?;
    if platform_marker == "backend" {
        return Some(SessionKey::local_tui().to_session_id());
    }
    let body_start = close + 1;
    let body_end = trimmed[body_start..].find("</")? + body_start;
    let payload: serde_json::Value =
        serde_json::from_str(trimmed[body_start..body_end].trim()).ok()?;
    let platform = payload.get("platform").and_then(|v| v.as_str())?;
    let adapter = payload
        .get("adapter")
        .and_then(|v| v.as_str())
        .unwrap_or(platform);
    let channel = payload.get("channel");
    let (scope, scope_id) = match channel.and_then(|c| c.get("type")).and_then(|t| t.as_str()) {
        Some("group") => (
            "group",
            channel
                .and_then(|c| c.get("group_id"))
                .and_then(|g| g.as_str())
                .unwrap_or(""),
        ),
        _ => ("dm", ""),
    };
    let user_id = payload
        .get("sender")
        .and_then(|s| s.get("user_id"))
        .and_then(|u| u.as_str())?;
    let account = (adapter != DEFAULT_QQ_INSTANCE).then(|| adapter.to_string());
    Some(
        SessionKey {
            platform: platform.to_string(),
            scope: scope.to_string(),
            scope_id: scope_id.to_string(),
            user_id: user_id.to_string(),
            account,
        }
        .to_session_id(),
    )
}

/// 旧版事件归因迁移（v5 及更早的日志没有会话归属）。
///
/// 规则（两遍）：
/// 1. 用户消息从 hook 内容推导会话（可切换"当前会话"）；assistant/tool
///    事件继承当前会话（旧日志按会话交错写入，正文应答紧跟其请求）；
/// 2. 仍未知的事件（如前导的孤儿事件、压缩事件）挂到首个已知会话；
///    完全没有可推导会话时统一挂本地 TUI 会话。
///
/// 返回是否有改动。混合文件（部分已归因）按已有归属继续，保持幂等。
fn attribute_legacy_events(events: &mut [echo_session::SessionEvent]) -> bool {
    if events.iter().all(|event| event.session().is_some()) {
        return false;
    }
    let mut changed = false;
    let mut current: Option<String> = None;
    for event in events.iter_mut() {
        if matches!(event, echo_session::SessionEvent::Compaction(_)) {
            continue;
        }
        if let Some(id) = event.session() {
            current = Some(id.to_string());
            continue;
        }
        if let echo_session::SessionEvent::UserMessage(user) = event {
            if let Some(derived) = session_id_from_hook_content(&user.content) {
                current = Some(derived);
            }
        }
        if let Some(id) = &current {
            *event.session_mut() = Some(id.clone());
            changed = true;
        }
    }
    let first_known = events
        .iter()
        .find_map(|event| event.session().map(str::to_string));
    let fallback = first_known.unwrap_or_else(|| SessionKey::local_tui().to_session_id());
    for event in events.iter_mut() {
        if event.session().is_none() {
            *event.session_mut() = Some(fallback.clone());
            changed = true;
        }
    }
    changed
}

/// 把模型可见消息转回事件（最老格式迁移；归属可选）。
fn events_from_messages(
    messages: &[ChatMessage],
    session: Option<String>,
) -> Vec<echo_session::SessionEvent> {
    use crate::llm::ChatRole;
    let mut events = Vec::new();
    for message in messages {
        match message.role {
            ChatRole::User => events.push(echo_session::SessionEvent::UserMessage(
                echo_session::event::UserMessage {
                    content: message.content.clone(),
                    timestamp: 0,
                    message_sequence: None,
                    source: None,
                    images: message.images.clone(),
                    session: session.clone(),
                },
            )),
            ChatRole::Assistant => events.push(echo_session::SessionEvent::AssistantMessage(
                echo_session::event::AssistantMessage {
                    content: message.content.clone(),
                    reasoning_content: message.reasoning_content.clone(),
                    tool_calls: message.tool_calls.clone().unwrap_or_default(),
                    session: session.clone(),
                },
            )),
            ChatRole::Tool => events.push(echo_session::SessionEvent::ToolResult(
                echo_session::event::ToolResultEvent {
                    tool_call_id: message.tool_call_id.clone().unwrap_or_default(),
                    result: message.content.clone(),
                    images: message.images.clone(),
                    session: session.clone(),
                },
            )),
            // 系统提示词不入事件日志（每轮重建）。
            ChatRole::System => {}
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Round-trip for any key whose fields don't contain the `:` separator
        /// (the wire format's only structural character).
        #[test]
        fn session_key_roundtrip(
            platform in "[a-z0-9_]{1,10}",
            scope in "dm|group|tui",
            scope_id in "[0-9]{0,12}",
            user_id in "[0-9]{1,15}",
        ) {
            let key = SessionKey { platform, scope, scope_id, user_id, account: None };
            let id = key.to_session_id();
            let parsed = SessionKey::parse(&id).expect("roundtrip parse");
            prop_assert_eq!(parsed, key);
        }

        /// Arbitrary strings must never panic in parse; malformed input
        /// either parses to a key whose serialisation round-trips or is None.
        #[test]
        fn session_key_parse_never_panics(
            input in ".*",
        ) {
            if let Some(key) = SessionKey::parse(&input) {
                prop_assert_eq!(SessionKey::parse(&key.to_session_id()), Some(key));
            }
        }
    }

    // ── 多会话上下文（2026-09）──

    /// 旧版事件归因：hook 内容推导 + 粘滞继承 + 兜底。
    #[test]
    fn legacy_attribution_splits_interleaved_sessions() {
        use echo_session::event::{AssistantMessage, ToolCallEvent, ToolResultEvent, UserMessage};
        let qq_hook = "<qq_message_hook>\n{\n  \"platform\": \"qq\",\n  \"adapter\": \"qq\",\n  \"channel\": {\"type\": \"private\"},\n  \"sender\": {\"user_id\": \"1828980067\"}\n}\n</qq_message_hook>".to_string();
        let group_hook = "<qq_message_hook>\n{\n  \"platform\": \"qq\",\n  \"adapter\": \"qq\",\n  \"channel\": {\"type\": \"group\", \"group_id\": \"999\"},\n  \"sender\": {\"user_id\": \"42\"}\n}\n</qq_message_hook>".to_string();
        let backend_hook =
            "<backend_message_hook>\n{\"content\": \"你好\"}\n</backend_message_hook>".to_string();
        let mut events = vec![
            echo_session::SessionEvent::UserMessage(UserMessage {
                content: qq_hook.clone(),
                timestamp: 0,
                message_sequence: None,
                source: None,
                images: vec![],
                session: None,
            }),
            echo_session::SessionEvent::ToolCall(ToolCallEvent {
                id: "c1".into(),
                name: "send_private_msg".into(),
                arguments: "{}".into(),
                session: None,
            }),
            echo_session::SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "c1".into(),
                result: "sent".into(),
                images: vec![],
                session: None,
            }),
            echo_session::SessionEvent::AssistantMessage(AssistantMessage {
                content: "已回复。".into(),
                reasoning_content: None,
                tool_calls: vec![],
                session: None,
            }),
            echo_session::SessionEvent::UserMessage(UserMessage {
                content: backend_hook.clone(),
                timestamp: 0,
                message_sequence: None,
                source: None,
                images: vec![],
                session: None,
            }),
            echo_session::SessionEvent::AssistantMessage(AssistantMessage {
                content: "你好，本地".into(),
                reasoning_content: None,
                tool_calls: vec![],
                session: None,
            }),
            echo_session::SessionEvent::UserMessage(UserMessage {
                content: group_hook.clone(),
                timestamp: 0,
                message_sequence: None,
                source: None,
                images: vec![],
                session: None,
            }),
        ];
        assert!(attribute_legacy_events(&mut events), "changed");
        let sessions: Vec<Option<&str>> = events.iter().map(|e| e.session()).collect();
        assert_eq!(
            sessions,
            vec![
                Some("qq:dm::1828980067"),
                Some("qq:dm::1828980067"),
                Some("qq:dm::1828980067"),
                Some("qq:dm::1828980067"),
                Some("local:tui::local_user"),
                Some("local:tui::local_user"),
                Some("qq:group:999:42"),
            ]
        );
        // 幂等：再跑一遍无改动。
        assert!(!attribute_legacy_events(&mut events));
    }

    /// 未归因的前导事件挂到首个已知会话；完全无可推导时挂本地。
    #[test]
    fn legacy_attribution_falls_back_to_first_known_session() {
        use echo_session::event::{AssistantMessage, UserMessage};
        let mut events = vec![
            echo_session::SessionEvent::AssistantMessage(AssistantMessage {
                content: "孤儿应答".into(),
                reasoning_content: None,
                tool_calls: vec![],
                session: None,
            }),
            echo_session::SessionEvent::UserMessage(UserMessage {
                content: "<qq_message_hook>\n{\"platform\":\"qq\",\"channel\":{\"type\":\"private\"},\"sender\":{\"user_id\":\"7\"}}\n</qq_message_hook>".into(),
                timestamp: 0,
                message_sequence: None,
                source: None,
                images: vec![],
                session: None,
            }),
        ];
        assert!(attribute_legacy_events(&mut events));
        assert_eq!(
            events[0].session(),
            Some("qq:dm::7"),
            "leading orphan joins first known"
        );

        // 完全无法推导：统一挂本地 TUI 会话。
        let mut plain = vec![echo_session::SessionEvent::UserMessage(UserMessage {
            content: "普通文本（无 hook 标记）".into(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
            session: None,
        })];
        assert!(attribute_legacy_events(&mut plain));
        assert_eq!(plain[0].session(), Some("local:tui::local_user"));
    }

    /// 同一事件日志交错写入两个会话：各自快照互不可见。
    #[tokio::test]
    async fn interleaved_events_project_per_session() {
        use echo_session::event::{AssistantMessage, UserMessage};
        let store = TrunkStore::new(10_000);
        let local = store.get_or_create(&SessionKey::local_tui(), "local".into(), None);
        let qq_key = SessionKey::parse("qq:dm::9").unwrap();
        let qq = store.get_or_create(&qq_key, "u".into(), None);
        let pairs = [
            (local.id.clone(), "本地一"),
            (qq.id.clone(), "qq 一"),
            (local.id.clone(), "本地二"),
            (qq.id.clone(), "qq 二"),
        ];
        for (session_id, content) in pairs {
            store.append_event(echo_session::SessionEvent::UserMessage(UserMessage {
                content: content.into(),
                timestamp: 0,
                message_sequence: None,
                source: None,
                images: vec![],
                session: Some(session_id.clone()),
            }));
            store.append_event(echo_session::SessionEvent::AssistantMessage(
                AssistantMessage {
                    content: format!("{content} 的回复"),
                    reasoning_content: None,
                    tool_calls: vec![],
                    session: Some(session_id),
                },
            ));
        }
        let local_hist = store.snapshot_for(&local.id).await;
        let qq_hist = store.snapshot_for(&qq.id).await;
        assert_eq!(local_hist.len(), 4, "local sees only its 4 messages");
        assert_eq!(qq_hist.len(), 4, "qq sees only its 4 messages");
        assert!(local_hist.iter().all(|m| !m.content.starts_with("qq")));
        assert!(qq_hist.iter().all(|m| !m.content.starts_with("本地")));
    }

    #[test]
    fn session_key_format() {
        let key = SessionKey {
            platform: "qq".into(),
            scope: "dm".into(),
            scope_id: "".into(),
            user_id: "123".into(),
            account: None,
        };
        assert_eq!(key.to_session_id(), "qq:dm::123");
    }

    #[test]
    fn session_key_parse() {
        let key = SessionKey::parse("qq:group:456:789").unwrap();
        assert_eq!(key.platform, "qq");
        assert_eq!(key.scope, "group");
        assert_eq!(key.scope_id, "456");
        assert_eq!(key.user_id, "789");
    }

    #[test]
    fn session_key_parse_legacy() {
        let key = SessionKey::parse("user_12345").unwrap();
        assert_eq!(key.platform, "local");
        assert_eq!(key.scope, "tui");
        assert_eq!(key.user_id, "12345");
    }

    #[test]
    fn session_key_local_tui() {
        let key = SessionKey::local_tui();
        assert_eq!(key.to_session_id(), "local:tui::local_user");
    }

    #[test]
    fn session_key_parse_malformed() {
        assert!(SessionKey::parse("abc").is_none());
        assert!(SessionKey::parse("a:b").is_none());
    }

    #[tokio::test]
    async fn trunk_get_or_create_deduplicates_identities() {
        let store = TrunkStore::new(1000);
        let key = SessionKey::parse("qq:dm::111").unwrap();
        let s = store.get_or_create(&key, "alice".into(), None);
        assert_eq!(s.id, "qq:dm::111");
        assert_eq!(store.len(), 1);

        // Same key returns existing identity.
        let s2 = store.get_or_create(&key, "bob".into(), None);
        assert_eq!(store.len(), 1);
        assert_eq!(s2.nickname, "alice"); // original nickname preserved
    }

    #[tokio::test]
    async fn sessions_have_independent_histories_and_shared_turn_lock() {
        let store = TrunkStore::new(1000);
        let local = store.get_or_create(&SessionKey::local_tui(), "local".into(), None);
        let qq = store.get_or_create(
            &SessionKey::parse("qq:dm::123").unwrap(),
            "alice".into(),
            None,
        );

        // 多会话（2026-09）：上下文相互独立，只有写入锁共享。
        assert!(!Arc::ptr_eq(&local.history, &qq.history));
        assert!(Arc::ptr_eq(&local.turn_lock, &qq.turn_lock));

        // 归属 local 的事件只进 local 的上下文，qq 不受影响。
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                content: "local only".into(),
                timestamp: 0,
                message_sequence: None,
                source: None,
                images: vec![],
                session: Some("local:tui::local_user".into()),
            },
        ));
        assert_eq!(local.history.lock().await[0].content, "local only");
        assert!(qq.history.lock().await.is_empty(), "qq context untouched");
        assert_eq!(store.trunk_len(), 1, "aggregate over sessions");
    }

    #[tokio::test]
    async fn session_info_has_platform_fields() {
        let store = TrunkStore::new(1000);
        let key = SessionKey::parse("qq:group:789:456").unwrap();
        let s = store.get_or_create(&key, "bob".into(), Some("TestGroup".into()));
        let info = s.info("last msg".into());
        assert_eq!(info.platform, "qq");
        assert_eq!(info.scope, "group");
        assert_eq!(info.user_id, "456");
        assert_eq!(info.group_name, Some("TestGroup".into()));
        assert_eq!(info.last_message, "last msg");
    }

    // ── Persistence ─────────────────────────────────────────────────────────

    fn temp_sessions_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("echo-trunk-{name}-{}.json", std::process::id()))
    }

    #[tokio::test]
    async fn save_and_load_roundtrip_restores_trunk_and_identities() {
        let path = temp_sessions_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        let store = TrunkStore::new(1000);
        store.set_persist_path(&path);
        let key = SessionKey::parse("qq:group:789:456").unwrap();
        let _s = store.get_or_create(&key, "bob".into(), Some("TestGroup".into()));
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some(key.to_session_id()),
                content: "你好".into(),
                timestamp: 1700000000,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
        store.append_event(echo_session::SessionEvent::AssistantMessage(
            echo_session::event::AssistantMessage {
                session: Some(key.to_session_id()),
                content: "回复".into(),
                reasoning_content: None,
                tool_calls: vec![],
            },
        ));
        store.save_now().await;
        assert!(path.exists(), "trunk file written");

        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["version"], 6, "v6 multi-session format");
        assert!(
            root["trunk_histories"].is_object(),
            "per-session projections"
        );
        assert_eq!(root["events"].as_array().unwrap().len(), 2);
        assert!(root["identities"]
            .as_array()
            .unwrap()
            .iter()
            .all(|identity| identity.get("history").is_none()));

        // A fresh store restores everything.
        let store2 = TrunkStore::new(1000);
        store2.set_persist_path(&path);
        let restored = store2.load_from_file().await;
        assert_eq!(restored, 1, "one identity restored");
        let s2 = store2.get_or_create(&key, "bob".into(), Some("TestGroup".into()));
        let hist = s2.history.lock().await.clone();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].content, "你好");
        assert_eq!(hist[1].content, "回复");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn persisted_history_restores_per_session() {
        let path = temp_sessions_path("shared-roundtrip");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(1000);
        store.set_persist_path(&path);
        let _local = store.get_or_create(&SessionKey::local_tui(), "local".into(), None);
        let qq_key = SessionKey::parse("qq:dm::123").unwrap();
        let _qq = store.get_or_create(&qq_key, "alice".into(), None);
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some("local:tui::local_user".into()),
                content: "from tui".into(),
                timestamp: 0,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
        store.append_event(echo_session::SessionEvent::AssistantMessage(
            echo_session::event::AssistantMessage {
                session: Some("local:tui::local_user".into()),
                content: "tui reply".into(),
                reasoning_content: None,
                tool_calls: vec![],
            },
        ));
        store.save_now().await;

        let restored = TrunkStore::new(1000);
        restored.set_persist_path(&path);
        assert_eq!(restored.load_from_file().await, 2);
        let local = restored.get("local:tui::local_user").unwrap();
        let qq = restored.get(&qq_key.to_session_id()).unwrap();
        // 上下文按会话隔离：local 有 2 条，qq 为空。
        assert!(!Arc::ptr_eq(&local.history, &qq.history));
        assert_eq!(local.history.lock().await.len(), 2);
        assert!(qq.history.lock().await.is_empty(), "qq context stays empty");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v1_array_migrates_distinct_histories_per_session() {
        let store = TrunkStore::new(1000);
        let restored = store.deserialize(
            r#"[
                {"id":"local:tui::local_user","platform":"local","scope":"tui","scope_id":"","user_id":"local_user","nickname":"local","last_active":1,"history":[{"role":"user","content":"local"}]},
                {"id":"qq:dm::123","platform":"qq","scope":"dm","scope_id":"","user_id":"123","nickname":"alice","last_active":2,"history":[{"role":"user","content":"qq"}]}
            ]"#,
        );

        assert_eq!(restored, 2);
        // 多会话（2026-09）：v1 的每身份历史归入各自会话的上下文。
        let local = store
            .history_for("local:tui::local_user")
            .expect("local history");
        assert_eq!(local.try_lock().unwrap()[0].content, "local");
        let qq = store.history_for("qq:dm::123").expect("qq history");
        assert_eq!(qq.try_lock().unwrap()[0].content, "qq");
        assert_eq!(store.trunk_len(), 2, "aggregate over both sessions");
    }

    #[test]
    fn v2_shared_object_migrates_into_first_identity() {
        let store = TrunkStore::new(1000);
        let restored = store.deserialize(
            r#"{
                "version": 2,
                "shared_context": true,
                "shared_history": [{"role":"user","content":"shared"}],
                "sessions": [
                    {"id":"local:tui::local_user","platform":"local","scope":"tui","scope_id":"","user_id":"local_user","nickname":"local","last_active":1},
                    {"id":"qq:dm::123","platform":"qq","scope":"dm","scope_id":"","user_id":"123","nickname":"alice","last_active":2}
                ]
            }"#,
        );

        assert_eq!(restored, 2);
        // v2 共享上下文没有归属信息：归入首个身份（local）。
        let history = store
            .history_for("local:tui::local_user")
            .expect("local history");
        assert_eq!(history.try_lock().unwrap()[0].content, "shared");
        assert!(store
            .history_for("qq:dm::123")
            .map(|h| h.try_lock().unwrap().is_empty())
            .unwrap_or(true));
        assert_eq!(store.len(), 2, "both identities restored");
    }

    #[test]
    fn timeline_push_caps_at_max_and_keeps_newest() {
        let store = TrunkStore::new(1000);
        for i in 0..(TRUNK_TIMELINE_MAX + 50) {
            store.push_timeline(crate::event::TimelineMessage {
                kind: "user".into(),
                content: format!("msg-{i}"),
                session_id: "local:tui::local_user".into(),
                time: i as i64,
                source: None,
                reasoning: None,
                tool: None,
                images: None,
                seq: 0,
            });
        }
        let timeline = store.timeline_snapshot();
        assert_eq!(timeline.len(), TRUNK_TIMELINE_MAX, "capped at the max");
        assert_eq!(
            timeline.first().unwrap().content,
            "msg-50",
            "oldest dropped"
        );
        assert_eq!(
            timeline.last().unwrap().content,
            format!("msg-{}", TRUNK_TIMELINE_MAX + 49)
        );
    }

    #[tokio::test]
    async fn clear_history_wipes_log_trunk_timeline_and_disk() {
        let path = temp_sessions_path("clear-history");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(1000);
        store.set_persist_path(&path);
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some("local:tui::local_user".into()),
                content: "记住我".into(),
                timestamp: 1700000000,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
        store.push_timeline(crate::event::TimelineMessage {
            kind: "user".into(),
            content: "记住我".into(),
            session_id: "local:tui::local_user".into(),
            time: 1700000000,
            source: None,
            reasoning: None,
            tool: None,
            images: None,
            seq: 0,
        });
        store.save_now().await;
        assert_eq!(store.trunk_len(), 1);
        assert_eq!(store.timeline_snapshot().len(), 1);

        store.clear_history().await;

        assert_eq!(store.trunk_len(), 0, "trunk projection cleared");
        assert!(store.event_log().is_empty(), "event log cleared");
        assert!(store.timeline_snapshot().is_empty(), "timeline cleared");
        // The persisted file must already reflect the wipe.
        let restored = TrunkStore::new(1000);
        restored.set_persist_path(&path);
        restored.load_from_file().await;
        assert_eq!(restored.trunk_len(), 0, "cleared history survives restart");
        assert!(restored.timeline_snapshot().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn timeline_roundtrips_through_the_persisted_file() {
        let path = temp_sessions_path("timeline-roundtrip");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(1000);
        store.set_persist_path(&path);
        store.push_timeline(crate::event::TimelineMessage {
            kind: "user".into(),
            content: "你好".into(),
            session_id: "qq:dm::123".into(),
            time: 1700000000,
            source: Some(crate::event::TimelineSource {
                adapter_name: "qq".into(),
                platform: "qq".into(),
                user_id: "123".into(),
                user_name: "alice".into(),
                channel: "direct".into(),
                group_name: None,
                received_at_ms: 1700000000123,
                message_sequence: 7,
            }),
            reasoning: None,
            tool: None,
            images: None,
            seq: 0,
        });
        store.push_timeline(crate::event::TimelineMessage {
            kind: "backend".into(),
            content: "回复".into(),
            session_id: "qq:dm::123".into(),
            time: 1700000001,
            source: None,
            reasoning: Some(vec!["先分析".into()]),
            tool: None,
            images: None,
            seq: 0,
        });
        store.save_now().await;

        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["version"], 6, "timeline persisted as v6");
        assert_eq!(root["timeline"].as_array().unwrap().len(), 2);

        let restored = TrunkStore::new(1000);
        restored.set_persist_path(&path);
        restored.load_from_file().await;
        let timeline = restored.timeline_snapshot();
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[0].kind, "user");
        assert_eq!(timeline[0].content, "你好");
        assert_eq!(
            timeline[0].source.as_ref().unwrap().message_sequence,
            7,
            "source provenance restored"
        );
        assert_eq!(
            timeline[1].reasoning.as_deref(),
            Some(vec!["先分析".to_string()].as_slice()),
            "reasoning restored"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v3_file_without_timeline_loads_with_empty_timeline() {
        let store = TrunkStore::new(1000);
        let restored = store.deserialize(
            r#"{
                "version": 3,
                "trunk_history": [{"role":"user","content":"hi"}],
                "identities": []
            }"#,
        );
        assert_eq!(restored, 0);
        assert!(store.timeline_snapshot().is_empty(), "no timeline → empty");
    }

    fn push_user(store: &TrunkStore, content: &str) {
        store.push_timeline(crate::event::TimelineMessage {
            kind: "user".into(),
            content: content.into(),
            session_id: "local:tui::local_user".into(),
            time: 0,
            source: None,
            reasoning: None,
            tool: None,
            images: None,
            seq: 0,
        });
    }

    #[test]
    fn snapshot_since_filters_by_entry_seq() {
        let store = TrunkStore::new(1000);
        push_user(&store, "a");
        push_user(&store, "b");
        push_user(&store, "c");
        // seq: a=1, b=2, c=3
        let (delta, seq) = store.timeline_snapshot_since(1).expect("window intact");
        assert_eq!(seq, 3);
        assert_eq!(
            delta.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        // 空增量：只推进游标，不返回条目（前端不得据此清空聊天）。
        let (delta, seq) = store.timeline_snapshot_since(3).expect("no-op");
        assert!(delta.is_empty());
        assert_eq!(seq, 3);
    }

    #[test]
    fn snapshot_since_redelivers_in_place_updated_entry() {
        let store = TrunkStore::new(1000);
        push_user(&store, "a");
        // 模拟 update_timeline_tool 的就地更新：bump 计数器并写回条目 seq。
        let new_seq = store.bump_timeline_seq();
        if let Some(mut timeline) = store.timeline_mut() {
            timeline[0].seq = new_seq;
            timeline[0].content = "a-updated".into();
        }
        let (delta, seq) = store.timeline_snapshot_since(1).expect("window intact");
        assert_eq!(seq, 2);
        assert_eq!(delta.len(), 1, "updated entry re-delivered");
        assert_eq!(delta[0].content, "a-updated");
    }

    #[test]
    fn snapshot_since_returns_none_on_gap_or_future_cursor() {
        let store = TrunkStore::new(1000);
        for i in 0..(TRUNK_TIMELINE_MAX + 10) {
            push_user(&store, &format!("msg-{i}"));
        }
        // 头部已滚出：最老条目 seq = 11，since_seq = 1 的窗口不完整 → None。
        assert!(
            store.timeline_snapshot_since(1).is_none(),
            "gap → full fallback"
        );
        // 恰好贴着现存最老条目（seq 11 的前一个）仍可增量。
        let cursor = store.timeline_snapshot().first().unwrap().seq - 1;
        assert!(store.timeline_snapshot_since(cursor).is_some());
        // 游标超前于当前序号（来自 core 重启前）→ None。
        let current = store.timeline_seq();
        assert!(store.timeline_snapshot_since(current + 1).is_none());
    }

    #[test]
    fn restore_marks_dangling_running_tool_as_interrupted() {
        // 进程被杀（如自更新重启）时 ToolResult 永远不会到达；恢复时必须
        // 把 output=null 的僵尸 running 条目标注为中断并分配新 seq，
        // 否则前端会一直显示"运行中"。
        let store = TrunkStore::new(1000);
        store.deserialize(
            r#"{
                "version": 5,
                "events": [],
                "timeline": [
                    {"kind":"tool","content":"bash","session_id":"s","time":1,
                     "tool":{"name":"bash","input":"x","output":null,"failed":false}},
                    {"kind":"tool","content":"read_file","session_id":"s","time":2,
                     "tool":{"name":"read_file","input":"y","output":"ok","failed":false}}
                ]
            }"#,
        );
        let timeline = store.timeline_snapshot();
        let zombie = timeline[0].tool.as_ref().unwrap();
        assert!(zombie.failed, "dangling entry marked failed");
        assert!(
            zombie
                .output
                .as_deref()
                .unwrap_or_default()
                .contains("已中断"),
            "interrupted note attached"
        );
        assert!(timeline[0].seq > 0, "interrupted entry gets a fresh seq");
        // 已完成的条目不受影响。
        let done = timeline[1].tool.as_ref().unwrap();
        assert!(!done.failed);
        assert_eq!(done.output.as_deref(), Some("ok"));
        assert_eq!(timeline[1].seq, 0);
        assert_eq!(
            store.timeline_seq(),
            1,
            "seq counter past the patched entry"
        );
    }

    #[tokio::test]
    async fn timeline_seq_survives_save_load() {
        let path = temp_sessions_path("timeline-seq-rehydrate");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(1000);
        store.set_persist_path(&path);
        push_user(&store, "a");
        push_user(&store, "b");
        store.save_now().await;

        let restored = TrunkStore::new(1000);
        restored.set_persist_path(&path);
        restored.load_from_file().await;
        assert_eq!(
            restored.timeline_seq(),
            2,
            "seq counter rehydrated from disk"
        );
        // 重启后继续 push，序号连续不回退。
        push_user(&restored, "c");
        assert_eq!(restored.timeline_seq(), 3);
        let (delta, _) = restored.timeline_snapshot_since(2).expect("intact");
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].content, "c");
        let _ = std::fs::remove_file(&path);
    }

    /// BASELINE (Phase 0): the v4 persistence format dropped `tool_calls` and
    /// `tool_call_id` on the tool round-trip — a tool message serialized then
    /// deserialized degraded to plain text. Phase 3 replaced the format with
    /// the event-sourced log (`echo_session`), whose event round-trip
    /// preserves tool structure (see `echo_session::event` tests). This
    /// legacy serializer is retained only for v4-compatible writes and
    /// documents the old lossy behaviour.
    #[test]
    fn baseline_tool_message_structure_is_lost_on_roundtrip() {
        let message = ChatMessage {
            role: crate::llm::ChatRole::Assistant,
            content: "".into(),
            reasoning_content: None,
            tool_calls: Some(vec![crate::llm::ToolCall {
                id: "call_1".into(),
                name: "send_private_msg".into(),
                arguments: r#"{"user_id":123,"content":"hi"}"#.into(),
            }]),
            tool_call_id: None,
            images: vec![],
        };
        let serialized = serialize_messages(std::slice::from_ref(&message));
        let restored = deserialize_messages(&serialized);
        assert_eq!(restored.len(), 1);
        assert!(
            restored[0].tool_calls.is_none(),
            "baseline: tool_calls are dropped by the v4 format (Phase 3 target: preserved)"
        );
    }

    #[tokio::test]
    async fn load_from_missing_file_returns_zero() {
        let store = TrunkStore::new(1000);
        store.set_persist_path(temp_sessions_path("missing"));
        assert_eq!(store.load_from_file().await, 0);
    }

    #[tokio::test]
    async fn load_handles_malformed_entries_leniently() {
        let path = temp_sessions_path("malformed");
        std::fs::write(
            &path,
            r#"[
                {"id": "ok", "platform": "qq", "scope": "dm", "scope_id": "", "user_id": "1", "nickname": "a", "history": []},
                {"id": "bad", "platform": "qq", "scope": "dm"},
                "not an object"
            ]"#,
        )
        .unwrap();
        let store = TrunkStore::new(1000);
        store.set_persist_path(&path);
        let restored = store.load_from_file().await;
        // Entries with an id are restored leniently (missing fields fall back
        // to defaults); non-object entries are skipped.
        assert_eq!(restored, 2);
        // The partially-populated entry falls back to defaults for the
        // missing fields only (scope_id/user_id → empty/0).
        let fallback = store.get("qq:dm::0").expect("lenient default identity");
        assert_eq!(fallback.session_key.scope_id, "");
        assert_eq!(fallback.session_key.user_id, "0");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_get_or_create_yields_single_identity() {
        let store = TrunkStore::new(1000);
        let key = SessionKey::parse("qq:dm::999").unwrap();
        let key2 = key.clone();

        // 32 concurrent callers racing on the same key.
        let mut handles = Vec::new();
        for _ in 0..32 {
            let store = store.clone();
            let key = key.clone();
            handles.push(tokio::spawn(async move {
                store.get_or_create(&key, "alice".into(), None)
            }));
        }
        for h in handles {
            let _ = h.await.unwrap();
        }
        assert_eq!(store.len(), 1, "single identity for concurrent callers");
        let _ = key2;
    }

    #[test]
    fn oversized_newest_message_is_truncated_to_fit_the_budget() {
        // 600 CJK chars ≈ 600 tokens, budget 100 → must be truncated.
        let mut history = vec![ChatMessage::user("x".repeat(600))];
        trim_by_tokens(&mut history, 100);
        assert_eq!(history.len(), 1);
        let tokens = crate::llm::estimate_message_tokens(&history[0]);
        assert!(
            tokens <= 100,
            "truncated message must fit the budget, got {tokens} tokens"
        );
        assert!(
            history[0].content.contains("内容过长已截断"),
            "truncation marker expected"
        );
    }

    #[test]
    fn short_histories_are_never_trimmed() {
        let mut history = vec![ChatMessage::user("你好"), ChatMessage::assistant("好的")];
        trim_by_tokens(&mut history, 100);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "你好");
    }

    #[test]
    fn structured_hook_keeps_its_sequence_after_truncation() {
        let hook = r#"<qq_message_hook>
{"channel":{"type":"private"},"message_sequence":42,"content":"请分析这份超长文档"}</qq_message_hook>"#;
        let mut history = vec![ChatMessage::user(hook)];
        trim_by_tokens(&mut history, 60);
        let remaining = &history[0].content;
        // Sequence must survive the truncation (it lives at the head).
        assert!(remaining.starts_with("<qq_message_hook>"));
        assert!(remaining.contains("\"message_sequence\":42"));
    }

    #[test]
    fn evict_idle_drops_stale_identity_but_keeps_context() {
        let store = TrunkStore::new(1000);
        let key = SessionKey::parse("qq:dm::111").unwrap();
        let stale = store.get_or_create(&key, "alice".into(), None);
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some(key.to_session_id()),
                content: "kept".into(),
                timestamp: 0,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
        // Force last_active far in the past (2× TTL).
        let past = chrono::Utc::now().timestamp() - (IDENTITY_IDLE_TTL_MS / 1000) * 2;
        stale
            .last_active
            .store(past, std::sync::atomic::Ordering::Relaxed);

        let key2 = SessionKey::parse("qq:dm::222").unwrap();
        let recent = store.get_or_create(&key2, "bob".into(), None);
        recent.touch(); // now

        assert_eq!(store.len(), 2);
        store.evict_idle();
        assert_eq!(store.len(), 1);
        assert!(
            store.get(&key2.to_session_id()).is_some(),
            "recent identity survives"
        );
        // 身份被回收，但上下文（投影缓存）保留（历史不随身份标签删除）。
        assert_eq!(store.trunk_len(), 1, "context survives eviction");
        assert!(store.history_for(&key.to_session_id()).is_some());
    }
}
