//! Global conversation trunk with per-source identities.
//!
//! The agent keeps exactly ONE conversation context — the trunk. Every
//! inbound message forks a temporary branch from a point-in-time snapshot
//! of the trunk; when the branch finishes, its result is merged back in
//! append-only order (see [`crate::agent::Agent::process_message`]).
//!
//! `Session` is now a lightweight identity (origin label) bound to the
//! trunk: it records who said what and where, but owns no conversation
//! history of its own.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::event::SessionInfo;
use crate::llm::{estimate_history_tokens, estimate_message_tokens, ChatMessage};

/// Structured session identity, replacing the old `user_{qq_number}` format.
///
/// ```text
/// format: {platform}:{scope}:{scope_id}:{user_id}
///
/// Examples:
///   qq:dm::123456           — QQ direct message, user 123456
///   qq:group:987654:123456  — QQ group 987654, user 123456
///   local:tui::local_user   — local TUI interaction
/// ```
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
}

impl SessionKey {
    /// Build the canonical session ID string.
    pub fn to_session_id(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.platform, self.scope, self.scope_id, self.user_id
        )
    }

    /// Create a session key for local TUI use.
    pub fn local_tui() -> Self {
        SessionKey {
            platform: "local".into(),
            scope: "tui".into(),
            scope_id: String::new(),
            user_id: "local_user".into(),
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
            });
        }
        let parts: Vec<&str> = session_id.splitn(4, ':').collect();
        if parts.len() != 4 {
            return None;
        }
        Some(SessionKey {
            platform: parts[0].to_string(),
            scope: parts[1].to_string(),
            scope_id: parts[2].to_string(),
            user_id: parts[3].to_string(),
        })
    }
}

