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

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
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

    /// Create a session key for a local workspace channel
    /// (`local:workspace:<workspace_id>:local_user`): the workspace's own
    /// local conversation — activating a workspace enters this channel
    /// (2026-09-14「激活 = 进入项目对话」).
    pub fn local_workspace(workspace_id: &str) -> Self {
        SessionKey {
            platform: "local".into(),
            scope: "workspace".into(),
            scope_id: workspace_id.to_string(),
            user_id: "local_user".into(),
            account: None,
        }
    }

    /// Parse a session ID string back into a key. Returns `None` on legacy
    /// `user_{id}` format (falls back to `local:tui:`) or malformed strings.
    pub fn parse(session_id: &str) -> Option<Self> {
        // federation Phase 0：`node://<node>/<local>` 全局引用——剥离节点
        // 前缀按本机格式解析，节点归属由 [`SessionKey::parse_qualified`]
        // 保留（Phase 1 联邦路由消费）。
        let (_node, local) = echo_defs::NodeId::split_ref(session_id);
        let session_id = local;
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

    /// 解析一条可能带 `node://` 前缀的会话引用，保留节点归属。
    ///
    /// 返回 `(node, key)`：`node = None` 表示本机会话（既有一切 id 不变）；
    /// `Some(node)` 表示远程节点上的会话——Phase 0 仅解析，Core 内所有
    /// 现存调用方仍用 [`SessionKey::parse`]（节点前缀被剥离、按本机处理，
    /// 行为与此前一致）。
    pub fn parse_qualified(reference: &str) -> Option<(Option<String>, Self)> {
        let (node, local) = echo_defs::NodeId::split_ref(reference);
        Self::parse(local).map(|key| (node.map(str::to_string), key))
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
    pub history: Arc<parking_lot::Mutex<Vec<ChatMessage>>>,
    /// Serialises trunk snapshot creation and result merges; branch execution never holds this lock.
    pub turn_lock: Arc<tokio::sync::Mutex<()>>,
    /// 单会话模式的 turn 排队闸门（FIFO，tokio Mutex 保证公平）：
    /// 同一会话同一时刻只跑一个 turn，其余 turn 在获得许可后才取上下文
    /// 快照（因此排队的 turn 能看到前一轮的回复）。**每会话一把独立锁**
    /// （2026-10 修复：此前误为 agent 全局共享，跨会话也被串行）；并行
    /// 多会话模式下不使用（`Agent::loop_mode` = `Parallel`）。
    pub turn_queue: Arc<tokio::sync::Mutex<()>>,
}

impl Session {
    fn with_trunk(
        key: &SessionKey,
        team_id: Option<String>,
        nickname: String,
        group_name: Option<String>,
        history: Arc<parking_lot::Mutex<Vec<ChatMessage>>>,
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
            node_id: crate::node_id().map(str::to_string),
            region_name: crate::region_name().map(str::to_string),
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
    /// token 预算（可热更：SaveTeam 保存后运行期生效，2026-10 审计 🔴）。
    memory_limit_tokens: AtomicUsize,
    /// 每会话的上下文投影缓存（session_id → history）。事件日志是唯一
    /// 事实来源，这里只是它的按会话投影（见 `reproject_session`）。
    session_histories: Arc<DashMap<String, Arc<parking_lot::Mutex<Vec<ChatMessage>>>>>,
    trunk_turn_lock: Arc<tokio::sync::Mutex<()>>,
    /// 压缩编排互斥（2026-10 巡检 🔴）：double compact / compact 与
    /// clear_history 竞争会按过时摘要重写日志、统计错乱——压缩全程
    /// 持有（预览→归档→摘要→落地），与 turn 写路径解耦（不阻塞正常
    /// 消息，只互斥压缩类操作）。
    compact_lock: Arc<tokio::sync::Mutex<()>>,
    /// 落盘互斥（2026-10 审计）：周期保存与各 save_now（清空 / 压缩 /
    /// 归档后）共用 `{path}.tmp`——并发写时旧快照可能**后** rename，
    /// 覆盖较新状态（重启后"压缩丢失 / 清空复活"）。所有写盘路径经此
    /// 串行：序列化在锁内完成，后到的写者一定写出 ≥ 前者的状态。
    save_lock: Arc<tokio::sync::Mutex<()>>,
    /// Display timeline (persisted, survives TUI restarts). Independent of the
    /// token-bounded LLM trunk: bounded by entry count, keeps richer metadata
    /// (source provenance, tool calls, reasoning) for the TUI history view.
    timeline: Arc<Mutex<Vec<crate::event::TimelineMessage>>>,
    /// Timeline 单调序号：每次新增/更新条目递增。前端用它做增量同步
    ///（RequestTrunkTimeline.since_seq + TrunkTimeline.seq），避免切换
    /// agent 时全量重传。
    timeline_seq: Arc<std::sync::atomic::AtomicU64>,
    /// 时间线写路径计数（探针，2026-10 面板"历史无声消失"事故定位）——
    /// push / cap-drain / 其余删除三个单调计数用于相邻采样对账。
    timeline_pushed: Arc<std::sync::atomic::AtomicU64>,
    timeline_drained: Arc<std::sync::atomic::AtomicU64>,
    timeline_removed: Arc<std::sync::atomic::AtomicU64>,
    /// 整表替换（restore / 旧格式加载）后置位：下一轮探针只重设基线，
    /// 不做跨表对账。
    timeline_rebaseline: Arc<std::sync::atomic::AtomicBool>,
    /// 探针上次采样快照（对账基准）。
    timeline_probe_state: Arc<std::sync::Mutex<Option<TimelineProbeSample>>>,
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
            memory_limit_tokens: AtomicUsize::new(self.memory_limit_tokens.load(Ordering::Relaxed)),
            session_histories: Arc::clone(&self.session_histories),
            trunk_turn_lock: Arc::clone(&self.trunk_turn_lock),
            compact_lock: Arc::clone(&self.compact_lock),
            save_lock: Arc::clone(&self.save_lock),
            timeline: Arc::clone(&self.timeline),
            timeline_seq: Arc::clone(&self.timeline_seq),
            timeline_pushed: Arc::clone(&self.timeline_pushed),
            timeline_drained: Arc::clone(&self.timeline_drained),
            timeline_removed: Arc::clone(&self.timeline_removed),
            timeline_rebaseline: Arc::clone(&self.timeline_rebaseline),
            timeline_probe_state: Arc::clone(&self.timeline_probe_state),
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
///   `session` attribution; per-session histories are re-projected from the
///   event log on load (v6 文件不再持久化 `trunk_histories` 投影）。
const PERSIST_VERSION: u32 = 6;

/// Upper bound on persisted display timeline entries.
const TRUNK_TIMELINE_MAX: usize = 1024;

/// One session's share of a compaction plan: the events that a compaction
/// would replace, in log order, plus counts for the fallback summary.
#[derive(Debug, Clone)]
pub struct CompactGroup {
    /// Owning session (None = unattributed legacy events).
    pub session: Option<String>,
    /// Leading events this group would replace (already sliced).
    pub replaced: Vec<echo_session::SessionEvent>,
    /// `replaced.len()`, kept for convenience.
    pub replaced_count: usize,
    /// User messages inside `replaced`.
    pub users: usize,
    /// Tool calls inside `replaced`.
    pub tools: usize,
}

/// What [`TrunkStore::compact_preview`] would do — computed without mutating
/// anything so the caller can archive and summarize before committing.
#[derive(Debug, Clone)]
pub struct CompactPreview {
    /// Groups that would be compacted (at least one replaced event each).
    pub groups: Vec<CompactGroup>,
    /// Total events across those groups' replaced prefixes.
    pub total_replaced: usize,
    /// Total events in the log at preview time (for reporting).
    pub total_events: usize,
}

/// One prepared summary to install, keyed by session.
#[derive(Debug, Clone)]
pub struct CompactionSummary {
    /// Owning session this summary stands in for (must match a preview group).
    pub session: Option<String>,
    /// Summary **body** — no `[历史摘要]` marker (added at projection time).
    pub summary: String,
    /// 预览时该组被替换的前缀长度（事件条数）。落地切片用它——见
    /// [`TrunkStore::apply_compaction`]（2026-10 审计：不再按应用时点
    /// `len - keep_recent` 重算，防窗口期新事件把保留尾最老条目挤成
    /// 无摘要丢弃）。
    pub replaced_count: usize,
}

/// Result of [`TrunkStore::apply_compaction`].
#[derive(Debug, Clone)]
pub struct CompactionApplied {
    /// Total events replaced.
    pub total_replaced: usize,
    /// Number of session groups compacted.
    pub groups: usize,
    /// Events left untouched behind each summary.
    pub kept: usize,
}

/// 时间线探针的采样快照（见 [`TrunkStore::probe_timeline`]）。
#[derive(Debug, Clone, Copy)]
struct TimelineProbeSample {
    /// 采样时的条目数。
    count: usize,
    /// 时间线头部条目的 seq（头部前移 = 有删除发生）。
    head_seq: u64,
    /// 三个单调写计数：成功入列的 push / cap-drain 滚出 / 其余删除。
    pushed: u64,
    drained: u64,
    removed: u64,
}

impl TimelineProbeSample {
    /// 相邻采样之间按写计数推导的**期望**条目数（探针对账恒等式）。
    fn expected_count_after(&self, now: &TimelineProbeSample) -> i64 {
        self.count as i64 + now.pushed.saturating_sub(self.pushed) as i64
            - now.drained.saturating_sub(self.drained) as i64
            - now.removed.saturating_sub(self.removed) as i64
    }
}

/// 时间线条目的会话分布（探针日志用；Top 6，`,` 连接）。
fn timeline_sessions_summary(timeline: &[crate::event::TimelineMessage]) -> String {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for message in timeline {
        *counts.entry(message.session_id.as_str()).or_default() += 1;
    }
    let mut entries: Vec<(&str, usize)> = counts.into_iter().collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    entries.truncate(6);
    entries
        .into_iter()
        .map(|(session, count)| format!("{}={count}", short_timeline_session(session)))
        .collect::<Vec<_>>()
        .join(",")
}

/// `local:workspace:<id>:local_user` → `<id>`；其余原样（截到 24 字符）。
fn short_timeline_session(session_id: &str) -> String {
    let short = session_id
        .strip_prefix("local:workspace:")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or(session_id);
    short.chars().take(24).collect()
}
/// Group events by owning session, preserving first-appearance order
/// (unattributed legacy events form their own group, in place).
fn group_events_by_session(
    events: &[echo_session::SessionEvent],
) -> Vec<(Option<String>, Vec<echo_session::SessionEvent>)> {
    let mut order: Vec<Option<String>> = Vec::new();
    let mut groups: std::collections::HashMap<Option<String>, Vec<echo_session::SessionEvent>> =
        std::collections::HashMap::new();
    for event in events {
        let key = event.session().map(str::to_string);
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(event.clone());
    }
    order
        .into_iter()
        .filter_map(|key| groups.remove(&key).map(|group| (key, group)))
        .collect()
}

impl TrunkStore {
    /// Create a store owning the single global conversation trunk.
    pub fn new(memory_limit_tokens: usize) -> Self {
        Self {
            team_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            identities: Arc::new(DashMap::new()),
            memory_limit_tokens: AtomicUsize::new(memory_limit_tokens),
            session_histories: Arc::new(DashMap::new()),
            trunk_turn_lock: Arc::new(tokio::sync::Mutex::new(())),
            compact_lock: Arc::new(tokio::sync::Mutex::new(())),
            save_lock: Arc::new(tokio::sync::Mutex::new(())),
            timeline: Arc::new(Mutex::new(Vec::new())),
            timeline_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeline_pushed: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeline_drained: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeline_removed: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeline_rebaseline: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            timeline_probe_state: Arc::new(std::sync::Mutex::new(None)),
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
    /// 压缩编排互斥锁（见字段注释）。`compact_history` 全程持有。
    pub async fn compact_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.compact_lock.lock().await
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Append one entry to the display timeline, dropping the oldest entries
    /// beyond the cap. Marks the store dirty so the entry is persisted.
    ///
    /// 条目 seq 在锁内赋值并与计数器同步推进（仅在条目真正入列时递增），
    /// 保证 `timeline_snapshot_since` 的按 seq 过滤语义成立。
    pub fn push_timeline(&self, message: crate::event::TimelineMessage) {
        let mut timeline = match self.timeline.try_lock() {
            Ok(guard) => guard,
            // A concurrent save/record is holding the lock; skip rather than
            // block the event loop (emit runs on the hot path).
            Err(_) => return,
        };
        self.push_timeline_locked(&mut timeline, message);
        drop(timeline);
        self.mark_dirty();
    }

    /// 同 [`Self::push_timeline`]，但**阻塞等待锁**——供异步控制路径
    /// （归档 / 压缩的 system 提示行等）使用：这些行必须入列，且要经
    /// 统一路径赋 seq / 计数 / 执行上限裁剪。
    ///
    /// 2026-10 审计修复：此前归档 / 压缩直接 `timeline.lock().await.push(…)`，
    /// 绕过 seq 与 pushed 计数、也不裁剪——(a) 提示行 seq=0，增量同步
    /// （since_seq）永远看不到它；(b) 探针对账恒等式在下一采样点误报
    /// 失衡（差一条未计数的插入）。
    pub async fn push_timeline_blocking(&self, message: crate::event::TimelineMessage) {
        let mut timeline = self.timeline.lock().await;
        self.push_timeline_locked(&mut timeline, message);
        drop(timeline);
        self.mark_dirty();
    }

    /// 入列共享实现（调用方持锁）：赋 seq、推进 pushed、执行 cap 裁剪。
    fn push_timeline_locked(
        &self,
        timeline: &mut Vec<crate::event::TimelineMessage>,
        mut message: crate::event::TimelineMessage,
    ) {
        message.seq = self
            .timeline_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        timeline.push(message);
        self.timeline_pushed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if timeline.len() > TRUNK_TIMELINE_MAX {
            let excess = timeline.len() - TRUNK_TIMELINE_MAX;
            timeline.drain(..excess);
            self.timeline_drained
                .fetch_add(excess as u64, std::sync::atomic::Ordering::Relaxed);
            if excess > 1 {
                // 正常路径单次 push 只滚出一条；一次滚出多条意味着异常批量
                // 插入（探针：定位"时间线头部无声前移"事故，2026-10）。
                tracing::warn!(
                    excess,
                    "timeline drained more than one entry in a single push"
                );
            }
        }
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
    /// 全量快照。锁被周期保存短暂持有时**短重试**（每次 10ms、至多
    /// 50 次 ≈ 0.5s）而非立即返回空——空快照曾被调用方当"空全量"
    /// 发给前端，直接清空本地聊天缓存（🔴 修复）。重试耗尽返回
    /// `None`，由调用方决定降级（跳过本次同步/下轮再试）。
    pub fn timeline_snapshot(&self) -> Option<Vec<crate::event::TimelineMessage>> {
        for _ in 0..50 {
            if let Ok(timeline) = self.timeline.try_lock() {
                return Some(timeline.clone());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        tracing::warn!("timeline_snapshot: lock contention exceeded 500ms, degraded to None");
        None
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
    /// attaching a tool result to its running entry).
    ///
    /// 锁被周期保存/快照短暂持有时**短重试**（每次 10ms、至多 10 次
    /// ≈ 100ms）。2026-10 审计：此前 try_lock 失败即返回 None——工具
    /// 完成更新（"运行中"→ 终值、附带输出）在竞争窗口被直接丢失，条目
    /// 永远停在运行态；该更新发生在计数之前，探针对账恒等式也测不到。
    /// 重试耗尽（>100ms）仍在日志留 warn，便于事后定位。
    pub fn timeline_mut(
        &self,
    ) -> Option<tokio::sync::MutexGuard<'_, Vec<crate::event::TimelineMessage>>> {
        for attempt in 0..10 {
            if let Ok(guard) = self.timeline.try_lock() {
                return Some(guard);
            }
            if attempt < 9 {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        tracing::warn!("timeline_mut: lock contention exceeded 100ms; in-place update dropped");
        None
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
        // 落盘互斥（2026-10 审计）：与周期保存 / 其它 save_now 串行。
        // 串行化后不存在"旧快照后 rename 覆盖新快照"的窗口——等待者在
        // 锁内重新序列化，写出的永远是最新（或更新）状态。
        let _save_guard = self.save_lock.lock().await;
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
        let header = self.header.lock().expect("header poisoned").clone();
        serde_json::to_string_pretty(&serde_json::json!({
            "version": PERSIST_VERSION,
            // The event log is the source of truth; per-session histories are
            // re-projected from it on load.
            "header": header,
            "events": events,
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
            // 媒体引用迁移（2026-09-24）：历史事件里内嵌的 data URI 图片
            // 落盘并改写为 `/media/<id>` 引用——不丢图片（模型上下文与
            // 面板展示都还原），但文件从数十 MB 收缩到 KB 级。
            let media_migrated = spill_event_media(&mut events);
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
                if media_migrated {
                    self.mark_dirty();
                }
                let count = self.restore_identity_labels(&root);
                // 事件日志里出现但身份元数据缺失的会话也要补建（含归属）——
                // 否则 Panel 会话列表丢项。
                self.ensure_identities_for_events(&events);
                // 按会话重建投影（身份 + 事件中出现的所有会话）。
                self.ensure_histories_for_events(&events);
                self.reproject_all();
                let mut timeline = root["timeline"]
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
                // 同一迁移作用于显示时间线：内嵌图片落盘 → `/media/<id>`。
                for entry in timeline.iter_mut() {
                    if let Some(images) = entry.images.as_mut() {
                        for image in images.iter_mut() {
                            *image = echo_defs::media_store::spill_or_keep(image);
                        }
                    }
                }
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
            let mut restored_timeline = root["timeline"]
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
            for entry in restored_timeline.iter_mut() {
                if let Some(images) = entry.images.as_mut() {
                    for image in images.iter_mut() {
                        *image = echo_defs::media_store::spill_or_keep(image);
                    }
                }
            }
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
        let entries = timeline.len();
        let head_seq = timeline.first().map(|entry| entry.seq).unwrap_or(0);
        let tail_seq = timeline.last().map(|entry| entry.seq).unwrap_or(0);
        if let Ok(mut guard) = self.timeline.try_lock() {
            *guard = timeline;
        }
        self.timeline_seq
            .store(max_seq, std::sync::atomic::Ordering::Relaxed);
        // 整表替换：置位探针重基线标志（跨表差值对账对替换无意义）。
        self.timeline_rebaseline
            .store(true, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(
            entries,
            head_seq,
            tail_seq,
            max_seq,
            interrupted,
            "display timeline restored from persisted file"
        );
    }

    /// 时间线探针（2026-10："面板看不到历史"事故定位用）。
    ///
    /// 背景：显示时间线头部曾两次在**没有任何**清理/归档/压缩/重启日志的
    /// 情况下被整体前移（10-01、10-13 各一次，近千条无声消失），事后无法
    /// 指认元凶。本探针做**对账式监控**：周期采样 条目数 + 头/尾 seq 与
    /// 时间 + 各会话分布 + 三个单调写计数（push / cap-drain / 其余删除），
    /// 并对相邻采样验证恒等式
    ///
    /// ```text
    /// count(now) == count(last) + Δpushed − Δdrained − Δremoved
    /// ```
    ///
    /// 失衡即当场 warn（附全部差值）——把事故窗口压到 ≤ 采样周期；配合
    /// clear / restore / drain 各写路径自身的日志即可指认来源，任何绕过
    /// 这些写路径的整表替换（即使来自当前代码之外的机制）都会触发失衡。
    pub fn probe_timeline(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        // 计数器前后各读一次，一致才算样本（避免与并发写交错造成假失衡；
        // 不一致就跳过本轮，下轮再采）。
        let pushed = self.timeline_pushed.load(Relaxed);
        let drained = self.timeline_drained.load(Relaxed);
        let removed = self.timeline_removed.load(Relaxed);
        let sampled = match self.timeline.try_lock() {
            Ok(timeline) => {
                let count = timeline.len();
                let head_seq = timeline.first().map(|m| m.seq).unwrap_or(0);
                let head_time = timeline.first().map(|m| m.time).unwrap_or(0);
                let tail_seq = timeline.last().map(|m| m.seq).unwrap_or(0);
                let sessions = timeline_sessions_summary(&timeline);
                (count, head_seq, head_time, tail_seq, sessions)
            }
            // 锁被占用：跳过本轮，下轮再采。
            Err(_) => return,
        };
        let (count, head_seq, head_time, tail_seq, sessions) = sampled;
        if self.timeline_pushed.load(Relaxed) != pushed
            || self.timeline_drained.load(Relaxed) != drained
            || self.timeline_removed.load(Relaxed) != removed
        {
            return;
        }
        let rebaseline = self.timeline_rebaseline.swap(false, Relaxed);
        let now = TimelineProbeSample {
            count,
            pushed,
            drained,
            removed,
            head_seq,
        };
        let mut state = self
            .timeline_probe_state
            .lock()
            .expect("timeline probe state poisoned");
        let Some(last) = *state else {
            tracing::info!(
                count,
                head_seq,
                head_time,
                tail_seq,
                sessions = %sessions,
                "timeline probe: baseline established"
            );
            *state = Some(now);
            return;
        };
        if rebaseline {
            tracing::info!(
                count,
                head_seq,
                head_time,
                tail_seq,
                "timeline probe: baseline re-established after full replace"
            );
            *state = Some(now);
            return;
        }
        let expected = last.expected_count_after(&now);
        if expected != count as i64 {
            tracing::warn!(
                expected,
                actual = count,
                last_count = last.count,
                pushed_delta = now.pushed.saturating_sub(last.pushed),
                drained_delta = now.drained.saturating_sub(last.drained),
                removed_delta = now.removed.saturating_sub(last.removed),
                head_seq,
                head_time,
                sessions = %sessions,
                "timeline probe: entry count mismatch — timeline changed outside tracked write paths"
            );
        } else if count != last.count || head_seq != last.head_seq {
            tracing::info!(
                count,
                head_seq,
                head_time,
                tail_seq,
                sessions = %sessions,
                "timeline probe: sample"
            );
        }
        *state = Some(now);
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
            // 归属持久化（2026-09-18 起）：旧档无此字段时保持 None，
            // 由 get_or_create 的补标逻辑或事件归属兜底。
            if let Some(team_id) = s["team_id"].as_str() {
                if session.team_id.is_none() {
                    if let Some(mut entry) = self.identities.get_mut(&session.id) {
                        entry.team_id = Some(team_id.to_string());
                    }
                }
            }
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
        let timeline_entries = restored_timeline.len();
        if let Ok(mut timeline) = self.timeline.try_lock() {
            *timeline = restored_timeline;
        }
        // 整表替换（旧格式迁移）：置位探针重基线标志（见 `probe_timeline`）。
        self.timeline_rebaseline
            .store(true, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(
            entries = timeline_entries,
            "display timeline replaced from legacy document"
        );
        count
    }

    /// Evict identity labels idle longer than TTL. Called periodically from a
    /// background task; only the source label is dropped, the trunk survives.
    ///
    /// **工作区通道常驻**（scope = `workspace`）：它们是用户管理的项目上下文
    /// （每工作区一条、数量有界），不随闲置回收——否则会话列表行消失、
    /// 重建时昵称退化（2026-09-14 激活 = 进入项目对话）。
    pub fn evict_idle(&self) {
        let now = chrono::Utc::now().timestamp_millis();
        self.identities.retain(|_, s| {
            s.session_key.scope == "workspace"
                || now - s.last_active() * 1000 < IDENTITY_IDLE_TTL_MS
        });
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
            dashmap::mapref::entry::Entry::Occupied(mut e) => {
                e.get().touch();
                // 旧会话恢复后再调 set_team_id 的场景（persona 组装顺序）：
                // 既有会话的 team_id 还停留在 None，就地补标当前归属——否则
                // 该 persona 的持久化会话在 Panel 按 team 过滤时全部消失。
                if e.get().team_id.is_none() {
                    if let Some(current) = self.team_id() {
                        e.get_mut().team_id = Some(current);
                    }
                }
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
                    // 每会话一把新锁（2026-10 修复）：同一 **agent 的
                    // `trunk_turn_queue` 曾被克隆给所有会话——所谓"按会话
                    // 排队"实际是 agent 全局串行，默认模式下 local:tui 的
                    // 长任务会阻塞 QQ 回复。现改为每会话独立闸门：同会话
                    // 串行（保持「后到的 turn 看到前一轮回复」语义），
                    // 跨会话并行（与文档承诺一致）。
                    std::sync::Arc::new(tokio::sync::Mutex::new(())),
                );
                v.insert(session.clone());
                session
            }
        }
    }

    /// 确保本地工作区通道会话存在（激活 = 进入项目对话；惰性注册后常驻）。
    ///
    /// 已存在时把昵称刷新为最新工作区名（工作区可改名而 id 不变），
    /// 返回刷新后的会话（带 team_id 归属，供 Panel 过滤展示与广播
    /// `SessionUpdated`）。
    pub fn ensure_workspace_channel(&self, workspace_id: &str, name: &str) -> Session {
        let key = SessionKey::local_workspace(workspace_id);
        let _ = self.get_or_create(&key, name.to_string(), None);
        match self.identities.get_mut(&key.to_session_id()) {
            Some(mut entry) => {
                if !name.is_empty() && entry.nickname != name {
                    entry.nickname = name.to_string();
                }
                entry.touch();
                (*entry).clone()
            }
            // `get_or_create` 刚插入：不可达；兜底再走一次完整路径。
            None => self.get_or_create(&key, name.to_string(), None),
        }
    }

    /// 某会话的上下文投影句柄（不存在时惰性创建空投影）。
    pub fn history_or_create(&self, session_id: &str) -> Arc<parking_lot::Mutex<Vec<ChatMessage>>> {
        if let Some(existing) = self.session_histories.get(session_id) {
            return Arc::clone(existing.value());
        }
        self.session_histories
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(parking_lot::Mutex::new(Vec::new())))
            .value()
            .clone()
    }

    /// 某会话的上下文投影句柄（只读；None = 该会话从未有过历史）。
    pub fn history_for(
        &self,
        session_id: &str,
    ) -> Option<Arc<parking_lot::Mutex<Vec<ChatMessage>>>> {
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
        self.memory_limit_tokens.load(Ordering::Relaxed)
    }

    /// 热更 token 预算（2026-10 上下文审计 🔴 修复）：SaveTeam 保存运行中
    /// persona 时同步——此前预算只在 `TrunkStore::new` 取一次，配置变更要
    /// 重启 agent 才生效。收窄立即重投影裁剪生效；放宽在下一条事件或
    /// 压缩后的重投影自然生效（已被裁掉的旧事件不可恢复——与重启语义一致）。
    pub fn set_memory_limit_tokens(&self, tokens: usize) {
        let old = self.memory_limit_tokens.swap(tokens, Ordering::Relaxed);
        if old != tokens {
            tracing::info!(old, new = tokens, "memory_limit_tokens hot-updated");
            self.reproject_all();
        }
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
            Some(history) => history.lock().clone(),
            None => Vec::new(),
        }
    }

    /// Erase all conversation memory: the durable event log, the in-memory
    /// trunk projection and the display timeline, then persist immediately so
    /// a restart cannot resurrect the cleared history. Identity labels
    /// (provenance metadata) and the session header are kept — they carry no
    /// message content.
    pub async fn clear_history(&self) {
        // 压缩互斥（2026-10 巡检 P1）：compact_history 全程持有 compact_lock
        // （预览→归档→LLM 摘要→replace_all 落地，秒级窗口）。不取锁的话，
        // 清理落在压缩窗口内时，压缩的 replace_all 会把**清理前**的旧事件
        // 重新灌回日志——「清空」被静默复活（清空发生在压缩落地之后才正确）。
        // 与 compact_history 同锁互斥；无嵌套调用（compact 流程不调
        // clear_history），取锁无死锁风险。
        let _compact_guard = self.compact_lock().await;
        let events_cleared = self.event_log.len();
        self.event_log.clear();
        for entry in self.session_histories.iter() {
            entry.value().lock().clear();
        }
        let timeline_cleared = {
            let mut timeline = self.timeline.lock().await;
            let count = timeline.len();
            timeline.clear();
            count
        };
        self.timeline_removed.fetch_add(
            timeline_cleared as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.mark_dirty();
        self.save_now().await;
        tracing::info!(
            events_cleared,
            timeline_cleared,
            "clear_history: all sessions wiped (events + projections + timeline)"
        );
    }

    /// 清理**单个会话**的记忆：事件日志中该会话的事件、该会话的上下文
    /// 投影、时间线中该会话的条目。
    ///
    /// 2026-10 用户报告修复：面板「清理历史记忆」此前映射到
    /// [`Self::clear_history`]（全量）——同一人格下其他会话（QQ 群/私聊、
    /// 其他 workspace 会话）的记忆被一并抹掉。现在 UI 走本方法按
    /// `session_id` 精确清理；全量版本保留（显式全清的管理语义）。
    ///
    /// - `session == None` 的旧版事件不删（无归属信息，宁保守不误删）；
    /// - 压缩互斥同 [`Self::clear_history`]（清理与压缩落地竞争同样会
    ///   让旧事件被 replace_all 复活）。
    pub async fn clear_session(&self, session_id: &str) {
        let _compact_guard = self.compact_lock().await;
        let removed = self.event_log.retain_excluding_session(session_id);
        // 2026-10 上下文审计 🔴 修复：不能只 remove 映射——身份对象
        // `Session.history` 仍持有同一 Vec 的 Arc（`get_or_create` 命中既有
        // 身份时原样返回，`process_message` 读的也是它）。仅 remove 会让
        // 清理后的旧内容继续进下一次 LLM 请求，且此后 `history_or_create`
        // 新建的空投影与旧 Arc 永久脱节（新消息"失忆"）。与全量
        // `clear_history` 的原地 `clear()` 语义对齐：先清空既有 Arc 再摘映射。
        if let Some((_, history)) = self.session_histories.remove(session_id) {
            history.lock().clear();
        }
        let (timeline_removed, timeline_remaining) = {
            let mut timeline = self.timeline.lock().await;
            let before = timeline.len();
            timeline.retain(|entry| entry.session_id != session_id);
            (before - timeline.len(), timeline.len())
        };
        self.timeline_removed.fetch_add(
            timeline_removed as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.mark_dirty();
        self.save_now().await;
        tracing::info!(
            session = %session_id,
            removed_events = removed,
            timeline_removed,
            timeline_remaining,
            "session memory cleared (scoped)"
        );
    }

    /// 归档保留上限（archives/ 下本 stem 的归档文件数，最旧先删；
    /// 2026-10 议题：此前归档只增不减，一年可达数百 MB）。
    pub const DEFAULT_ARCHIVE_MAX_FILES: usize = 20;

    /// 归档文件名是否属于本 stem：`{stem}-{8位日期}-{6位时间}[-{reason}].json`。
    ///
    /// 2026-10 审计：前缀匹配必须**锚定时间戳段**——`starts_with("{stem}-")`
    /// 会让 stem `x` 误统计/删除 `x-y` 的归档（改名/新增 agent 后旧 stem
    /// 归档被连带清理）。
    fn is_own_archive_name(name: &str, stem: &str) -> bool {
        let Some(rest) = name.strip_prefix(stem) else {
            return false;
        };
        let Some(rest) = rest.strip_prefix('-') else {
            return false;
        };
        let Some(stamp) = rest.strip_suffix(".json") else {
            return false;
        };
        // stamp: `20261013-074421` / `20261013-074421-123`（2026-10 起带毫秒）
        // 或追加 `-{reason}`（如 `-precompact`）。
        let bytes = stamp.as_bytes();
        if bytes.len() < 15 || bytes[8] != b'-' {
            return false;
        }
        if !bytes[..8].iter().all(u8::is_ascii_digit)
            || !bytes[9..15].iter().all(u8::is_ascii_digit)
        {
            return false;
        }
        // 第 15 位之后：直接结尾（旧格式）/ `-{reason}`（旧）/
        // `-{3位毫秒}`（新）/ `-{3位毫秒}-{reason}`（新）。
        match stamp.len() {
            15 => true,
            19 => bytes[15] == b'-' && bytes[16..19].iter().all(u8::is_ascii_digit),
            _ if bytes[15] == b'-' => {
                // 新格式带 reason：`…-{3位毫秒}-{reason}`
                if bytes[16..19].iter().all(u8::is_ascii_digit) {
                    bytes.len() > 19 && bytes[19] == b'-'
                } else {
                    // 旧格式带 reason：`…-{reason}`
                    true
                }
            }
            _ => false,
        }
    }

    /// 归档保留清理：`archives/` 下**本 stem**（时间戳锚定）的归档按
    /// 文件名内嵌时间戳（字典序 = 时间序）保留最新 `max` 份，多余的最旧
    /// 先删（删除失败仅记日志不阻塞归档）。
    pub fn prune_archives(archive_dir: &std::path::Path, stem: &str, max: usize) {
        if max == 0 || !archive_dir.is_dir() {
            return;
        }
        let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
        if let Ok(read) = std::fs::read_dir(archive_dir) {
            for entry in read.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if !Self::is_own_archive_name(name, stem) {
                    continue;
                }
                entries.push((name.to_string(), entry.path()));
            }
        }
        if entries.len() <= max {
            return;
        }
        // 2026-10 上下文审计 🔵 修复：按文件名排序而非 mtime——归档名内嵌
        // 时间戳（字典序 = 时间序），文件被复制/恢复后 mtime 失真会淘汰错
        // 目标。
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let doomed = entries.len() - max;
        for (_, path) in entries.into_iter().take(doomed) {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!(path = %path.display(), %e, "archive prune failed");
            } else {
                tracing::info!(path = %path.display(), "old archive pruned");
            }
        }
    }

    /// 归档会话（整档）：把**内存快照**（序列化当前态）复制到
    /// `archives/{stem}-{ts}.json`，再清空全部历史（归档后从头开始）。
    /// 返回归档文件路径。
    ///
    /// 2026-10 审计修复两处：
    /// - 归档源从"复制磁盘文件"改为内存快照——磁盘最多落后 30s 保存
    ///   周期，窗口内已接收的事件此前"未归档即被清"，归档与日志双失；
    /// - 提示行走统一 `push_timeline_blocking`（seq / 计数 / 上限），
    ///   不再绕过探针对账（见 [`Self::push_timeline_blocking`]）。
    pub async fn archive_history(&self) -> Result<String, String> {
        if !self.has_archivable_content() {
            return Err("没有可归档的历史（会话为空）".into());
        }
        let data = self
            .serialize()
            .ok_or_else(|| "会话快照序列化失败（时间线锁被占用）".to_string())?;
        let archive = self.write_archive_file(&data, None)?;
        self.clear_history().await;
        self.push_timeline_blocking(crate::event::TimelineMessage::system(
            format!("历史已归档：{archive}"),
            String::new(),
            chrono::Utc::now().timestamp(),
        ))
        .await;
        // 提示行落盘：clear_history 内部 save_now 先于 push 完成，
        // 不落盘则重启后提示行消失（2026-10 归档巡检发现的生产 bug）。
        self.mark_dirty();
        self.save_now().await;
        Ok(archive)
    }

    /// 归档**单个会话**：先写整档快照（与 [`Self::archive_history`] 同一
    /// 文件形态——恢复时可直接作为会话文件手工换回），再只清空该会话的
    /// 历史；其他会话不受影响。
    ///
    /// 面板「归档」按钮的会话级语义（2026-10 审计：此前该命令无 session
    /// 维度，点一次把同人格**所有**会话一并清空——与面板"归档当前会话"
    /// 的承诺不符，属「清理历史记忆」同类漏网的误清）。
    pub async fn archive_session(&self, session_id: &str) -> Result<String, String> {
        if !self.has_archivable_content() {
            return Err("没有可归档的历史（会话为空）".into());
        }
        let data = self
            .serialize()
            .ok_or_else(|| "会话快照序列化失败（时间线锁被占用）".to_string())?;
        let archive = self.write_archive_file(&data, None)?;
        self.clear_session(session_id).await;
        self.push_timeline_blocking(crate::event::TimelineMessage::system(
            format!("会话历史已归档：{archive}（会话 {session_id}）"),
            String::new(),
            chrono::Utc::now().timestamp(),
        ))
        .await;
        self.mark_dirty();
        self.save_now().await;
        Ok(archive)
    }

    /// 压缩计划（多会话，2026-09）：**按会话分别**计算该会话前
    /// `keep_recent` 条之外、将被 Compaction 摘要替换的事件——纯只读，
    /// 不改动日志。调用方据此先归档、再逐组生成摘要，最后
    /// [`Self::apply_compaction`] 落地。
    ///
    /// 未归因的旧事件单独成组（None 键，与日志同序）。
    pub fn compact_preview(&self, keep_recent: usize) -> Result<CompactPreview, String> {
        let events = self.event_log.log();
        if events.len() <= keep_recent + 1 {
            return Err(format!("历史不足（{} 条事件，无需压缩）", events.len()));
        }
        let total_events = events.len();
        let mut groups = Vec::new();
        let mut total_replaced = 0usize;
        for (key, group) in group_events_by_session(&events) {
            if group.len() <= keep_recent + 1 {
                continue;
            }
            let replaced_count = group.len() - keep_recent;
            let replaced = group[..replaced_count].to_vec();
            let tools = replaced
                .iter()
                .filter(|e| matches!(e, echo_session::SessionEvent::ToolCall(_)))
                .count();
            let users = replaced
                .iter()
                .filter(|e| matches!(e, echo_session::SessionEvent::UserMessage(_)))
                .count();
            total_replaced += replaced_count;
            groups.push(CompactGroup {
                session: key,
                replaced,
                replaced_count,
                users,
                tools,
            });
        }
        if total_replaced == 0 {
            return Err(format!("历史不足（{total_events} 条事件，无需压缩）"));
        }
        Ok(CompactPreview {
            groups,
            total_replaced,
            total_events,
        })
    }

    /// 归档前的空档防守：事件日志与显示时间线都为空时无可归档内容。
    /// （时间线锁被占用时保守放行——宁可行，不误拦。）
    fn has_archivable_content(&self) -> bool {
        if !self.event_log.is_empty() {
            return true;
        }
        self.timeline
            .try_lock()
            .map(|timeline| !timeline.is_empty())
            .unwrap_or(true)
    }

    /// 把序列化数据写入 `archives/{stem}-{ts}[-{suffix}].json`，并按
    /// [`Self::DEFAULT_ARCHIVE_MAX_FILES`] 做保留清理。返回归档文件路径。
    ///
    /// 2026-10 审计：归档数据统一**来自内存快照**（调用方先 `serialize()`），
    /// 不再复制磁盘文件——磁盘最多落后 30s 保存周期，此前 ArchiveHistory
    /// 走磁盘会在"归档后立即清空"时让窗口内已接收事件"未归档即被清"，
    /// 归档与日志双失。
    fn write_archive_file(&self, data: &str, suffix: Option<&str>) -> Result<String, String> {
        let path = self
            .persist_path
            .lock()
            .expect("persist path poisoned")
            .clone()
            .ok_or_else(|| "没有可归档的会话文件（未启用持久化）".to_string())?;
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let archive_dir = dir.join("archives");
        std::fs::create_dir_all(&archive_dir)
            .map_err(|e| format!("create archives dir failed: {e}"))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        // 2026-10 上下文审计 🟡 修复：时间戳补毫秒段——秒级精度下同秒两次
        // 归档（压缩前自动归档 + 手动归档连点）会同名覆盖、静默丢一份。
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f");
        let name = match suffix {
            Some(suffix) => format!("{stem}-{ts}-{suffix}.json"),
            None => format!("{stem}-{ts}.json"),
        };
        let archive = archive_dir.join(name);
        std::fs::write(&archive, data).map_err(|e| format!("write archive failed: {e}"))?;
        Self::prune_archives(&archive_dir, stem, Self::DEFAULT_ARCHIVE_MAX_FILES);
        Ok(archive.display().to_string())
    }

    /// 归档当前会话文件的**内存快照**到 `archives/{stem}-{ts}-{reason}.json`。
    ///
    /// 压缩前调用：快照用序列化出的当前态（比直接复制磁盘文件准——磁盘
    /// 可能落后一次 tick）。未配置持久化路径时返回 Err（纯内存部署无档
    /// 可归，调用方继续压缩、只在结果里注明）。
    pub async fn archive_snapshot(&self, reason: &str) -> Result<String, String> {
        let data = self
            .serialize()
            .ok_or_else(|| "会话快照序列化失败（时间线锁被占用）".to_string())?;
        self.write_archive_file(&data, Some(reason))
    }

    /// 落地压缩：把各组前缀替换为其摘要（Compaction 事件带会话归属与
    /// 归档路径），投影随之更新并立即持久化。
    ///
    /// 切片口径（2026-10 审计 🔴 修复两处读-改-写问题）：
    /// - 每组替换**预览时算好的前缀长度**（[`CompactionSummary::replaced_count`]），
    ///   不再按"应用时点 `len - keep_recent`"重算——摘要生成窗口内新到的
    ///   k 条事件会把旧保留尾最老 k 条挤进被替换前缀而**无摘要**（静默
    ///   丢档，仅归档可救）；改用预览切片后，窗口期事件全部留在保留尾。
    /// - 整个读-改-写在 [`echo_session::EventLog::transform`] 的单次取锁内
    ///   完成——"克隆 → 替换"两步之间并发 append 被整体覆盖丢弃的窗口
    ///   （含整日志克隆 + 按会话重排，长会话可达数十 ms）随之关闭。
    ///
    /// `summaries` 按会话键匹配预览分组；缺失的组保持原样（不压缩）。
    /// `keep_recent` 仅用于结果文案（保留尾实际 ≥ 该值）。
    pub async fn apply_compaction(
        &self,
        keep_recent: usize,
        summaries: &[CompactionSummary],
        archive: Option<&str>,
    ) -> Result<CompactionApplied, String> {
        let outcome = self.event_log.transform(|events| {
            let mut new_log: Vec<echo_session::SessionEvent> = Vec::new();
            let mut total_replaced = 0usize;
            let mut groups_done = 0usize;
            for (key, group) in group_events_by_session(events) {
                let prepared = summaries.iter().find(|s| s.session == key);
                let Some(prepared) = prepared else {
                    new_log.extend(group);
                    continue;
                };
                let replace = prepared.replaced_count;
                if replace == 0 || replace >= group.len() {
                    // 组在预览后被外部改动（append-only 下理论不会收缩）：
                    // 保守跳过——宁可不压缩，也不把无摘要内容换出。
                    tracing::warn!(
                        session = ?key,
                        preview_replaced = replace,
                        group_len = group.len(),
                        "compaction: group changed since preview; skipping group"
                    );
                    new_log.extend(group);
                    continue;
                }
                new_log.push(echo_session::SessionEvent::Compaction(
                    echo_session::CompactionEvent {
                        replaced_count: replace,
                        summary: prepared.summary.clone(),
                        archive: archive.map(str::to_string),
                        session: key.clone(),
                    },
                ));
                new_log.extend(group[replace..].to_vec());
                total_replaced += replace;
                groups_done += 1;
            }
            if total_replaced == 0 {
                return None;
            }
            Some((new_log, (total_replaced, groups_done)))
        });
        let Some((total_replaced, groups_done)) = outcome else {
            return Err(format!(
                "历史不足（{} 条事件，无需压缩）",
                self.event_log.len()
            ));
        };
        // 变换已原子提交；投影重建并立即持久化。
        self.reproject_all();
        let archive_note = archive
            .map(|p| format!("，原事件已归档：{p}"))
            .unwrap_or_default();
        self.push_timeline_blocking(crate::event::TimelineMessage::system(
            format!(
                "历史已压缩：{total_replaced} 条事件 → {groups_done} 条摘要（各会话保留最近 ≥{keep_recent} 条）{archive_note}"
            ),
            String::new(),
            chrono::Utc::now().timestamp(),
        ))
        .await;
        self.mark_dirty();
        self.save_now().await;
        Ok(CompactionApplied {
            total_replaced,
            groups: groups_done,
            kept: keep_recent,
        })
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
        // 先在锁外完成投影。
        let mut projected = echo_session::derive::derive_messages_for(
            log,
            Some(session_id),
            self.memory_limit_tokens.load(Ordering::Relaxed),
        );
        // 媒体引用 → data URI：事件日志/时间线里只存 `/media/<id>`（轻量），
        // 发往 LLM 的 `ChatMessage` 在这一步（唯一的投影出口）还原图片。
        echo_defs::media_store::inline_media_refs_in_messages(&mut projected);
        let Some(entry) = self.session_histories.get(session_id) else {
            return;
        };
        let handle = Arc::clone(entry.value());
        drop(entry);
        // 2026-10 上下文审计 🟡 修复：此前 `try_lock` 竞争失败即静默跳过，
        // 压缩后的 `reproject_all` 若撞上读锁（`process_message` 正持有快照），
        // 该会话投影会停留在压缩前、直到下一条事件或重启才自愈。history 锁
        // 已改为 `parking_lot::Mutex`（临界区均为纯内存克隆/赋值、无跨 await
        // 持锁），同步阻塞取锁保证投影收敛。
        let mut guard = handle.lock();
        *guard = projected;
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

    /// 从事件日志里的会话 id 补建缺失的身份条目（含归属）。
    ///
    /// 事件是事实来源：identities 元数据可能缺失（旧档/写入失败），但事件
    /// 里的 `session` 归属足以把会话恢复到注册表——否则 Panel 的会话
    /// 列表丢失这些会话（入口行「会话」按钮不出现）。
    fn ensure_identities_for_events(&self, events: &[echo_session::SessionEvent]) {
        let current_team = self.team_id();
        for event in events {
            let Some(id) = event.session() else {
                continue;
            };
            if self.identities.contains_key(id) {
                continue;
            }
            let Some(key) = SessionKey::parse(id) else {
                continue;
            };
            let session = Session::with_trunk(
                &key,
                current_team.clone(),
                String::new(),
                None,
                self.history_or_create(id),
                Arc::clone(&self.trunk_turn_lock),
                // 每会话独立闸门（同上）。
                std::sync::Arc::new(tokio::sync::Mutex::new(())),
            );
            self.identities.insert(id.to_string(), session);
        }
    }

    /// The full event log (oldest first) — the durable source of truth.
    pub fn event_log(&self) -> Vec<echo_session::SessionEvent> {
        self.event_log.log()
    }

    /// 导入某会话的完整事件集（会话迁移 P3-2b）：追加到事实来源日志并重投影
    /// 该会话（物化其历史句柄）。调用方保证事件已归因到 `session_id` 且不重复
    /// （重复导入会重复追加——幂等性由迁移层的 transfer_id 去重负责）。
    pub fn import_session_events(&self, session_id: &str, events: Vec<echo_session::SessionEvent>) {
        if !events.is_empty() {
            self.event_log.extend(events);
        }
        let _ = self.history_or_create(session_id);
        self.reproject(&Some(session_id.to_string()));
        self.mark_dirty();
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
        "team_id": session.team_id,
        "platform": session.session_key.platform,
        "scope": session.session_key.scope,
        "scope_id": session.session_key.scope_id,
        "user_id": session.session_key.user_id,
        "nickname": session.nickname,
        "group_name": session.group_name,
        "last_active": session.last_active(),
    })
}

/// v4 legacy 序列化对照（仅测试用；v6 持久化不再写 per-session 投影）。
#[cfg(test)]
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
/// 加载期媒体迁移：把事件里内嵌的大图落盘并改写为 `/media/<id>` 引用。
///
/// 返回是否有改动（改动需 `mark_dirty` 以便下一次持久化收缩文件）。
/// 落盘失败时省略该图（`ELIDED_IMAGE`），不阻塞加载。
fn spill_event_media(events: &mut [echo_session::SessionEvent]) -> bool {
    let mut changed = false;
    for event in events.iter_mut() {
        let images = match event {
            echo_session::SessionEvent::UserMessage(message) => {
                // 先把 content 里内嵌的 data URI 落盘（QQ hook JSON 原样
                // 序列化了 images，此前一份图片在日志里存在三份：content
                // 内嵌 + images 字段 + 时间线 images）。
                if let Some(rewritten) =
                    echo_defs::media_store::spill_inline_data_uris(&message.content)
                {
                    message.content = rewritten;
                    changed = true;
                }
                Some(&mut message.images)
            }
            echo_session::SessionEvent::ToolResult(result) => {
                if let Some(rewritten) =
                    echo_defs::media_store::spill_inline_data_uris(&result.result)
                {
                    result.result = rewritten;
                    changed = true;
                }
                Some(&mut result.images)
            }
            _ => None,
        };
        let Some(images) = images else { continue };
        for image in images.iter_mut() {
            if !image.starts_with("data:") {
                continue;
            }
            let migrated = echo_defs::media_store::spill_or_keep(image);
            if migrated != *image {
                *image = migrated;
                changed = true;
            }
        }
    }
    changed
}

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
                    elapsed_ms: None,
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

    /// P3-2b：迁移导入把事件追加进事实来源日志并物化历史句柄。
    #[test]
    fn import_session_events_appends_to_fact_log() {
        let store = TrunkStore::new(100_000);
        let sid = "local:tui::local_user";
        let event = echo_session::SessionEvent::UserMessage(echo_session::UserMessage {
            content: "hello".into(),
            timestamp: 1,
            message_sequence: Some(1),
            source: None,
            images: Vec::new(),
            session: Some(sid.into()),
        });
        store.import_session_events(sid, vec![event.clone()]);
        assert_eq!(store.event_log(), vec![event]);
        assert!(store.history_for(sid).is_some());
    }

    // ── federation Phase 0：node:// 命名空间 ──

    #[test]
    fn parse_strips_node_prefix_for_local_handling() {
        let key =
            SessionKey::parse("node://node-abc/qq:group:987:123").expect("qualified id parses");
        assert_eq!(key.platform, "qq");
        assert_eq!(key.scope, "group");
        assert_eq!(key.scope_id, "987");
        assert_eq!(key.user_id, "123");
        // 本机 id 完全不受新前缀逻辑影响
        let plain = SessionKey::parse("qq:group:987:123").unwrap();
        assert_eq!(key, plain);
    }

    #[test]
    fn parse_qualified_preserves_node_attribution() {
        let (node, key) =
            SessionKey::parse_qualified("node://node-abc/local:workspace:ws1:local_user")
                .expect("qualified parse");
        assert_eq!(node.as_deref(), Some("node-abc"));
        assert_eq!(key.scope, "workspace");

        let (node, key) = SessionKey::parse_qualified("local:tui::local_user").unwrap();
        assert!(node.is_none());
        assert_eq!(key, SessionKey::local_tui());
    }

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
                started_at_ms: None,
            }),
            echo_session::SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "c1".into(),
                result: "sent".into(),
                images: vec![],
                session: None,
                elapsed_ms: None,
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
    fn session_key_local_workspace_roundtrip() {
        let key = SessionKey::local_workspace("echo-agent");
        assert_eq!(key.to_session_id(), "local:workspace:echo-agent:local_user");
        // parse 往返：scope=workspace / scope_id=工作区 id
        let parsed = SessionKey::parse("local:workspace:echo-agent:local_user").unwrap();
        assert_eq!(parsed.platform, "local");
        assert_eq!(parsed.scope, "workspace");
        assert_eq!(parsed.scope_id, "echo-agent");
        assert_eq!(parsed.user_id, "local_user");
        assert_eq!(parsed.to_session_id(), key.to_session_id());
    }

    #[test]
    fn session_key_parse_malformed() {
        assert!(SessionKey::parse("abc").is_none());
        assert!(SessionKey::parse("a:b").is_none());
    }

    /// 工作区通道不随闲置回收（列表行常驻），普通身份照常回收。
    #[tokio::test]
    async fn evict_idle_keeps_workspace_channels() {
        let store = TrunkStore::new(1000);
        let channel = store.ensure_workspace_channel("proj", "Proj");
        let plain =
            store.get_or_create(&SessionKey::parse("qq:dm::111").unwrap(), "a".into(), None);
        let past = chrono::Utc::now().timestamp() - (IDENTITY_IDLE_TTL_MS / 1000) * 2;
        channel
            .last_active
            .store(past, std::sync::atomic::Ordering::Relaxed);
        plain
            .last_active
            .store(past, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(store.len(), 2);
        store.evict_idle();
        assert!(
            store.get("local:workspace:proj:local_user").is_some(),
            "workspace channel survives eviction"
        );
        assert!(store.get("qq:dm::111").is_none(), "stale identity evicted");
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
        assert_eq!(local.history.lock()[0].content, "local only");
        assert!(qq.history.lock().is_empty(), "qq context untouched");
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
            root.get("trunk_histories").is_none(),
            "per-session projections are derived from the event log, not persisted"
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
        let hist = s2.history.lock().clone();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].content, "你好");
        assert_eq!(hist[1].content, "回复");

        let _ = std::fs::remove_file(&path);
    }

    // ── 媒体落盘迁移（2026-09-24）──
    //
    // 这两个用例改的是进程级环境变量 `ECHO_MEDIA_DIR`，必须串行执行。
    // tokio Mutex：其中一个用例是 async（持锁跨 await），std 锁会触发
    // clippy::await_holding_lock；两者语义一致（仍按进程级环境变量串行）。
    static MEDIA_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn spill_event_media_migrates_inline_images_to_refs() {
        let _serial = MEDIA_ENV_LOCK.blocking_lock();
        use echo_session::event::{ToolResultEvent, UserMessage};
        let dir = std::env::temp_dir().join(format!(
            "echo-session-media-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::env::set_var("ECHO_MEDIA_DIR", &dir);

        // 超过落盘阈值的 data URI（~5.4KB base64）
        let payload = echo_defs::media_store::tests_support_base64(vec![7u8; 4096]);
        let data_uri = format!("data:image/png;base64,{payload}");
        // 真实形态：content 是 hook JSON（内嵌 images 的 data URI）+ images 字段
        let content = format!(
            "<qq_message_hook>\n{{\"content\":\"看图\",\"images\":[\"{data_uri}\"]}}\n</qq_message_hook>"
        );
        let mut events = vec![
            echo_session::SessionEvent::UserMessage(UserMessage {
                content,
                timestamp: 0,
                message_sequence: None,
                source: None,
                images: vec![data_uri.clone(), "https://example.com/a.png".into()],
                session: Some("local:tui::local_user".into()),
            }),
            echo_session::SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "c1".into(),
                result: "ok".into(),
                images: vec![data_uri.clone()],
                session: Some("local:tui::local_user".into()),
                elapsed_ms: None,
            }),
        ];
        let changed = spill_event_media(&mut events);
        std::env::remove_var("ECHO_MEDIA_DIR");

        assert!(changed, "migration reports change");
        let (content, images) = match &events[0] {
            echo_session::SessionEvent::UserMessage(m) => (&m.content, &m.images),
            other => panic!("unexpected: {other:?}"),
        };
        // content 里的内嵌 data URI 也被改写为引用（QQ hook JSON 场景）
        assert!(content.contains("/media/"), "content: {content}");
        assert!(!content.contains("data:image"), "content payload gone");
        assert!(images[0].starts_with("/media/"), "{:?}", images[0]);
        assert!(images[0].ends_with(".png"));
        // 远端 URL 原样保留
        assert_eq!(images[1], "https://example.com/a.png");
        let tool_images = match &events[1] {
            echo_session::SessionEvent::ToolResult(r) => &r.images,
            other => panic!("unexpected: {other:?}"),
        };
        assert!(tool_images[0].starts_with("/media/"));

        // 幂等：再跑一次没有改动
        std::env::set_var("ECHO_MEDIA_DIR", &dir);
        assert!(!spill_event_media(&mut events));
        std::env::remove_var("ECHO_MEDIA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn persisted_refs_inline_on_projection_for_model() {
        let _serial = MEDIA_ENV_LOCK.lock().await;
        // 事件日志里存引用（轻量）；投影（模型上下文）出口还原为 data URI。
        let dir = std::env::temp_dir().join(format!(
            "echo-session-inline-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::env::set_var("ECHO_MEDIA_DIR", &dir);
        let bytes = vec![9u8; 4096];
        let id = echo_defs::media_store::save_image_bytes(&bytes, "image/png").unwrap();
        let reference = echo_defs::media_store::media_ref(&id);

        let store = TrunkStore::new(10_000);
        let _session = store.get_or_create(&SessionKey::local_tui(), "local".into(), None);
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                content: "看图".into(),
                timestamp: 0,
                message_sequence: None,
                source: None,
                images: vec![reference.clone()],
                session: Some("local:tui::local_user".into()),
            },
        ));
        let snapshot = store.snapshot_for("local:tui::local_user").await;
        std::env::remove_var("ECHO_MEDIA_DIR");

        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].images.len(), 1, "image survives projection");
        assert!(
            snapshot[0].images[0].starts_with("data:image/png;base64,"),
            "refs inline into data URI for the model: {}",
            snapshot[0].images[0]
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!(local.history.lock().len(), 2);
        assert!(qq.history.lock().is_empty(), "qq context stays empty");
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
        let timeline = store.timeline_snapshot().unwrap_or_default();
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
        assert_eq!(store.timeline_snapshot().map(|t| t.len()), Some(1));

        store.clear_history().await;

        assert_eq!(store.trunk_len(), 0, "trunk projection cleared");
        assert!(store.event_log().is_empty(), "event log cleared");
        assert!(
            store.timeline_snapshot().is_none_or(|t| t.is_empty()),
            "timeline cleared"
        );
        // The persisted file must already reflect the wipe.
        let restored = TrunkStore::new(1000);
        restored.set_persist_path(&path);
        restored.load_from_file().await;
        assert_eq!(restored.trunk_len(), 0, "cleared history survives restart");
        assert!(restored.timeline_snapshot().is_none_or(|t| t.is_empty()));
        let _ = std::fs::remove_file(&path);
    }

    /// 会话级清理（2026-10 用户报告回归）：清一个会话，**另一会话完好**。
    ///
    /// 背景：面板「清理历史记忆」此前映射到全量 `clear_history()`——
    /// 同一人格下其他会话（QQ 群/私聊、其他 workspace 会话）的记忆被一并
    /// 抹掉。本测试钉住 `clear_session` 的精确性：事件日志、上下文投影、
    /// 显示时间线三面都只清目标会话，且磁盘持久化一致。
    #[tokio::test]
    async fn clear_session_only_wipes_target_and_keeps_siblings() {
        let path = temp_sessions_path("clear-session");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(1000);
        store.set_persist_path(&path);

        let user_event = |session: &str, content: &str, ts: i64| {
            echo_session::SessionEvent::UserMessage(echo_session::event::UserMessage {
                session: Some(session.into()),
                content: content.into(),
                timestamp: ts,
                message_sequence: None,
                source: None,
                images: vec![],
            })
        };
        let timeline_msg = |session: &str, content: &str, ts: i64| crate::event::TimelineMessage {
            kind: "user".into(),
            content: content.into(),
            session_id: session.into(),
            time: ts,
            source: None,
            reasoning: None,
            tool: None,
            images: None,
            seq: 0,
        };

        // 会话 A（待清理）与 B（必须完好）各写事件 + 时间线条目。
        store.append_event(user_event(
            "local:workspace:proj:alice",
            "A-记忆",
            1700000001,
        ));
        store.append_event(user_event("qq:group:123:456", "B-记忆", 1700000002));
        store.push_timeline(timeline_msg(
            "local:workspace:proj:alice",
            "A-线",
            1700000001,
        ));
        store.push_timeline(timeline_msg("qq:group:123:456", "B-线", 1700000002));
        store.save_now().await;
        assert_eq!(store.event_log().len(), 2);

        store.clear_session("local:workspace:proj:alice").await;

        // ① 事件日志：只剩 B。
        let log = store.event_log();
        assert_eq!(log.len(), 1, "只应保留另一会话的事件: {log:?}");
        assert_eq!(log[0].session(), Some("qq:group:123:456"));
        // ② 上下文投影：A 的投影条目已删、B 的保留。
        let a_cleared = match store.history_for("local:workspace:proj:alice") {
            Some(h) => h.lock().is_empty(),
            None => true,
        };
        assert!(a_cleared, "目标会话投影应清空");
        // ③ 时间线：只剩 B 的条目。
        let timeline = store.timeline_snapshot().unwrap_or_default();
        assert_eq!(timeline.len(), 1, "时间线只应保留另一会话条目");
        assert_eq!(timeline[0].session_id, "qq:group:123:456");

        // ④ 持久化：重启加载后同样只有 B（清理落盘）。
        let restored = TrunkStore::new(1000);
        restored.set_persist_path(&path);
        restored.load_from_file().await;
        let restored_log = restored.event_log();
        assert_eq!(restored_log.len(), 1, "磁盘应只存另一会话");
        assert!(
            restored
                .timeline_snapshot()
                .is_some_and(|t| t.iter().all(|m| m.session_id == "qq:group:123:456")),
            "重启后时间线也不含被清会话"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 2026-10 上下文审计 🔴 回归：会话级清理后，身份对象持有的**同一
    /// Arc** 必须同步清空——否则 `get_or_create` 命中的旧 `Session.history`
    /// 把已清理的内容带进下一次 LLM 请求（清理"复活"），且新投影写进新
    /// Arc 与旧 Arc 永久脱节（新消息"失忆"）。
    #[tokio::test]
    async fn clear_session_empties_the_shared_history_arc() {
        let store = TrunkStore::new(1000);
        let key = SessionKey::parse("local:workspace:proj:alice").unwrap();
        // 身份 + 事件 + 投影就位；`session` 持有 history Arc 的一份拷贝。
        let session = store.get_or_create(&key, "alice".into(), None);
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some(session.id.clone()),
                content: "被清理的记忆".into(),
                timestamp: 1700000001,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
        assert!(!session.history.lock().is_empty(), "投影已物化");

        store.clear_session(&session.id).await;

        // 身份持有的旧 Arc 已原地清空（复活路径切断）。
        assert!(
            session.history.lock().is_empty(),
            "clear_session 必须清空身份持有的共享 Arc"
        );
        // 新事件写入后：投影重新物化且只含新内容（新旧 Arc 未脱节）。
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some(session.id.clone()),
                content: "清理后的新消息".into(),
                timestamp: 1700000002,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
        let history = store.history_for(&session.id).expect("投影重建");
        let contents: Vec<String> = history.lock().iter().map(|m| m.content.clone()).collect();
        assert!(
            contents.iter().all(|c| !c.contains("被清理的记忆")),
            "旧内容不得复活: {contents:?}"
        );
        assert!(
            contents.iter().any(|c| c.contains("清理后的新消息")),
            "新消息应进投影: {contents:?}"
        );
    }

    /// 旧版无归属事件（session=None）在会话级清理中**不被误删**（保守语义）。
    #[tokio::test]
    async fn clear_session_keeps_legacy_unattributed_events() {
        let path = temp_sessions_path("clear-session-legacy");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(1000);
        store.set_persist_path(&path);

        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some("s1".into()),
                content: "归属 s1".into(),
                timestamp: 1,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: None, // 旧版事件：无归属
                content: "无归属".into(),
                timestamp: 2,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));

        store.clear_session("s1").await;

        let log = store.event_log();
        assert_eq!(log.len(), 1, "仅删该会话事件，旧版无归属事件保留: {log:?}");
        assert_eq!(log[0].session(), None);
        let _ = std::fs::remove_file(&path);
    }

    /// 时间线写路径计数（探针基础设施）：push / cap-drain / 会话级删除三个
    /// 单调计数与真实变化一致——[`TrunkStore::probe_timeline`] 的对账恒等式
    /// 依赖它们。
    #[tokio::test]
    async fn timeline_probe_counters_track_writes() {
        use std::sync::atomic::Ordering::Relaxed;
        let store = TrunkStore::new(1000);
        let message = |session: &str| crate::event::TimelineMessage {
            kind: "user".into(),
            content: "x".into(),
            session_id: session.into(),
            time: 1,
            source: None,
            reasoning: None,
            tool: None,
            images: None,
            seq: 0,
        };
        for i in 0..(TRUNK_TIMELINE_MAX + 3) {
            store.push_timeline(message(if i % 2 == 0 { "sA" } else { "sB" }));
        }
        assert_eq!(
            store.timeline_pushed.load(Relaxed),
            (TRUNK_TIMELINE_MAX + 3) as u64
        );
        assert_eq!(store.timeline_drained.load(Relaxed), 3, "cap-drain counted");
        store.clear_session("sA").await;
        assert!(
            store.timeline_removed.load(Relaxed) > 0,
            "session-scoped timeline removal counted"
        );
        // 冒烟：探针可执行、不 panic（是否告警由日志观察）。
        store.probe_timeline();
    }

    /// 探针对账恒等式：期望条目数 = 上次 + Δpush − Δdrain − Δremove；
    /// 条目"绕过写路径"地减少时恒等式失衡（探针据此告警，2026-10 时间线
    /// 无声消失事故定位基建）。
    #[test]
    fn timeline_probe_accounting_identity() {
        let last = TimelineProbeSample {
            count: 10,
            head_seq: 5,
            pushed: 10,
            drained: 0,
            removed: 0,
        };
        let now = TimelineProbeSample {
            count: 10,
            head_seq: 5,
            pushed: 13,
            drained: 1,
            removed: 2,
        };
        assert_eq!(last.expected_count_after(&now), 10, "10 + 3 − 1 − 2");
        let vanished = TimelineProbeSample { count: 3, ..now };
        assert_ne!(
            last.expected_count_after(&vanished),
            3,
            "无声减少必须被对账检出"
        );
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
        let timeline = restored.timeline_snapshot().unwrap_or_default();
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
        assert!(
            store.timeline_snapshot().is_none_or(|t| t.is_empty()),
            "no timeline → empty"
        );
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
        let cursor = store
            .timeline_snapshot()
            .unwrap_or_default()
            .first()
            .unwrap()
            .seq
            - 1;
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
        let timeline = store.timeline_snapshot().unwrap_or_default();
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

    // ── 压缩计划 / 归档快照 / 落地（2026-09 LLM 摘要改造）──

    fn ev_user(session: &str, content: &str) -> echo_session::SessionEvent {
        echo_session::SessionEvent::UserMessage(echo_session::event::UserMessage {
            session: Some(session.into()),
            content: content.into(),
            timestamp: 1700000000,
            message_sequence: None,
            source: None,
            images: vec![],
        })
    }

    fn ev_tool(session: &str, id: &str) -> echo_session::SessionEvent {
        echo_session::SessionEvent::ToolCall(echo_session::event::ToolCallEvent {
            id: id.into(),
            name: "bash".into(),
            arguments: "{}".into(),
            session: Some(session.into()),
            started_at_ms: None,
        })
    }

    #[tokio::test]
    async fn compact_preview_groups_by_session_and_counts() {
        let store = TrunkStore::new(100_000);
        // 会话 A：5 条（将被压前 3 条），会话 B：1 条（不动）。
        let a = store.ensure_workspace_channel("proj", "Proj");
        let a_id = a.id.clone();
        let b_id = "local:tui::local_user".to_string();
        for i in 0..5 {
            store.append_event(ev_user(&a_id, &format!("a{i}")));
            if i < 3 {
                store.append_event(ev_tool(&a_id, &format!("c{i}")));
            }
        }
        store.append_event(ev_user(&b_id, "b0"));

        // keep=3 → A 组 8 条事件压 5 留 3；B 组只有 1 条不动。
        let preview = store.compact_preview(3).expect("preview");
        assert_eq!(preview.groups.len(), 1, "only session A compacts");
        assert_eq!(preview.total_events, 9);
        let g = &preview.groups[0];
        assert_eq!(g.session.as_deref(), Some(a_id.as_str()));
        assert_eq!(g.replaced_count, 5);
        assert_eq!(g.users, 3, "a0,a1,a2 are the first three user messages");
        assert_eq!(g.tools, 2);

        // keep=10 → 两组都不足（A=8, B=1 ≤ 11）→ 无需压缩。
        assert!(store.compact_preview(10).is_err());
    }

    #[tokio::test]
    async fn compact_preview_rejects_short_log() {
        let store = TrunkStore::new(1000);
        store.append_event(ev_user("local:tui::local_user", "唯一一条"));
        let err = store.compact_preview(40).unwrap_err();
        assert!(err.contains("历史不足"), "got: {err}");
    }

    #[tokio::test]
    async fn apply_compaction_installs_body_summary_and_archive_path() {
        let path = temp_sessions_path("apply-compact");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(100_000);
        store.set_persist_path(&path);
        let sid = "local:tui::local_user";
        for i in 0..6 {
            store.append_event(ev_user(sid, &format!("m{i}")));
        }
        store.save_now().await;

        let preview = store.compact_preview(2).expect("preview");
        assert_eq!(preview.total_replaced, 4);
        let summaries = vec![CompactionSummary {
            session: Some(sid.into()),
            summary: "用户先问了 m0、m1，随后推进到 m2、m3。".into(),
            replaced_count: preview.total_replaced,
        }];
        let applied = store
            .apply_compaction(2, &summaries, Some("/tmp/archives/x-precompact.json"))
            .await
            .expect("apply");
        assert_eq!(applied.total_replaced, 4);
        assert_eq!(applied.groups, 1);

        // 事件日志：Compaction（正文无前缀 + 归档路径）+ 尾部 2 条。
        let events = store.event_log();
        assert_eq!(events.len(), 3, "1 compaction + 2 kept");
        match &events[0] {
            echo_session::SessionEvent::Compaction(c) => {
                assert_eq!(c.replaced_count, 4);
                assert!(
                    !c.summary.contains("[历史摘要]"),
                    "body must not embed the marker"
                );
                assert_eq!(
                    c.archive.as_deref(),
                    Some("/tmp/archives/x-precompact.json")
                );
                assert_eq!(c.session.as_deref(), Some(sid));
            }
            other => panic!("expected compaction, got {other:?}"),
        }

        // 投影：唯一前缀 + 保留的尾部。
        let history = store.snapshot_for(sid).await;
        assert_eq!(history.len(), 3);
        assert!(
            history[0].content.starts_with("[历史摘要] "),
            "got: {}",
            history[0].content
        );
        assert!(!history[0].content.contains("[历史摘要] [历史摘要]"));
        assert!(history[1].content.contains("m4"));
        assert!(history[2].content.contains("m5"));

        // 立即持久化：磁盘文件里能找到归档路径。
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(disk.contains("x-precompact.json"));
        assert!(disk.contains("m0") || disk.contains("replaced_count"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn apply_compaction_keeps_groups_without_a_summary() {
        let store = TrunkStore::new(100_000);
        let a_id = store.ensure_workspace_channel("proj", "Proj").id.clone();
        let b_id = "local:tui::local_user".to_string();
        for i in 0..6 {
            store.append_event(ev_user(&a_id, &format!("a{i}")));
        }
        for i in 0..6 {
            store.append_event(ev_user(&b_id, &format!("b{i}")));
        }
        let preview = store.compact_preview(2).expect("preview");
        assert_eq!(preview.groups.len(), 2);

        // 只为 A 组准备摘要：B 组必须原样保留（不能换出无摘要的 Compaction）。
        let summaries = vec![CompactionSummary {
            session: Some(a_id.clone()),
            summary: "A 组摘要".into(),
            replaced_count: 4,
        }];
        let applied = store
            .apply_compaction(2, &summaries, None)
            .await
            .expect("apply");
        assert_eq!(applied.groups, 1, "only the summarized group compacts");
        assert_eq!(applied.total_replaced, 4);

        let events = store.event_log();
        let compactions = events
            .iter()
            .filter(|e| matches!(e, echo_session::SessionEvent::Compaction(_)))
            .count();
        assert_eq!(compactions, 1);
        // B 组全部 6 条仍在。
        let b_left = events
            .iter()
            .filter(|e| e.session() == Some(b_id.as_str()))
            .count();
        assert_eq!(b_left, 6);
    }

    #[tokio::test]
    async fn archive_snapshot_writes_memory_state_with_reason() {
        let path = temp_sessions_path("archive-snapshot");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(1000);
        store.set_persist_path(&path);
        store.append_event(ev_user("local:tui::local_user", "未落盘的最新消息"));
        // 故意不 save_now()：快照必须来自内存，而不是落后的磁盘文件。

        let archive = store.archive_snapshot("precompact").await.expect("archive");
        assert!(archive.contains("precompact"), "reason in name: {archive}");
        let data = std::fs::read_to_string(&archive).unwrap();
        assert!(
            data.contains("未落盘的最新消息"),
            "snapshot must serialize the in-memory state"
        );
        assert!(data.contains("\"events\""));
        let _ = std::fs::remove_file(&archive);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn archive_history_copies_file_clears_and_marks_timeline() {
        let path = temp_sessions_path("archive-history");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(1000);
        store.set_persist_path(&path);
        store.append_event(ev_user("local:tui::local_user", "归档前的消息"));
        store.save_now().await;
        assert!(path.exists());

        let archive = store.archive_history().await.expect("archive path");
        // 归档文件可读且含原消息
        let data = std::fs::read_to_string(&archive).unwrap();
        assert!(data.contains("归档前的消息"));
        assert!(std::path::Path::new(&archive).starts_with(path.parent().unwrap().join("archives")));
        // 归档返回后（clear + 重新 push 之后）内存 timeline 即带系统提示行
        {
            let timeline = store.timeline.lock().await;
            assert!(
                timeline.iter().any(|m| m.content.contains("历史已归档")),
                "timeline carries the archive notice, got: {:?}",
                timeline.iter().map(|m| &m.content).collect::<Vec<_>>()
            );
        }
        // 历史已清空：重载为 0 条事件，但提示行随 clear 后的落盘持久化——
        // 磁盘上的 timeline 保留归档提示（用户下次打开能看到"历史已归档"）。
        let restored = store.load_from_file().await;
        assert_eq!(restored, 0, "history cleared after archive");
        {
            let timeline = store.timeline.lock().await;
            assert!(
                timeline.iter().any(|m| m.content.contains("历史已归档")),
                "archive notice persists across reload"
            );
        }
        let _ = std::fs::remove_file(&archive);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn archive_history_without_persist_path_errors() {
        let store = TrunkStore::new(1000);
        store.append_event(ev_user("local:tui::local_user", "内存消息"));
        let err = store.archive_history().await.expect_err("no persist path");
        assert!(err.contains("没有可归档"), "{err}");
    }

    #[tokio::test]
    async fn archive_history_without_existing_file_errors() {
        let path = temp_sessions_path("archive-missing");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(1000);
        store.set_persist_path(&path); // 路径已配但文件尚未写过
        let err = store.archive_history().await.expect_err("no file yet");
        assert!(err.contains("没有可归档"), "{err}");
    }

    #[test]
    fn prune_archives_keeps_newest_and_ignores_foreign_stems() {
        let dir = std::env::temp_dir().join(format!("echo-prune-{}", std::process::id()));
        let archives = dir.join("archives");
        std::fs::create_dir_all(&archives).unwrap();
        // 6 份本 stem 归档（mtime 递增）+ 1 份别的 stem + 1 个非 json
        for i in 0..6 {
            let f = archives.join(format!("echo-sessions-x-2026100{i}-000000-precompact.json"));
            std::fs::write(&f, "{}").unwrap();
            let mtime = filetime::FileTime::from_unix_time(1_700_000_000 + i as i64, 0);
            filetime::set_file_mtime(&f, mtime).unwrap();
        }
        std::fs::write(archives.join("other-stem-20261001-000000.json"), "{}").unwrap();
        std::fs::write(archives.join("echo-sessions-x-note.txt"), "x").unwrap();

        TrunkStore::prune_archives(&archives, "echo-sessions-x", 2);

        let left: Vec<String> = std::fs::read_dir(&archives)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        // 本 stem 只剩最新 2 份
        let own: Vec<&String> = left
            .iter()
            .filter(|n| n.starts_with("echo-sessions-x-") && n.ends_with(".json"))
            .collect();
        assert_eq!(own.len(), 2, "left: {left:?}");
        assert!(left.contains(&"echo-sessions-x-20261004-000000-precompact.json".to_string()));
        assert!(left.contains(&"echo-sessions-x-20261005-000000-precompact.json".to_string()));
        // 别的 stem 与非 json 不受影响
        assert!(left.contains(&"other-stem-20261001-000000.json".to_string()));
        assert!(left.contains(&"echo-sessions-x-note.txt".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prune_archives_does_not_swallow_longer_stems() {
        // 2026-10 审计回归：前缀匹配锚定时间戳段——stem `echo-sessions-x`
        // 不得统计/删除 `echo-sessions-x-y` 的归档（旧 starts_with 会命中）。
        let dir = std::env::temp_dir().join(format!("echo-prune-anchor-{}", std::process::id()));
        let archives = dir.join("archives");
        std::fs::create_dir_all(&archives).unwrap();
        let long_stem = archives.join("echo-sessions-x-y-20261001-000000.json");
        std::fs::write(&long_stem, "{}").unwrap();
        filetime::set_file_mtime(
            &long_stem,
            filetime::FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();
        for (i, ts) in ["20261002-000000", "20261003-000000"].iter().enumerate() {
            let f = archives.join(format!("echo-sessions-x-{ts}.json"));
            std::fs::write(&f, "{}").unwrap();
            filetime::set_file_mtime(
                &f,
                filetime::FileTime::from_unix_time(1_700_000_100 + i as i64, 0),
            )
            .unwrap();
        }

        TrunkStore::prune_archives(&archives, "echo-sessions-x", 1);

        let left: Vec<String> = std::fs::read_dir(&archives)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        // 本 stem 只保留最新 1 份；更长 stem 的文件不受影响。
        assert!(
            left.contains(&"echo-sessions-x-20261003-000000.json".to_string()),
            "newest own kept: {left:?}"
        );
        assert!(
            !left.contains(&"echo-sessions-x-20261002-000000.json".to_string()),
            "older own pruned: {left:?}"
        );
        assert!(
            left.contains(&"echo-sessions-x-y-20261001-000000.json".to_string()),
            "longer stem untouched: {left:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 2026-10 上下文审计 🔴 回归：token 预算热更——收窄立即重投影裁剪
    /// 生效；getter 反映新值（此前预算构造时固化，保存后要重启才生效）。
    #[tokio::test]
    async fn set_memory_limit_tokens_reprojects_immediately() {
        let store = TrunkStore::new(10_000);
        let key = SessionKey::local_tui();
        let session = store.get_or_create(&key, "user".into(), None);
        for i in 0..10 {
            store.append_event(echo_session::SessionEvent::UserMessage(
                echo_session::event::UserMessage {
                    session: Some(session.id.clone()),
                    content: format!("消息{i}-{}", "长".repeat(200)),
                    timestamp: 1700000000 + i,
                    message_sequence: None,
                    source: None,
                    images: vec![],
                },
            ));
        }
        let before = session.history.lock().len();
        assert_eq!(before, 10, "宽预算下全部保留");

        // 收窄到约 1 条消息的预算：立即重投影生效。
        store.set_memory_limit_tokens(200);
        assert_eq!(store.memory_limit_tokens(), 200);
        let after = session.history.lock().len();
        assert!(after < before, "收窄后投影应被裁剪: {after} !< {before}");

        // 放宽回大预算：下一条事件后投影按新预算容纳（旧事件不可恢复）。
        store.set_memory_limit_tokens(10_000);
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some(session.id.clone()),
                content: "新消息".into(),
                timestamp: 1700000100,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
        let contents: Vec<String> = session
            .history
            .lock()
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert!(contents.iter().any(|c| c.contains("新消息")));
    }

    /// 2026-10 上下文审计回归：归档名带毫秒段——同秒两次归档不得同名
    /// 覆盖；新旧格式（含 -precompact）都被茎匹配识别。
    #[test]
    fn archive_name_with_millis_never_collides_within_a_second() {
        // 新格式（带毫秒）茎匹配
        for name in [
            "echo-sessions-x-20261013-074421-123.json",
            "echo-sessions-x-20261013-074421-123-precompact.json",
            // 旧格式（无毫秒）向后兼容
            "echo-sessions-x-20261013-074421.json",
            "echo-sessions-x-20261013-074421-precompact.json",
        ] {
            assert!(
                TrunkStore::is_own_archive_name(name, "echo-sessions-x"),
                "recognized: {name}"
            );
        }
        // 非本 stem / 非归档名不误识
        for name in [
            "echo-sessions-x-y-20261013-074421-123.json",
            "echo-sessions-x-note.txt",
            "echo-sessions-x-2026101X-074421-123.json",
            "echo-sessions-x-20261013-07442112.json", // 时间戳连写无分隔
        ] {
            assert!(
                !TrunkStore::is_own_archive_name(name, "echo-sessions-x"),
                "rejected: {name}"
            );
        }
    }

    /// 2026-10 上下文审计回归：淘汰按文件名内嵌时间戳排序——mtime 失真
    /// （文件被复制/恢复）不影响"保留最新"。
    #[test]
    fn prune_archives_orders_by_filename_timestamp_not_mtime() {
        let dir = std::env::temp_dir().join(format!("echo-prune-name-{}", std::process::id()));
        let archives = dir.join("archives");
        std::fs::create_dir_all(&archives).unwrap();
        // 文件名时间戳递增，但 mtime 故意**倒置**（最新文件 mtime 最旧）。
        for i in 0..4 {
            let f = archives.join(format!("echo-sessions-x-2026101{i}-000000-001.json"));
            std::fs::write(&f, "{}").unwrap();
            let mtime = filetime::FileTime::from_unix_time(1_700_000_000 - i as i64 * 100, 0);
            filetime::set_file_mtime(&f, mtime).unwrap();
        }

        TrunkStore::prune_archives(&archives, "echo-sessions-x", 2);

        let left: Vec<String> = std::fs::read_dir(&archives)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            left.contains(&"echo-sessions-x-20261012-000000-001.json".to_string())
                && left.contains(&"echo-sessions-x-20261013-000000-001.json".to_string()),
            "filename-newest kept despite inverted mtimes: {left:?}"
        );
        assert_eq!(left.len(), 2, "left: {left:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn apply_compaction_keeps_events_arriving_after_preview() {
        let path = temp_sessions_path("compact-drift");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(100_000);
        store.set_persist_path(&path);
        let sid = "local:tui::local_user";
        // 预览口径：10 条事件，keep_recent=3 → 压缩前 7 条
        for i in 0..10 {
            store.append_event(ev_user(sid, &format!("消息 {i}")));
        }
        let preview = store.compact_preview(3).expect("preview");
        assert_eq!(preview.total_replaced, 7);
        // 摘要生成窗口内新到 2 条（漂移）——旧实现会把它们挤出保留尾
        store.append_event(ev_user(sid, "窗口期新消息 A"));
        store.append_event(ev_user(sid, "窗口期新消息 B"));

        let summaries = vec![crate::session::CompactionSummary {
            session: Some(sid.to_string()),
            summary: "[历史摘要] 前 7 条的摘要".into(),
            replaced_count: preview.total_replaced,
        }];
        let applied = store
            .apply_compaction(3, &summaries, None)
            .await
            .expect("apply");

        let events = store.event_log.log();
        // 修复后的切片语义：替换 = **预览切片**（前 7 条），窗口期新到的
        // A/B 与旧保留尾（消息 7/8/9）全部留在保留尾——不再有"按应用时点
        // len-keep 重算"把旧尾最老 a 条挤进无摘要前缀的静默丢档。
        let compaction = events
            .iter()
            .find_map(|e| match e {
                echo_session::SessionEvent::Compaction(c) => Some(c),
                _ => None,
            })
            .expect("compaction event");
        assert_eq!(
            compaction.replaced_count, 7,
            "replaced slice must be the preview count, not runtime drift"
        );
        let texts: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                echo_session::SessionEvent::UserMessage(m) => Some(m.content.clone()),
                _ => None,
            })
            .collect();
        for expected in [
            "消息 7",
            "消息 8",
            "消息 9",
            "窗口期新消息 A",
            "窗口期新消息 B",
        ] {
            assert!(
                texts.iter().any(|t| t.contains(expected)),
                "{expected} must survive: {texts:?}"
            );
        }
        assert_eq!(texts.len(), 5, "5 kept user messages: {texts:?}");
        assert_eq!(applied.total_replaced, 7);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn archive_history_uses_memory_snapshot_not_stale_disk_file() {
        // 2026-10 审计回归：归档源必须是**内存快照**——磁盘最多落后
        // 30s 保存周期，窗口内新到的事件此前"未归档即被清"（归档与
        // 日志双失，仅剩内存里的残影）。
        let path = temp_sessions_path("archive-memory");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(100_000);
        store.set_persist_path(&path);
        store.append_event(ev_user("local:tui::local_user", "已落盘的消息"));
        store.save_now().await;
        // 保存之后又到一条（窗口期事件，磁盘上没有）。
        store.append_event(ev_user("local:tui::local_user", "未落盘的窗口期消息"));

        let archive = store.archive_history().await.expect("archive");
        let data = std::fs::read_to_string(&archive).unwrap();
        assert!(
            data.contains("未落盘的窗口期消息"),
            "archive must contain the in-memory event disk has not saved yet"
        );
        assert!(data.contains("已落盘的消息"));
        let _ = std::fs::remove_file(&archive);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn archive_session_clears_only_the_target_session() {
        // 2026-10 审计回归：会话级归档——A 被归档并清空，B 原样保留；
        // 归档文件是整档快照（恢复时可直接作为会话文件手工换回）。
        let path = temp_sessions_path("archive-scoped");
        let _ = std::fs::remove_file(&path);
        let store = TrunkStore::new(100_000);
        store.set_persist_path(&path);
        let a_id = store.ensure_workspace_channel("proj", "Proj").id.clone();
        let b_id = "local:tui::local_user".to_string();
        for i in 0..3 {
            store.append_event(ev_user(&a_id, &format!("A{i}")));
        }
        for i in 0..3 {
            store.append_event(ev_user(&b_id, &format!("B{i}")));
        }

        let archive = store.archive_session(&a_id).await.expect("archive");
        let data = std::fs::read_to_string(&archive).unwrap();
        assert!(
            data.contains("A0") && data.contains("B0"),
            "snapshot is the whole-file archive"
        );

        // 只剩 B 的 3 条事件；A 被清空。
        let events = store.event_log();
        let a_left = events
            .iter()
            .filter(|e| e.session() == Some(a_id.as_str()))
            .count();
        let b_left = events
            .iter()
            .filter(|e| e.session() == Some(b_id.as_str()))
            .count();
        assert_eq!(a_left, 0, "target session cleared");
        assert_eq!(b_left, 3, "other sessions untouched");
        // timeline 里 A 的条目被清、B 保留；提示行走统一入列路径（带 seq）。
        {
            let timeline = store.timeline.lock().await;
            assert!(
                timeline.iter().all(|m| m.session_id != a_id),
                "timeline for the archived session is cleared"
            );
            assert!(timeline
                .iter()
                .any(|m| m.content.contains("会话历史已归档")));
            assert!(
                timeline.last().is_some_and(|m| m.seq > 0),
                "archive notice went through the counted push path (seq assigned)"
            );
        }
        let _ = std::fs::remove_file(&archive);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn compact_history_is_serialized_by_compact_lock() {
        use std::sync::Arc;
        let store = Arc::new(TrunkStore::new(1000));
        let guard1 = store.compact_lock().await;
        // 第二把锁必须等——try_lock 立即失败证明互斥
        assert!(
            store.compact_lock.try_lock().is_err(),
            "compact_lock must be exclusive while held"
        );
        drop(guard1);
        let _guard2 = store.compact_lock().await;
    }

    /// 真实会话文件离线体检（人工触发，不进 CI）：
    ///
    /// ```text
    /// ECHO_COMPACTION_LIVE_FILE=~/.config/echo-agent-core/echo-sessions-EchoCode.json \
    ///   cargo test -p echo-agent --lib live_compaction_on_real_session_file -- --ignored
    /// ```
    ///
    /// 在**文件副本**上走完整链路（载入 → 预览 → 归档 → 应用），验证真实
    /// 数据的分组/统计与归档产物；绝不触碰原文件。
    #[tokio::test]
    #[ignore = "manual: needs a real session file via ECHO_COMPACTION_LIVE_FILE"]
    async fn live_compaction_on_real_session_file() {
        let source = std::env::var("ECHO_COMPACTION_LIVE_FILE")
            .expect("set ECHO_COMPACTION_LIVE_FILE to a sessions JSON file");
        let dir = std::env::temp_dir().join(format!("echo-live-compact-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let copy = dir.join("sessions.json");
        std::fs::copy(&source, &copy).expect("copy session file");

        let store = TrunkStore::new(800_000);
        store.set_persist_path(&copy);
        let restored = store.load_from_file().await;
        assert!(restored > 0, "sessions restored");
        let preview = store.compact_preview(40).expect("preview");
        eprintln!(
            "预览：共 {} 条事件 → 压缩 {} 条，{} 组",
            preview.total_events,
            preview.total_replaced,
            preview.groups.len()
        );
        for group in &preview.groups {
            eprintln!(
                "  - {}: {} 条（{} 用户 / {} 工具调用）",
                group.session.as_deref().unwrap_or("<legacy>"),
                group.replaced_count,
                group.users,
                group.tools
            );
        }
        let archive = store
            .archive_snapshot("precompact-live")
            .await
            .expect("archive");
        eprintln!("归档：{archive}");
        assert!(std::path::Path::new(&archive).exists());

        // 以规则回退文案落地（离线无 LLM；真实链路里这里是模型摘要）。
        let summaries: Vec<CompactionSummary> = preview
            .groups
            .iter()
            .map(|g| CompactionSummary {
                session: g.session.clone(),
                summary: format!("[offline-体检] {} 条事件", g.replaced_count),
                replaced_count: g.replaced_count,
            })
            .collect();
        let applied = store
            .apply_compaction(40, &summaries, Some(&archive))
            .await
            .expect("apply");
        eprintln!(
            "已应用：{} 条 → {} 组（保留 {}）",
            applied.total_replaced, applied.groups, applied.kept
        );
        // 每个受影响会话的投影第一条应是唯一前缀的摘要。
        for group in &preview.groups {
            let Some(sid) = &group.session else { continue };
            let history = store.snapshot_for(sid).await;
            assert!(
                history[0].content.starts_with("[历史摘要] "),
                "session {sid} projection should lead with the summary"
            );
            assert!(!history[0].content.contains("[历史摘要] [历史摘要]"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn archive_snapshot_without_persistence_errors() {
        let store = TrunkStore::new(1000);
        let err = store.archive_snapshot("precompact").await.unwrap_err();
        assert!(err.contains("未启用持久化"), "got: {err}");
    }
}
