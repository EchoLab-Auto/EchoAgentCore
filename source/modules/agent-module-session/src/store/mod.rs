//! Multi-session conversation contexts with per-source identities.
//!
//! 每个来源（本地 TUI / QQ 私聊 / QQ 群 / …）是一个 `Session`，拥有**独立
//! 的模型上下文**（按事件归属从同一份 append-only 事件日志投影而来，
//! 2026-09 多会话改造）。消息到达时从该会话上下文的时点快照 fork 一条临时
//! 分支；分支结束后按序合并回事件日志（见
//! [`Agent::process_message`]）。
//!
//! 展示时间线（timeline）仍是全部会话的合并视图（条目自带 session 归属），
//! 由前端按当前选中会话过滤。

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;

use dashmap::DashMap;

pub mod archive;
pub mod archives;
pub mod history;
pub mod persist;
pub mod query;
pub mod timeline;

// 显示历史的合并水位线：agent 命令层要用（`RequestSessionHistory`）。
// 只导出这一个函数——其余派生实现细节留在模块内。
pub use history::{
    history_watermark, ArchiveEventSource, ExpandContext, ExtendedHistory,
    EXPAND_DEPTH_PER_REQUEST, EXPAND_HARD_CAP,
};

use agent_module_management::protocol::SessionInfo;
use echo_defs::llm::LlmProvider;
use echo_defs::message::ChatMessage;

/// 结构化会话标识（口径单源 = `echo_session::SessionKey`，解耦计划
/// Phase 3 ⑤：词汇下沉，使工作区模块与其它模块能直接构造会话 id）。
pub use echo_session::SessionKey;

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
    /// 会话级 LLM provider 覆盖（2026-10）：`Some(profile_name)` → 该
    /// 会话的 LLM 请求走专用 provider（按该 profile 重建）；`None` =
    /// 跟随 persona 级 provider（`Agent.provider`）。DashMap 值是
    /// `Arc<Session>` 克隆——`session_providers` 挂在 TrunkStore 上
    /// 以保证克隆间共享（与 `history`/`turn_lock` 同一生命周期）。
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

    /// 线格式快照（`SessionInfo`）。
    ///
    /// `identity`：本节点身份（NodeId + 运行区域名）。由调用方（Agent）
    /// 传入——Session 本身不持有身份，避免每会话复制一份；缺省空身份 =
    /// 字段不下发（嵌入式/测试用法）。P3 终态：身份是参数，不再从
    /// 进程级 kernel cell 读取。
    pub fn info(
        &self,
        last_message: String,
        identity: &echo_defs::node::NodeIdentity,
    ) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            node_id: identity.node_id_field(),
            region_name: identity.region_name_field(),
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
/// 会话数据加载到哪一步了（2026-11 懒加载）。
///
/// 为什么分两档：面板**启动就要**知道有哪些会话（列表、计数、"最后一条
/// 消息"），但那份 20 MB 事件日志只有真正要聊天/要历史时才用得上。
/// 元信息档用 [`serde::de::IgnoredAny`] 跳过 `events` / `timeline`——解析器
/// 只做词法推进、不建树，20 MB 文件的峰值内存从"整棵 JSON 树"降到常数级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LoadStage {
    /// 什么都没读（磁盘上可能躺着一份完整历史）。
    Untouched = 0,
    /// 只读了身份与头部：会话列表/计数可用，事件日志与显示时间线还没读。
    Metadata = 1,
    /// 全量：事件日志 + 显示时间线 + 各会话投影都在内存里。
    Full = 2,
    /// **读盘 / 解析失败**（2026-11 A）：内存里不是磁盘那份文档。
    ///
    /// 这是一个**负缓存**：`ensure_loaded` / `load_from_file` 见 Failed
    /// 直接返回，不再每轮自动重试。重试只能走显式出口
    /// [`TrunkStore::retry_failed_load`] / [`TrunkStore::recover_failed_load`]。
    /// 失败态**禁止落盘**（否则空文档会把磁盘上的历史抹掉），原因见
    /// [`TrunkStore::load_error`]。没有自动改名/备份：原文件原样留着。
    Failed = 3,
    /// **正在解析 / 提交**（2026-11 第二轮）：并发的加载在 `load_gate` 上
    /// 等待；`append_event` / `push_timeline` 持 `commit_barrier` 读锁，
    /// 提交持写锁——保证"确认空 store → 安装历史"之间没有新写入插进来。
    Loading = 4,
}

impl LoadStage {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Untouched,
            1 => Self::Metadata,
            2 => Self::Full,
            3 => Self::Failed,
            4 => Self::Loading,
            // 未知值（未来版本/内存损坏）按失败处理：宁可拒绝运行，也不
            // 假装已加载后覆盖文件。
            _ => Self::Failed,
        }
    }
}