/// A source identity bound to the global conversation trunk.
///
/// All sessions share the same trunk history and turn lock; a session only
/// carries provenance metadata (platform user/channel, nickname, activity).
/// Cloneable: clones share the same trunk and activity clock.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    /// Owning team id (None = default/legacy).
    pub team_id: Option<String>,
    pub session_key: SessionKey,
    pub nickname: String,
    pub group_name: Option<String>,
    last_active: Arc<AtomicI64>,
    /// The global trunk history (shared by every session, bounded by the
    /// token budget `memory_limit_tokens`).
    pub history: Arc<Mutex<Vec<ChatMessage>>>,
    /// Serialises trunk snapshot creation and result merges; branch execution never holds this lock.
    pub turn_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Session {
    fn with_trunk(
        key: &SessionKey,
        team_id: Option<String>,
        nickname: String,
        group_name: Option<String>,
        history: Arc<Mutex<Vec<ChatMessage>>>,
        turn_lock: Arc<tokio::sync::Mutex<()>>,
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

/// Registry of source identities plus the single global conversation trunk.
pub struct TrunkStore {
    /// Owning team id (None = default). Set once by the Agent.
    team_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    identities: Arc<DashMap<String, Session>>,
    memory_limit_tokens: usize,
    trunk_history: Arc<Mutex<Vec<ChatMessage>>>,
    trunk_turn_lock: Arc<tokio::sync::Mutex<()>>,
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
            trunk_history: Arc::clone(&self.trunk_history),
            trunk_turn_lock: Arc::clone(&self.trunk_turn_lock),
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

/// Persistence format version. v5 = event-sourced: the session event log is
/// the source of truth, `trunk_history`/`identities`/`timeline` are persisted
/// projections for compatibility. v4 and older (v1 per-session array, v2
/// shared context, v3 single trunk) migrate into the event log on load.
const PERSIST_VERSION: u32 = 5;

/// Upper bound on persisted display timeline entries.
const TRUNK_TIMELINE_MAX: usize = 1024;

impl TrunkStore {
    /// Create a store owning the single global conversation trunk.
    pub fn new(memory_limit_tokens: usize) -> Self {
        Self {
            team_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            identities: Arc::new(DashMap::new()),
            memory_limit_tokens,
            trunk_history: Arc::new(Mutex::new(Vec::new())),
            trunk_turn_lock: Arc::new(tokio::sync::Mutex::new(())),
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
        let history = self.trunk_history.try_lock().ok()?;
        let timeline = self.timeline.try_lock().ok()?;
        let identities = self.all().iter().map(identity_metadata).collect::<Vec<_>>();
        let events = self.event_log.log();
        let header = self.header.lock().expect("header poisoned").clone();
        serde_json::to_string_pretty(&serde_json::json!({
            "version": PERSIST_VERSION,
            // The event log is the source of truth; the projection fields are
            // persisted for forward compatibility with older readers.
            "header": header,
            "events": events,
            "trunk_history": serialize_messages(&history),
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

        // v5: event log is authoritative. Re-project the trunk from it.
        if let Some(events) = root["events"].as_array() {
            if let Ok(header) = serde_json::from_value::<Option<echo_session::SessionHeader>>(
                root["header"].clone(),
            ) {
                *self.header.lock().expect("header poisoned") = header;
            }
            let events: Vec<echo_session::SessionEvent> = events
                .iter()
                .filter_map(|event| serde_json::from_value(event.clone()).ok())
                .collect();
            if !events.is_empty() || root["version"].as_u64() == Some(5) {
                self.event_log.extend(events.clone());
                let projected = echo_session::derive::derive_messages(
                    &self.event_log.log(),
                    self.memory_limit_tokens,
                );
                if let Ok(mut trunk) = self.trunk_history.try_lock() {
                    *trunk = projected;
                }
                let count = self.restore_identity_labels(&root);
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
            let mut all_events = Vec::new();
            for session in &migrated {
                all_events.extend(session.events.clone());
            }
            // The old format kept one global trunk shared by all identities;
            // deduplicate the migrated events so the log is not duplicated.
            all_events.dedup();
            self.event_log.extend(all_events.clone());
            let projected = echo_session::derive::derive_messages(
                &self.event_log.log(),
                self.memory_limit_tokens,
            );
            if let Ok(mut trunk) = self.trunk_history.try_lock() {
                *trunk = projected;
            }
            // Fall through to the identity/timeline restore below.
            let count = self.restore_identity_labels(&root);
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
    fn restore_timeline(&self, timeline: Vec<crate::event::TimelineMessage>) {
        let max_seq = timeline.iter().map(|entry| entry.seq).max().unwrap_or(0);
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
            let key = SessionKey {
                platform: s["platform"].as_str().unwrap_or("local").into(),
                scope: s["scope"].as_str().unwrap_or("tui").into(),
                scope_id: s["scope_id"].as_str().unwrap_or("").into(),
                user_id: s["user_id"].as_str().unwrap_or("0").into(),
            };
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
        let mut legacy_histories = Vec::new();
        for s in identity_values {
            let Some(_id) = s["id"].as_str() else {
                continue;
            };
            let key = SessionKey {
                platform: s["platform"].as_str().unwrap_or("local").into(),
                scope: s["scope"].as_str().unwrap_or("tui").into(),
                scope_id: s["scope_id"].as_str().unwrap_or("").into(),
                user_id: s["user_id"].as_str().unwrap_or("0").into(),
            };
            let nickname = s["nickname"].as_str().unwrap_or("").into();
            let group_name = s["group_name"].as_str().map(|g| g.to_string());
            let session = self.get_or_create(&key, nickname, group_name);
            if let Some(last_active) = s["last_active"].as_i64() {
                session.last_active.store(last_active, Ordering::Relaxed);
            }
            if persisted_trunk.is_none() {
                if let Some(history) = s["history"].as_array() {
                    legacy_histories.push((
                        s["last_active"].as_i64().unwrap_or_default(),
                        deserialize_messages(history),
                    ));
                }
            }
            count += 1;
        }

        let mut history = persisted_trunk
            .map(|history| deserialize_messages(history))
            .unwrap_or_else(|| merge_legacy_histories(legacy_histories));
        trim_by_tokens(&mut history, self.memory_limit_tokens);
        if let Ok(mut trunk) = self.trunk_history.try_lock() {
            *trunk = history;
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
        match self.identities.entry(session_id) {
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
                    Arc::clone(&self.trunk_history),
                    Arc::clone(&self.trunk_turn_lock),
                );
                v.insert(session.clone());
                session
            }
        }
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

    /// Number of messages currently in the trunk.
    pub fn trunk_len(&self) -> usize {
        self.trunk_history.try_lock().map(|h| h.len()).unwrap_or(0)
    }

    /// Estimated tokens currently held in the trunk.
    pub fn trunk_tokens(&self) -> usize {
        self.trunk_history
            .try_lock()
            .map(|h| estimate_history_tokens(&h))
            .unwrap_or(0)
    }

    /// Clone the current trunk history (point-in-time snapshot).
    pub async fn snapshot(&self) -> Vec<ChatMessage> {
        self.trunk_history.lock().await.clone()
    }

    /// Erase all conversation memory: the durable event log, the in-memory
    /// trunk projection and the display timeline, then persist immediately so
    /// a restart cannot resurrect the cleared history. Identity labels
    /// (provenance metadata) and the session header are kept — they carry no
    /// message content.
    pub async fn clear_history(&self) {
        self.event_log.clear();
        self.trunk_history.lock().await.clear();
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

    /// 压缩历史：事件日志前 `keep_recent` 条之外的部分替换为一条
    /// 规则摘要（Compaction 事件），投影随之更新并立即持久化。
    pub async fn compact_history(&self, keep_recent: usize) -> Result<String, String> {
        let events = self.event_log.log();
        if events.len() <= keep_recent + 1 {
            return Err(format!("历史不足（{} 条事件，无需压缩）", events.len()));
        }
        let replaced_count = events.len() - keep_recent;
        let (tools, users) = {
            let tools = events[..replaced_count]
                .iter()
                .filter(|e| matches!(e, echo_session::SessionEvent::ToolCall(_)))
                .count();
            let users = events[..replaced_count]
                .iter()
                .filter(|e| matches!(e, echo_session::SessionEvent::UserMessage(_)))
                .count();
            (tools, users)
        };
        let summary = format!(
            "[历史摘要] 已压缩 {replaced_count} 条历史事件（{users} 条用户消息，{tools} 次工具调用），保留最近 {keep_recent} 条。"
        );
        // 重写日志：摘要 + 现存尾部
        let tail = events[replaced_count..].to_vec();
        self.event_log.clear();
        self.event_log
            .append(echo_session::SessionEvent::Compaction(
                echo_session::CompactionEvent {
                    replaced_count,
                    summary,
                },
            ));
        self.event_log.extend(tail);
        let projected =
            echo_session::derive::derive_messages(&self.event_log.log(), self.memory_limit_tokens);
        if let Ok(mut trunk) = self.trunk_history.try_lock() {
            *trunk = projected;
        }
        self.timeline
            .lock()
            .await
            .push(crate::event::TimelineMessage::system(
                format!("历史已压缩：{replaced_count} 条事件 → 摘要（保留最近 {keep_recent} 条）"),
                String::new(),
                chrono::Utc::now().timestamp(),
            ));
        self.mark_dirty();
        self.save_now().await;
        Ok(format!(
            "已压缩 {replaced_count} 条历史事件，保留最近 {keep_recent} 条"
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
        self.event_log.append(event);
        let projected =
            echo_session::derive::derive_messages(&self.event_log.log(), self.memory_limit_tokens);
        if let Ok(mut trunk) = self.trunk_history.try_lock() {
            *trunk = projected;
        }
        self.mark_dirty();
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
        self.event_log.insert_after_sequence(sequence, event);
        let projected =
            echo_session::derive::derive_messages(&self.event_log.log(), self.memory_limit_tokens);
        if let Ok(mut trunk) = self.trunk_history.try_lock() {
            *trunk = projected;
        }
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

fn merge_legacy_histories(mut histories: Vec<(i64, Vec<ChatMessage>)>) -> Vec<ChatMessage> {
    histories.sort_by_key(|(last_active, _)| *last_active);
    let Some((_, first)) = histories.first() else {
        return Vec::new();
    };
    if histories.iter().all(|(_, history)| history == first) {
        return first.clone();
    }
    histories
        .into_iter()
        .flat_map(|(_, history)| history)
        .collect()
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
            let key = SessionKey { platform, scope, scope_id, user_id };
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

    #[test]
    fn session_key_format() {
        let key = SessionKey {
            platform: "qq".into(),
            scope: "dm".into(),
            scope_id: "".into(),
            user_id: "123".into(),
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
    async fn all_identities_share_one_trunk_history_and_turn_lock() {
        let store = TrunkStore::new(1000);
        let local = store.get_or_create(&SessionKey::local_tui(), "local".into(), None);
        let qq = store.get_or_create(
            &SessionKey::parse("qq:dm::123").unwrap(),
            "alice".into(),
            None,
        );

        local.history.lock().await.push(ChatMessage::user("shared"));

        assert!(Arc::ptr_eq(&local.history, &qq.history));
        assert!(Arc::ptr_eq(&local.turn_lock, &qq.turn_lock));
        assert_eq!(qq.history.lock().await[0].content, "shared");
        assert_eq!(store.trunk_len(), 1, "trunk holds the single history");
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
                content: "你好".into(),
                timestamp: 1700000000,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
        store.append_event(echo_session::SessionEvent::AssistantMessage(
            echo_session::event::AssistantMessage {
                content: "回复".into(),
                reasoning_content: None,
                tool_calls: vec![],
            },
        ));
        store.save_now().await;
        assert!(path.exists(), "trunk file written");

        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["version"], 5, "v5 event-sourced format");
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
    async fn persisted_history_is_shared_across_restored_identities() {
        let path = temp_sessions_path("shared-roundtrip");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(1000);
        store.set_persist_path(&path);
        let _local = store.get_or_create(&SessionKey::local_tui(), "local".into(), None);
        let qq_key = SessionKey::parse("qq:dm::123").unwrap();
        let _qq = store.get_or_create(&qq_key, "alice".into(), None);
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                content: "from tui".into(),
                timestamp: 0,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
        store.append_event(echo_session::SessionEvent::AssistantMessage(
            echo_session::event::AssistantMessage {
                content: "shared reply".into(),
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
        assert!(Arc::ptr_eq(&local.history, &qq.history));
        assert_eq!(local.history.lock().await.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v1_array_migrates_distinct_histories_into_one_trunk() {
        let store = TrunkStore::new(1000);
        let restored = store.deserialize(
            r#"[
                {"id":"local:tui::local_user","platform":"local","scope":"tui","scope_id":"","user_id":"local_user","nickname":"local","last_active":1,"history":[{"role":"user","content":"local"}]},
                {"id":"qq:dm::123","platform":"qq","scope":"dm","scope_id":"","user_id":"123","nickname":"alice","last_active":2,"history":[{"role":"user","content":"qq"}]}
            ]"#,
        );

        assert_eq!(restored, 2);
        let history = store.trunk_history.try_lock().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "local");
        assert_eq!(history[1].content, "qq");
    }

    #[test]
    fn v2_shared_object_migrates_into_trunk() {
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
        let history = store.trunk_history.try_lock().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "shared");
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
        assert_eq!(root["version"], 5, "timeline persisted as v5");
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
        assert!(store.timeline_snapshot_since(1).is_none(), "gap → full fallback");
        // 恰好贴着现存最老条目（seq 11 的前一个）仍可增量。
        let cursor = store.timeline_snapshot().first().unwrap().seq - 1;
        assert!(store.timeline_snapshot_since(cursor).is_some());
        // 游标超前于当前序号（来自 core 重启前）→ None。
        let current = store.timeline_seq();
        assert!(store.timeline_snapshot_since(current + 1).is_none());
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
        assert_eq!(restored.timeline_seq(), 2, "seq counter rehydrated from disk");
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
    fn evict_idle_drops_stale_identity_but_keeps_trunk() {
        let store = TrunkStore::new(1000);
        let key = SessionKey::parse("qq:dm::111").unwrap();
        let stale = store.get_or_create(&key, "alice".into(), None);
        stale
            .history
            .blocking_lock()
            .push(ChatMessage::user("kept"));
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
        assert_eq!(store.trunk_len(), 1, "trunk history survives eviction");
    }
}