/// 会话文件加载失败的原因（可观察，见 [`TrunkStore::load_error`]）。
///
/// 共同点是**都不许覆盖原文件**；分开是为了诊断与恢复策略：
/// 读失败常常是环境问题，格式/版本问题要人工看一眼。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistLoadError {
    /// 读文件失败（权限 / IO / 非 UTF-8）。文件（或目录）仍在原地。
    Read {
        path: std::path::PathBuf,
        error: String,
    },
    /// 内容不是合法会话文档（非法 JSON / 认不出的形状 / 当前版坏事件）。
    Invalid {
        path: std::path::PathBuf,
        error: String,
    },
    /// 文件属于本进程不认识的**未来版本**（`version > PERSIST_VERSION`）。
    UnsupportedVersion {
        path: std::path::PathBuf,
        version: u64,
        supported: u32,
    },
    /// 当前版本文档里的坏条目（未知事件 kind / 字段类型错误）。
    Corrupt {
        path: std::path::PathBuf,
        section: &'static str,
        index: usize,
        error: String,
    },
    /// 目标 store 已有事件/时间线：拒绝在非空 store 上重载/合并。
    NotEmpty {
        path: std::path::PathBuf,
        events: usize,
        timeline: usize,
    },
    /// 加载期间发生了 append / push（隔离解析无法安全合并）。
    ConcurrentWrite {
        path: std::path::PathBuf,
        events: usize,
        timeline: usize,
    },
    /// 读到的文件在解析/提交前被其他写者替换。
    ChangedDuringLoad { path: std::path::PathBuf },
}

impl PersistLoadError {
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::Read { path, .. }
            | Self::Invalid { path, .. }
            | Self::UnsupportedVersion { path, .. }
            | Self::Corrupt { path, .. }
            | Self::NotEmpty { path, .. }
            | Self::ConcurrentWrite { path, .. }
            | Self::ChangedDuringLoad { path } => path,
        }
    }

    /// 分类名（日志/诊断用的稳定字符串）。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Invalid { .. } => "invalid",
            Self::UnsupportedVersion { .. } => "unsupported_version",
            Self::Corrupt { .. } => "corrupt",
            Self::NotEmpty { .. } => "not_empty",
            Self::ConcurrentWrite { .. } => "concurrent_write",
            Self::ChangedDuringLoad { .. } => "changed_during_load",
        }
    }

    fn key(&self) -> (std::path::PathBuf, String) {
        (self.path().to_path_buf(), self.to_string())
    }
}

impl std::fmt::Display for PersistLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, error } => {
                write!(f, "读取 {} 失败：{error}", path.display())
            }
            Self::Invalid { path, error } => {
                write!(f, "{} 不是合法会话文档：{error}", path.display())
            }
            Self::UnsupportedVersion {
                path,
                version,
                supported,
            } => write!(
                f,
                "{} 是未来版本 v{version}（本进程最高支持 v{supported}），拒绝加载",
                path.display()
            ),
            Self::Corrupt {
                path,
                section,
                index,
                error,
            } => write!(
                f,
                "{} 的 {section}[{index}] 不是合法条目：{error}",
                path.display()
            ),
            Self::NotEmpty {
                path,
                events,
                timeline,
            } => write!(
                f,
                "{} 加载被拒：store 已有 events={events}, timeline={timeline}；拒绝重载/合并",
                path.display()
            ),
            Self::ConcurrentWrite {
                path,
                events,
                timeline,
            } => write!(
                f,
                "{} 加载被拒：解析期间 store 新增 events={events}, timeline={timeline}；拒绝错序合并",
                path.display()
            ),
            Self::ChangedDuringLoad { path } => {
                write!(f, "{} 在加载期间被其他写者替换，拒绝提交陈旧快照", path.display())
            }
        }
    }
}

impl std::error::Error for PersistLoadError {}

/// 落盘被拒绝的原因（可观察，见 [`TrunkStore::last_save_error`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistSaveError {
    /// 磁盘文件已经不是本 store 加载/上次写出的那一份：陈旧快照明确拒绝。
    StaleSnapshot {
        path: std::path::PathBuf,
        expected: String,
        actual: String,
    },
    /// 写 tmp / rename / 取指纹失败。原文件保持不动。
    Io {
        path: std::path::PathBuf,
        error: String,
    },
    /// 有持久化路径，但会话历史加载失败：落盘被拒（原文件原样保留）。
    /// [`TrunkStore::save_now_checked`] 用它把"加载失败"显式交还退休收尾。
    LoadFailed {
        path: std::path::PathBuf,
        error: String,
    },
    /// 磁盘上已有会话档，但本 store **还没全量加载**，且仍有未落盘改动：
    /// 用空/不全的快照覆盖等于抹掉历史，明确拒绝。
    NotLoaded { path: std::path::PathBuf },
}

impl PersistSaveError {
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::StaleSnapshot { path, .. }
            | Self::Io { path, .. }
            | Self::LoadFailed { path, .. }
            | Self::NotLoaded { path } => path,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::StaleSnapshot { .. } => "stale_snapshot",
            Self::Io { .. } => "io",
            Self::LoadFailed { .. } => "load_failed",
            Self::NotLoaded { .. } => "not_loaded",
        }
    }
}

impl std::fmt::Display for PersistSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleSnapshot {
                path,
                expected,
                actual,
            } => write!(
                f,
                "{} 落盘被拒：磁盘文件已变化（expected={expected}, actual={actual}）",
                path.display()
            ),
            Self::Io { path, error } => write!(f, "{} 落盘失败：{error}", path.display()),
            Self::LoadFailed { path, error } => write!(
                f,
                "{} 落盘失败：会话历史加载失败（{error}），原文件保持不动",
                path.display()
            ),
            Self::NotLoaded { path } => write!(
                f,
                "{} 落盘被拒：磁盘已有会话档但尚未加载，拒绝用不全的快照覆盖",
                path.display()
            ),
        }
    }
}

impl std::error::Error for PersistSaveError {}

/// 文件指纹（乐观并发基线）：长度 + mtime + Unix inode。
///
/// 选 inode 是刻意的：本模块的提交是 tmp+rename，每次成功落盘都会换一个
/// inode；跨独立 `TrunkStore` 的陈旧快照因此能稳定识别，而不必每次保存
/// 都把整份 20 MB 文件读回来做内容哈希。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileStamp {
    len: u64,
    mtime: Option<std::time::SystemTime>,
    #[cfg(unix)]
    ino: u64,
}

impl FileStamp {
    pub(crate) fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            mtime: metadata.modified().ok(),
            #[cfg(unix)]
            ino: {
                use std::os::unix::fs::MetadataExt;
                metadata.ino()
            },
        }
    }

    /// 读取路径指纹；`Ok(None)` = 文件不存在。
    pub(crate) fn capture(path: &std::path::Path) -> std::io::Result<Option<Self>> {
        match std::fs::metadata(path) {
            Ok(metadata) => Ok(Some(Self::from_metadata(&metadata))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn describe(&self) -> String {
        let mut text = format!("len={}", self.len);
        #[cfg(unix)]
        {
            text.push_str(&format!(" ino={}", self.ino));
        }
        if let Some(mtime) = self.mtime {
            if let Ok(duration) = mtime.duration_since(std::time::UNIX_EPOCH) {
                text.push_str(&format!(
                    " mtime={}.{:09}",
                    duration.as_secs(),
                    duration.subsec_nanos()
                ));
            }
        }
        text
    }
}

/// 本 store 认为磁盘文件"应该是什么样"的基线。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PersistOrigin {
    /// 还没加载/没写过，且文件不存在：允许第一次落盘（新人格）。
    #[default]
    Unbaselined,
    /// 确认文件不存在（加载时 NotFound / 显式恢复移走后）。
    Missing,
    /// 加载或上次成功落盘时看到的文件身份。
    At(FileStamp),
}

impl PersistOrigin {
    pub(crate) fn matches(&self, current: Option<FileStamp>) -> bool {
        match (self, current) {
            // 未知基线只允许"文件仍不存在"的第一次写入；文件一旦存在就
            // 不能覆盖（否则等于绕过"没读过就别盖掉"的门禁）。
            (Self::Unbaselined | Self::Missing, None) => true,
            (Self::At(expected), Some(actual)) => *expected == actual,
            _ => false,
        }
    }

    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Unbaselined => "unbaselined".into(),
            Self::Missing => "absent".into(),
            Self::At(stamp) => stamp.describe(),
        }
    }
}

#[cfg(test)]
pub(crate) type ArchiveIoHook = Box<dyn FnOnce() + Send>;

/// **仅测试**的落盘插桩（`#[cfg(test)]` 字段，生产构建不存在）。
#[cfg(test)]
#[derive(Default)]
pub(crate) struct PersistTestHooks {
    /// 归档 IO 的一次性插桩；共享给同一 store 的克隆，无全局注册表。
    pub(crate) before_archive_io: Arc<std::sync::Mutex<Option<ArchiveIoHook>>>,

    /// 在"快照序列化完成 → 写 tmp/rename"之间执行一次（取走后即清空）。
    pub(crate) before_commit: std::sync::Mutex<Option<Box<dyn FnOnce() + Send + Sync>>>,
    /// 在"文件已解析成 ParsedDocument → 提交进 store"之间执行一次
    /// （取走后即清空）。用于确定性地模拟"加载期间 append/push/mark_dirty"。
    pub(crate) before_load_commit: std::sync::Mutex<Option<Box<dyn FnOnce() + Send + Sync>>>,
}

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
    /// 文件指纹基线（2026-11 第二轮，取代 `path_save_lock` 全局注册表）：
    /// 加载成功 / 上次成功落盘时磁盘文件的身份。写盘前后各比对一次，
    /// 跨独立 store 的陈旧快照 **明确拒绝**（`PersistSaveError::StaleSnapshot`），
    /// 而不是靠"同一路径排队"让旧快照后写覆盖新快照。
    persist_origin: Arc<std::sync::Mutex<PersistOrigin>>,
    /// 最近一次落盘被拒的原因（陈旧快照 / IO），可观察面。
    save_error: Arc<std::sync::Mutex<Option<PersistSaveError>>>,
    /// 加载提交屏障：`append_event` / `push_timeline` 持读锁，加载提交持
    /// 写锁。写锁只在同步的"确认空 store → 安装历史"临界区内持有，
    /// **不跨 await**；它保证隔离解析出来的历史不会与加载期间的新写入
    /// 交错成"新事件在前、旧事件在后"。
    commit_barrier: Arc<parking_lot::RwLock<()>>,
    /// 历史内容代次（2026-11 第二轮补充）：`clear_history` / `clear_session`
    /// / `apply_compaction` 在 `commit_barrier` 写锁内推进它。加载在**解析
    /// 之前**捕获代次，提交时在写锁内比对——期间发生的清空/压缩不会被
    /// "安装文件历史"覆盖。
    ///
    /// 为什么单靠 `commit_barrier` 不够：清空之后 store 恰好是**空的**，
    /// 加载提交的空 store 检查会通过，于是文件的旧历史会把刚清掉的内容
    /// 装回来。代次是这条边界的第二把锁（检查与提交同在写锁内）。
    history_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Display timeline (persisted, survives TUI restarts). Independent of the
    /// token-bounded LLM trunk: bounded by entry count, keeps richer metadata
    /// (source provenance, tool calls, reasoning) for the TUI history view.
    ///
    /// 同步锁（parking_lot）：所有临界区都是纯内存短操作（push / clone /
    /// retain），持锁期间绝不 `.await`、不做 IO / 序列化。此前为 tokio Mutex，
    /// 同步调用方只能 `try_lock` + `thread::sleep` 自旋（阻塞 runtime worker，
    /// 竞争时还会静默丢条目）——2026-10 架构评审修复。
    timeline: Arc<parking_lot::Mutex<Vec<agent_module_management::protocol::TimelineMessage>>>,
    /// Timeline 单调序号：每次新增/更新条目递增。前端用它做增量同步
    ///（RequestTrunkTimeline.since_seq + TrunkTimeline.seq），避免切换
    /// agent 时全量重传。
    timeline_seq: Arc<std::sync::atomic::AtomicU64>,
    /// 位置序号计数器（分页锚点）：与 `timeline_seq` 分开——后者会被就地更新
    /// 推进（增量同步需要），前者**只在 push 时递增**、此后永不改变。
    timeline_ord: Arc<std::sync::atomic::AtomicU64>,
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
    /// 会话级 provider 覆盖表（session_id → (profile_name, provider)）。
    /// 与会话生命周期解耦（会话删除时条目保留——重启后恢复会话时
    /// 可重新绑定；清理由 ClearHistory 触发）。
    /// `Arc` 共享使 `Session` 克隆间可见同一表（与 `history` 同理）。
    pub session_providers: Arc<DashMap<String, SessionProviderEntry>>,
    /// The append-only session event log — the **single source of truth** for
    /// the model-facing context. `trunk_history` is its in-memory projection
    /// cache (see `append_event`); persistence writes this log, and older
    /// formats migrate into it (dsh "model-visible means logged").
    event_log: echo_session::EventLog,
    /// Fork/resume metadata (lineage, origin, delegation depth). The global
    /// trunk model keeps one header for the store.
    ///
    /// `Arc`（2026-11）：克隆必须**共享**——`session_commands_host` 把
    /// `agent.trunk.clone()` 包成 `Arc<TrunkStore>` 再写盘，各持一份
    /// `Mutex` 会让"克隆写 header、本体读不到"。
    header: Arc<std::sync::Mutex<Option<echo_session::SessionHeader>>>,
    /// 会话文件路径（`Arc` 共享，理由同 `header`）。
    persist_path: Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
    /// 脏代次（2026-11 B1 修复）：每次 `mark_dirty` +1。
    ///
    /// 此前是 `AtomicBool`：写盘路径"快照 → await 写盘 → dirty=false"，
    /// await 期间 append 进来的数据被那一句 `store(false)` 一并清掉
    /// （lost update）——本轮写出去的是旧快照，而新数据再也不会有下一次
    /// 保存。改用代次后：写盘前记下代次，成功后只把 `persisted_gen`
    /// 推进到**那一个**代次；期间的新 dirty 会留下更高的代次，
    /// `is_dirty()` 仍然为真。
    dirty_gen: Arc<std::sync::atomic::AtomicU64>,
    /// 最近一次成功落盘覆盖到的脏代次。
    persisted_gen: Arc<std::sync::atomic::AtomicU64>,
    /// 最近一次加载失败的原因（`None` = 正常）。可观察性：诊断/面板读它。
    load_error: Arc<std::sync::Mutex<Option<PersistLoadError>>>,
    /// 加载阶段（2026-11 懒加载，见 [`LoadStage`]）。
    ///
    /// 启动期不再对每个人格全量读盘：一份 `echo-sessions-<team>.json` 实测
    /// 20+ MB、上万条事件，四个常驻人格加起来把整个事件日志钉在内存里
    /// （`Arc<Mutex<Vec<SessionEvent>>>`），进程 RSS 随人格数线性上涨。
    load_stage: Arc<std::sync::atomic::AtomicU8>,
    /// 加载串行闸：并发的首批请求只触发**一次**读盘。
    load_gate: Arc<tokio::sync::Mutex<()>>,
    /// **仅测试**：写盘两段之间的钩子（见 `append_between_snapshot_and_commit_keeps_dirty`）。
    /// 生产构建里不存在（`#[cfg(test)]`），零成本。
    #[cfg(test)]
    pub(crate) hooks: PersistTestHooks,
}

impl Clone for TrunkStore {
    fn clone(&self) -> Self {
        Self {
            team_id: Arc::clone(&self.team_id),
            identities: Arc::clone(&self.identities),
            history_epoch: Arc::clone(&self.history_epoch),
            memory_limit_tokens: AtomicUsize::new(self.memory_limit_tokens.load(Ordering::Relaxed)),
            session_histories: Arc::clone(&self.session_histories),
            trunk_turn_lock: Arc::clone(&self.trunk_turn_lock),
            compact_lock: Arc::clone(&self.compact_lock),
            save_lock: Arc::clone(&self.save_lock),
            persist_origin: Arc::clone(&self.persist_origin),
            save_error: Arc::clone(&self.save_error),
            commit_barrier: Arc::clone(&self.commit_barrier),
            timeline: Arc::clone(&self.timeline),
            timeline_seq: Arc::clone(&self.timeline_seq),
            timeline_ord: Arc::clone(&self.timeline_ord),
            timeline_pushed: Arc::clone(&self.timeline_pushed),
            timeline_drained: Arc::clone(&self.timeline_drained),
            timeline_removed: Arc::clone(&self.timeline_removed),
            timeline_rebaseline: Arc::clone(&self.timeline_rebaseline),
            timeline_probe_state: Arc::clone(&self.timeline_probe_state),
            session_providers: Arc::clone(&self.session_providers),
            event_log: self.event_log.clone(),
            // 克隆 = 同一份会话状态的**另一个句柄**，不是快照（2026-11 B2）：
            // header / persist_path / dirty 代次全部共享。此前这三样按值拷贝，
            // 于是"经克隆落盘"清不掉本体的 dirty、克隆设的路径本体看不见
            // （会话命令层每天都在这么用）。
            header: Arc::clone(&self.header),
            persist_path: Arc::clone(&self.persist_path),
            dirty_gen: Arc::clone(&self.dirty_gen),
            persisted_gen: Arc::clone(&self.persisted_gen),
            load_error: Arc::clone(&self.load_error),
            load_stage: Arc::clone(&self.load_stage),
            load_gate: Arc::clone(&self.load_gate),
            // 写盘/加载钩子不进克隆；归档 IO 钩子按 store 共享，取出后消费一次。
            #[cfg(test)]
            hooks: PersistTestHooks {
                before_archive_io: Arc::clone(&self.hooks.before_archive_io),
                ..PersistTestHooks::default()
            },
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

/// 压缩的四种词汇（计划 / 摘要 / 落地结果）**定义在 `echo-session`**
/// （全模块化 S3）：它们是"事件日志将被压缩"这件事的契约——日志自己算出
/// 预览、调用方填回摘要、落地结果回给调用方。压缩**策略**（怎么渲染
/// transcript、怎么分层裁剪、怎么让 LLM 出摘要）因此能独立于宿主。
pub use echo_session::{CompactGroup, CompactPreview, CompactionApplied, CompactionSummary};

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
/// 会话级 provider 覆盖条目：`(profile 名, provider)`。
pub type SessionProviderEntry = (String, Arc<dyn LlmProvider>);

fn timeline_sessions_summary(
    timeline: &[agent_module_management::protocol::TimelineMessage],
) -> String {
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
            history_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            trunk_turn_lock: Arc::new(tokio::sync::Mutex::new(())),
            compact_lock: Arc::new(tokio::sync::Mutex::new(())),
            save_lock: Arc::new(tokio::sync::Mutex::new(())),
            persist_origin: Arc::new(std::sync::Mutex::new(PersistOrigin::Unbaselined)),
            save_error: Arc::new(std::sync::Mutex::new(None)),
            commit_barrier: Arc::new(parking_lot::RwLock::new(())),
            timeline: Arc::new(parking_lot::Mutex::new(Vec::new())),
            timeline_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeline_ord: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeline_pushed: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeline_drained: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeline_removed: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeline_rebaseline: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            timeline_probe_state: Arc::new(std::sync::Mutex::new(None)),
            session_providers: Arc::new(DashMap::new()),
            event_log: echo_session::EventLog::new(),
            header: Arc::new(std::sync::Mutex::new(None)),
            persist_path: Arc::new(std::sync::Mutex::new(None)),
            // 代次从 (0, 0) 起步 = 干净。`mark_dirty` 只推进 `dirty_gen`。
            dirty_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            persisted_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            load_error: Arc::new(std::sync::Mutex::new(None)),
            // 纯内存 store（未设 persist_path）同样是"未加载"——`ensure_*`
            // 发现没有路径会直接落到 `Full`（没有东西可读）。
            load_stage: Arc::new(std::sync::atomic::AtomicU8::new(LoadStage::Untouched as u8)),
            load_gate: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(test)]
            hooks: PersistTestHooks::default(),
        }
    }

    /// Set the file path for session persistence.
    /// 当前持久化文件路径（`None` = 纯内存）。
    ///
    /// 归档目录从它派生（`parent/archives`），所以 `archives` /
    /// `archive` 两个子模块都要读它——加访问器而不是把字段提成 `pub(crate)`：
    /// 字段是 `Mutex`，直读会把锁语义散到各处。
    pub fn persist_path(&self) -> Option<std::path::PathBuf> {
        self.persist_path.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn set_persist_path(&self, path: impl Into<std::path::PathBuf>) {
        *self
            .persist_path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path.into());
        // 换了路径 = 换了并发基线；旧文件的指纹不能拿来判断新路径。
        self.set_persist_origin(PersistOrigin::Unbaselined);
        self.clear_save_error();
    }

    /// 当前文件指纹基线（加载 / 上次成功落盘时的磁盘身份）。
    pub(crate) fn persist_origin(&self) -> PersistOrigin {
        *self
            .persist_origin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn set_persist_origin(&self, origin: PersistOrigin) {
        *self
            .persist_origin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = origin;
    }

    /// 最近一次落盘被拒的原因（`None` = 正常 / 还没写过）。
    ///
    /// 陈旧快照与 IO 失败都会留在这里；dirty 不会因此被清掉。
    pub fn last_save_error(&self) -> Option<PersistSaveError> {
        self.save_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_save_error(&self, error: PersistSaveError) {
        let mut slot = self
            .save_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 周期保存会反复撞同一个陈旧快照：同一原因只 warn 一次，避免刷屏。
        if slot.as_ref() != Some(&error) {
            tracing::warn!(
                path = %error.path().display(),
                kind = error.kind(),
                error = %error,
                "session save rejected; keeping dirty state and the original file"
            );
        }
        *slot = Some(error);
    }

    pub(crate) fn clear_save_error(&self) {
        *self
            .save_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Mark sessions as needing save.
    ///
    /// 只推进代次（2026-11 B1）：写盘路径记下"快照时的代次"，成功后只把
    /// `persisted_gen` 推进到那一代——写盘期间的新标记不会被吞掉。
    pub fn mark_dirty(&self) {
        self.dirty_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// 压缩编排互斥锁（见字段注释）。`compact_history` 全程持有。
    pub async fn compact_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.compact_lock.lock().await
    }

    /// 当前脏代次（写盘前快照用）。
    pub(crate) fn dirty_generation(&self) -> u64 {
        self.dirty_gen.load(std::sync::atomic::Ordering::Acquire)
    }

    /// 当前历史内容代次（加载提交比对用，见字段注释）。
    pub(crate) fn history_epoch(&self) -> u64 {
        self.history_epoch
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// 推进历史内容代次。**调用方必须持有 `commit_barrier` 写锁**：代次
    /// 推进与"清空/压缩"必须在同一个对加载提交可见的临界区里，否则
    /// "先推进代次、后改内容"的窗口里加载提交会看到新代次却读到旧内容。
    pub(crate) fn bump_history_epoch(&self) -> u64 {
        self.history_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1
    }

    /// 记录"内容覆盖到 `generation` 的那份快照已经落盘"。
    ///
    /// 只进不退（`fetch_max`）：并发写者不会把进度改小；`generation` 之后
    /// 的新 `mark_dirty` 仍然让 [`is_dirty`] 为真。
    pub(crate) fn note_persisted(&self, generation: u64) {
        self.persisted_gen
            .fetch_max(generation, std::sync::atomic::Ordering::AcqRel);
    }

    /// 是否有未落盘的改动。
    pub fn is_dirty(&self) -> bool {
        self.dirty_generation()
            != self
                .persisted_gen
                .load(std::sync::atomic::Ordering::Acquire)
    }

    /// 最近一次加载失败的原因（`None` = 正常 / 还没加载过）。
    ///
    /// 失败时 [`Self::is_loaded`] 为假、落盘被拒绝（原文件原样保留）——
    /// 这个访问器是那条约束的**可观察面**（诊断 / 面板 / 测试）。
    pub fn load_error(&self) -> Option<PersistLoadError> {
        self.load_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_load_error(&self, error: PersistLoadError) {
        let mut slot = self
            .load_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 同一份档反复失败（周期重试 / 并发首批请求）只记一次，避免刷屏。
        if slot.as_ref().map(PersistLoadError::key) != Some(error.key()) {
            tracing::error!(
                path = %error.path().display(),
                kind = error.kind(),
                error = %error,
                "session file load failed; refusing to overwrite it"
            );
        }
        *slot = Some(error);
    }

    pub(crate) fn clear_load_error(&self) {
        *self
            .load_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// **仅测试**：跑一次"快照 → 提交"之间的钩子（生产构建为空操作）。
    /// **仅测试**：跑一次"解析完成 → 提交"之间的钩子（生产构建为空操作）。
    fn run_before_load_commit_hook(&self) {
        #[cfg(test)]
        {
            let hook = self
                .hooks
                .before_load_commit
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    fn run_before_commit_hook(&self) {
        #[cfg(test)]
        {
            let hook = self
                .hooks
                .before_commit
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(hook) = hook {
                hook();
            }
        }
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
        *self
            .team_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = team_id;
    }

    pub fn team_id(&self) -> Option<String> {
        self.team_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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

    /// Clone one session's model-facing history (point-in-time snapshot)。
    /// 会话不存在（从未收发过消息）时返回空列表。
    pub async fn snapshot_for(&self, session_id: &str) -> Vec<ChatMessage> {
        match self.history_for(session_id) {
            Some(history) => history.lock().clone(),
            None => Vec::new(),
        }
    }

    /// Append one durable session event and update the in-memory trunk
    /// projection cache under the same lock. The event log is the source of
    /// truth; `trunk_history` is its projection, trimmed to the token budget.
    ///
    /// The caller must hold `session.turn_lock` (or otherwise serialize
    /// writers) so the event order matches the projected message order.
    /// 写路径的自检（2026-11 懒加载）：未全量加载就改数据，说明有入口漏了
    /// `ensure_loaded`。**不 panic**（线上不值得为此挂掉），但必须留下痕迹
    /// ——否则"改了却存不下去"（未加载的档不会被写盘）会静默丢数据。
    fn warn_if_unloaded(&self, op: &str) {
        if !self.is_loaded() {
            tracing::warn!(
                op,
                stage = ?self.load_stage(),
                "session mutation before full load (missing ensure_loaded?)"
            );
        }
    }

    pub fn append_event(&self, event: echo_session::SessionEvent) {
        // 与加载提交互斥（读锁；写锁只在加载的同步提交窗口内持有）。
        // 加载期间 append 会阻塞到提交完成，然后排在文件事件之后。
        let _commit_guard = self.commit_barrier.read();
        self.warn_if_unloaded("append_event");
        let session = event.session().map(str::to_string);
        // 模型不可见事件（推理 / 模型往返观测）不改变投影：跳过重建。
        // 投影重建是 O(本会话事件数) 的整份克隆——长会话里每次 append
        // 都要重付，观测事件（2026-10-19 起每步一条）不该再摊这份成本。
        let affects_projection = event.affects_projection();
        self.event_log.append(event);
        if affects_projection {
            self.reproject(&session);
        } else if let Some(id) = session.as_deref() {
            // 仍然物化上下文句柄：历史查询/快照要能拿到该会话（幂等）。
            let _ = self.history_or_create(id);
        }
        self.mark_dirty();
    }

    /// 事件日志变化后刷新投影缓存：`Some(id)` 只刷新该会话（其余会话的
    /// 投影不受影响）；`None` 刷新全部会话（未归因事件的兜底路径）。
    fn reproject(&self, session: &Option<String>) {
        match session {
            Some(id) => {
                // 归因事件会物化其会话的上下文句柄（查询/快照可达）。
                let _ = self.history_or_create(id);
                // **只取该会话的事件**（2026-10 性能）：此前先 `log()`
                // 深克隆整份日志再过滤，等于每次 append 都付 O(全部会话)
                // 的字符串拷贝（实测 13 会话 / 4 MB 日志 → 每步工具调用
                // 多拷 4 MB；推理落日志后日志更大，这笔开销被放大）。
                let events = self.event_log.events_for_session(id);
                self.install_projection(id, &events);
            }
            None => {
                // 全量路径（加载 / 压缩 / 未归因事件）：全量日志取一次，
                // 各会话在内存里切分，不重复取锁。
                let log = self.event_log.log();
                let ids: Vec<String> = self
                    .session_histories
                    .iter()
                    .map(|entry| entry.key().clone())
                    .collect();
                for id in ids {
                    self.install_projection_from_log(&log, &id);
                }
            }
        }
    }

    /// 全量日志里切出该会话的事件后装入投影（罕见的全量路径专用）。
    fn install_projection_from_log(&self, log: &[echo_session::SessionEvent], session_id: &str) {
        let events: Vec<echo_session::SessionEvent> = log
            .iter()
            .filter(|event| event.session() == Some(session_id))
            .cloned()
            .collect();
        self.install_projection(session_id, &events);
    }

    /// 从**已按会话过滤**的事件集投影并写入句柄。
    fn install_projection(&self, session_id: &str, events: &[echo_session::SessionEvent]) {
        // 先在锁外完成投影。
        let mut projected = echo_session::derive::derive_messages(
            events,
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
            self.install_projection_from_log(&log, &id);
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

    /// 只取归属某会话的事件（不克隆其它会话的历史）。
    ///
    /// 只读查询（`store::query`，`list_sessions`/`read_session` 工具的后端）
    /// 走这条：多会话日志里按会话过滤能少拷 1/N，与投影热路径同口径。
    pub fn events_for_session(&self, session_id: &str) -> Vec<echo_session::SessionEvent> {
        self.event_log.events_for_session(session_id)
    }

    /// 导入某会话的完整事件集（会话迁移 P3-2b）：追加到事实来源日志并重投影
    /// 该会话（物化其历史句柄）。调用方保证事件已归因到 `session_id` 且不重复
    /// （重复导入会重复追加——幂等性由迁移层的 transfer_id 去重负责）。
    pub fn import_session_events(&self, session_id: &str, events: Vec<echo_session::SessionEvent>) {
        let _commit_guard = self.commit_barrier.read();
        if !events.is_empty() {
            self.event_log.extend(events);
        }
        let _ = self.history_or_create(session_id);
        self.reproject(&Some(session_id.to_string()));
        self.mark_dirty();
    }

    /// Insert an event after the last user event carrying `sequence`
    /// (concurrent-branch merge), then re-project the trunk cache.
    pub fn insert_event_after_sequence(&self, sequence: u64, event: echo_session::SessionEvent) {
        let _commit_guard = self.commit_barrier.read();
        let session = event.session().map(str::to_string);
        self.event_log.insert_after_sequence(sequence, event);
        self.reproject(&session);
        self.mark_dirty();
    }

    // ── Session header (fork/resume metadata) ───────────────────────────────

    /// Set the store's session header (lineage, origin, delegation depth).
    pub fn set_header(&self, header: echo_session::SessionHeader) {
        *self
            .header
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(header);
    }

    /// The store's session header, if set.
    pub fn header(&self) -> Option<echo_session::SessionHeader> {
        self.header
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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
                    echo_defs::message::ChatRole::System => "system",
                    echo_defs::message::ChatRole::User => "user",
                    echo_defs::message::ChatRole::Assistant => "assistant",
                    echo_defs::message::ChatRole::Tool => "tool",
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
                    "system" => echo_defs::message::ChatRole::System,
                    "user" => echo_defs::message::ChatRole::User,
                    "assistant" => echo_defs::message::ChatRole::Assistant,
                    "tool" => echo_defs::message::ChatRole::Tool,
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
///   （`channel.type == "group"` 时 scope=group，scope_id=群号；`instance` 为
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
    let instance = payload
        .get("instance")
        .or_else(|| payload.get("adapter"))
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
    // 实例名 == 平台名 = 默认实例，不写 account 后缀（与 SessionKey::account
    // 同一判据；不依赖任何写死的默认实例名）。
    let account = (!instance.eq_ignore_ascii_case(platform)).then(|| instance.to_string());
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
    use echo_defs::message::ChatRole;
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
                    files: vec![],
                    session: session.clone(),
                },
            )),
            ChatRole::Assistant => events.push(echo_session::SessionEvent::AssistantMessage(
                echo_session::event::AssistantMessage {
                    timestamp: 0,
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
mod tests;
