//! 压缩的**词汇**（计划 / 摘要 / 落地结果）。
//!
//! ## 为什么在 `echo-session`
//!
//! 这些类型是"事件日志将要被压缩"这件事的**契约**：
//!
//! - [`CompactGroup`] / [`CompactPreview`]：`TrunkStore::compact_preview` 的
//!   产出（谁会被替换、替换多少、前缀凭据）——日志自己算出来的；
//! - [`CompactionSummary`]：调用方（压缩策略）填回来的摘要 + 凭据，交给
//!   `apply_compaction` 落地；
//! - [`CompactionPrefix`]：被替换前缀的 SHA-256 指纹（提交时证明"换掉的
//!   仍是摘要描述的那段历史"）；
//! - [`CompactionApplied`]：落地的结果。
//!
//! 它们此前住在 `echo-agent::session`，于是**压缩策略**（怎么渲染 transcript、
//! 怎么分层裁剪、怎么让 LLM 出摘要）无法独立于宿主（全模块化 S3 的阻塞点）。
//! 事件日志的压缩契约本来就该和 `SessionEvent` 同层——这是词汇，不是实现。
//!
//! ## 与投影的关系
//!
//! 压缩**不改写**原始事件（原件归档在 `archives/`），只在日志里追加一条
//! [`crate::event::CompactionEvent`]；投影按"压缩范围 + 摘要"重放出可见历史。
//! 所以摘要正文**不带** `[历史摘要]` 标记——那是投影时加的。

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 一次压缩计划里**被替换前缀**的 SHA-256 指纹。
///
/// ## 为什么需要它
///
/// 预览与落地之间隔着归档 + 逐组 LLM 摘要，可能持续数分钟。这期间日志
/// 仍是可写的：普通消息只会 **append**（前缀不变，保留尾变长），但并发
/// 分支的回答会经 `EventLog::insert_after_sequence` **插到某条用户事件
/// 之后**——完全可能插进被替换前缀中间。此时若仍按预览长度切片，摘要
/// 描述的内容与实际被换掉的事件就错配了（静默丢档，只有归档可救）。
///
/// 落地必须在日志锁内证明"现在的前 N 条仍是预览时那 N 条"，否则拒绝
/// 该组。本类型就是这个证明：预览时对前缀做指纹，摘要携带同一个指纹。
///
/// ## 为什么是哈希，不是前缀副本
///
/// 前缀可能有上千条、数 MB；每个会话的待安装摘要再持一份完整副本会成倍
/// 放大内存（摘要生成阶段本来就同时持有转录与摘要）。SHA-256 固定 32
/// 字节、可 `Copy`，比较是常数时间。代价是纳秒级的哈希计算、可忽略的
/// 理论碰撞（2⁻²⁵⁶），以及对 serde 表示确定性的依赖——`SessionEvent`
/// 不含 `HashMap`、字段序固定，序列化逐字节稳定。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompactionPrefix([u8; 32]);

impl CompactionPrefix {
    /// 对一段事件前缀做指纹：事件按 serde JSON 逐个序列化，长度前缀
    /// （u64 LE）分隔，避免"拼接歧义"。
    pub fn of(events: &[crate::SessionEvent]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update((events.len() as u64).to_le_bytes());
        for event in events {
            let bytes =
                serde_json::to_vec(event).expect("SessionEvent 的 serde 表示不会序列化失败");
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
        let mut digest = [0u8; 32];
        digest.copy_from_slice(hasher.finalize().as_slice());
        Self(digest)
    }

    /// 原始 32 字节（诊断 / 持久化扩展用；比较请直接用 `==`）。
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// 小写十六进制全文（错误信息用）。
    pub fn to_hex(self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

impl fmt::Debug for CompactionPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CompactionPrefix({})", self.to_hex())
    }
}

impl fmt::Display for CompactionPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// 一个会话在一次压缩里的份额：按日志顺序将被替换的事件，以及规则回退
/// 摘要要用的计数。
#[derive(Debug, Clone)]
pub struct CompactGroup {
    /// 归属会话（`None` = 无归属的旧事件）。
    pub session: Option<String>,
    /// 该组将被替换的前缀事件（已切片）。
    pub replaced: Vec<crate::SessionEvent>,
    /// `replaced.len()`，方便调用方直接用。
    pub replaced_count: usize,
    /// `replaced` 里的用户消息条数（规则摘要用）。
    pub users: usize,
    /// `replaced` 里的工具调用条数（规则摘要用）。
    pub tools: usize,
    /// **被替换前缀**（`replaced[..replaced_count]`）的指纹。
    ///
    /// 预览时算好；[`CompactionSummary::from_group`] 原样带进摘要，落地
    /// 在日志锁内校验。不可缺省——没有凭据的摘要无法证明"替换的仍是
    /// 摘要描述的那段历史"。
    pub prefix_digest: CompactionPrefix,
    /// 预览时的历史代次（`TrunkStore::history_epoch`）。
    ///
    /// 清空 / 上一次压缩会推进代次（加载提交也持同一把写锁并在锁内比对）。
    /// 落地先比代次，能一眼拒绝"跨越了一次清空或另一次压缩"的过期计划；
    /// 不支持代次的嵌入式调用方填 `0`，但前缀指纹仍必须通过校验。
    pub history_epoch: u64,
}

/// [`crate::EventLog`] 之上的一次压缩**预览**：不改任何状态，所以调用方可以
/// 先归档、先摘要，再决定落地。
#[derive(Debug, Clone)]
pub struct CompactPreview {
    /// 会被压缩的组（每组至少一条被替换事件）。
    pub groups: Vec<CompactGroup>,
    /// 各组被替换前缀的事件总数。
    pub total_replaced: usize,
    /// 预览时日志里的事件总数（仅用于报告）。
    pub total_events: usize,
}

/// 一份待安装的摘要（按会话）。
#[derive(Debug, Clone)]
pub struct CompactionSummary {
    /// 这份摘要代表的会话（必须匹配某个预览组）。
    pub session: Option<String>,
    /// 摘要**正文**——不含 `[历史摘要]` 标记（投影时才加）。
    pub summary: String,
    /// 预览时该组被替换的前缀长度（事件条数）。
    ///
    /// 落地切片用它，而**不**按应用时点的 `len - keep_recent` 重算：窗口期
    /// 新到的事件会把保留尾最老的条目挤成"无摘要丢弃"（2026-10 审计）。
    pub replaced_count: usize,
    /// 预览时被替换前缀的指纹（**必填**，无 `Option` / 默认值可绕过）。
    ///
    /// 落地在日志锁内用当前前缀重算指纹；不一致即拒绝该组（保留原事件），
    /// 其他组照常。推荐用 [`Self::from_group`] 构造，避免漏填或与
    /// `replaced_count` 脱节。
    pub expected_prefix: CompactionPrefix,
    /// 预览时的历史代次（见 [`CompactGroup::history_epoch`]）。
    pub history_epoch: u64,
}

impl CompactionSummary {
    /// 从预览组构造待安装摘要：会话、切片长度、前缀指纹、历史代次一次
    /// 复制齐全，调用方不必逐字段抄（也抄不丢）。
    ///
    /// 这是策略层（`agent-module-loop`）把 `SummaryOrFallback` 变成落地
    /// 摘要的推荐入口：先按 `session` 找到预览组，再
    /// `CompactionSummary::from_group(group, summary_text)`。
    pub fn from_group(group: &CompactGroup, summary: impl Into<String>) -> Self {
        Self {
            session: group.session.clone(),
            summary: summary.into(),
            replaced_count: group.replaced_count,
            expected_prefix: group.prefix_digest,
            history_epoch: group.history_epoch,
        }
    }
}

/// 压缩/归档的**阶段**（面板据此决定进度条的形态）。
///
/// 走线（`serde` 名字即线上值）：`preview → archive → summarize → apply →
/// done`（失败时最后一步是 `failed`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionPhase {
    /// 计算压缩计划（只读，通常几十毫秒）。
    Preview,
    /// 写归档快照（磁盘 I/O；历史大时数百毫秒）。
    Archive,
    /// 逐组生成摘要——**唯一可能持续数分钟的阶段**，进度条的价值都在这里。
    Summarize,
    /// 落地（追加 `CompactionEvent` + 重投影）。
    Apply,
    /// 全部完成（`done == total`）。
    Done,
    /// 失败（`message` 里是原因）。
    Failed,
}

impl CompactionPhase {
    /// 线上字符串（与 `#[serde(rename_all = "snake_case")]` 一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Archive => "archive",
            Self::Summarize => "summarize",
            Self::Apply => "apply",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// 是否已到终态（面板据此停止转圈）。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// 一次压缩/归档的**进度快照**。
///
/// 为什么需要它：压缩（逐会话 LLM 摘要）可能持续数分钟，而此前 Core 只在
/// **开始**时发一句话、结束时发一句话——中间几分钟里面板只有一个静止的
/// 弹窗，用户无法区分"正在跑"与"卡死了"，只能刷新重来（而重来会撞上
/// `compact_lock`，看起来像毫无反应）。
#[derive(Debug, Clone)]
pub struct CompactionProgress {
    /// 当前阶段。
    pub phase: CompactionPhase,
    /// 已完成步数。
    pub done: usize,
    /// 总步数。`0` = 该阶段的长度不确定（例如预览、写快照），面板画
    /// 不确定进度条（`indeterminate`）。
    pub total: usize,
    /// 面向用户的一句话（面板直接显示，不再自己拼文案）。
    pub message: String,
    /// 本次压缩涉及的会话数（`summarize` 阶段的 `total` 同源，供面板显示
    /// "共 N 个会话"）。
    pub sessions: usize,
}

impl CompactionProgress {
    /// 不确定长度的阶段（预览 / 归档 / 落地 / 失败）。
    pub fn step(phase: CompactionPhase, message: impl Into<String>) -> Self {
        Self {
            phase,
            done: 0,
            total: 0,
            message: message.into(),
            sessions: 0,
        }
    }

    /// 有明确分子分母的阶段（逐组摘要 / 完成）。
    pub fn counted(
        phase: CompactionPhase,
        done: usize,
        total: usize,
        sessions: usize,
        message: impl Into<String>,
    ) -> Self {
        Self {
            phase,
            done,
            total,
            message: message.into(),
            sessions,
        }
    }

    /// 百分比（0–100）。`total == 0` 时返回 `None`（面板用不确定态）。
    pub fn percent(&self) -> Option<u8> {
        if self.total == 0 {
            return None;
        }
        let raw = (self.done.min(self.total) as f64 / self.total as f64) * 100.0;
        Some(raw.round().clamp(0.0, 100.0) as u8)
    }
}

/// 一次压缩落地的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionApplied {
    /// 被替换的事件总数。
    pub total_replaced: usize,
    /// 被压缩的会话组数。
    pub groups: usize,
    /// 每条摘要之后保留的未压缩事件数。
    pub kept: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::UserMessage;

    fn user(content: &str) -> crate::SessionEvent {
        crate::SessionEvent::UserMessage(UserMessage {
            content: content.into(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
            files: vec![],
            session: Some("s".into()),
        })
    }

    /// 摘要正文不含标记：标记由投影加（否则重放会得到两层标记）。
    #[test]
    fn summary_carries_no_marker() {
        let replaced = vec![user("a"), user("b"), user("c")];
        let summary = CompactionSummary {
            session: Some("s".into()),
            summary: "【当前目标】做完了".into(),
            replaced_count: replaced.len(),
            expected_prefix: CompactionPrefix::of(&replaced),
            history_epoch: 3,
        };
        assert!(!summary.summary.contains("[历史摘要]"));
    }

    /// 进度百分比：`total == 0` 一律 `None`（面板画不确定条），其余夹在 0–100
    /// 且不因 `done > total`（兜底分组等边界）溢出。
    #[test]
    fn progress_percent_handles_unknown_and_overflow() {
        let unknown = CompactionProgress::step(CompactionPhase::Preview, "…");
        assert_eq!(unknown.percent(), None, "长度不确定时不该给百分比");

        let mid = CompactionProgress::counted(CompactionPhase::Summarize, 3, 7, 7, "…");
        assert_eq!(mid.percent(), Some(43));

        let done = CompactionProgress::counted(CompactionPhase::Done, 7, 7, 7, "…");
        assert_eq!(done.percent(), Some(100));

        // done > total（理论上不该发生，但进度条更不能画出 >100%）
        let over = CompactionProgress::counted(CompactionPhase::Summarize, 9, 7, 7, "…");
        assert_eq!(over.percent(), Some(100));
    }

    /// 线上阶段名是**稳定字符串**（面板按它分支），改名等于破坏兼容。
    #[test]
    fn phase_wire_names_are_stable() {
        assert_eq!(CompactionPhase::Preview.as_str(), "preview");
        assert_eq!(CompactionPhase::Archive.as_str(), "archive");
        assert_eq!(CompactionPhase::Summarize.as_str(), "summarize");
        assert_eq!(CompactionPhase::Apply.as_str(), "apply");
        assert_eq!(CompactionPhase::Done.as_str(), "done");
        assert_eq!(CompactionPhase::Failed.as_str(), "failed");
        // 与 serde 的 rename_all 必须一致（两条路径产出的值不能分叉）
        let json = serde_json::to_string(&CompactionPhase::Summarize).expect("serialize");
        assert_eq!(json, "\"summarize\"");
    }

    /// 终态判定：面板靠它停止转圈（`apply` 之后还有 `done`，不能提前收）。
    #[test]
    fn only_done_and_failed_are_terminal() {
        assert!(!CompactionPhase::Preview.is_terminal());
        assert!(!CompactionPhase::Archive.is_terminal());
        assert!(!CompactionPhase::Summarize.is_terminal());
        assert!(!CompactionPhase::Apply.is_terminal());
        assert!(CompactionPhase::Done.is_terminal());
        assert!(CompactionPhase::Failed.is_terminal());
    }

    /// `replaced_count` 是**预览时**的量，与落地时的日志长度无关——这是
    /// 2026-10 审计修复的核心，值本身不该被落地逻辑改写。
    #[test]
    fn replaced_count_is_a_preview_snapshot() {
        let replaced = vec![user("a")];
        let summary = CompactionSummary {
            session: None,
            summary: "x".into(),
            replaced_count: 7,
            expected_prefix: CompactionPrefix::of(&replaced),
            history_epoch: 0,
        };
        assert_eq!(summary.replaced_count, 7);
    }

    /// 指纹的可区分性：append（前缀不变）必须相同；插入到前缀中间必须
    /// 不同——落地据此拒绝"摘要与内容错配"的计划。
    #[test]
    fn prefix_digest_distinguishes_append_from_insertion() {
        let base = vec![user("a"), user("b"), user("c")];
        let expected = CompactionPrefix::of(&base);

        let mut appended = base.clone();
        appended.push(user("d"));
        assert_eq!(
            CompactionPrefix::of(&appended[..3]),
            expected,
            "append 不改前缀，凭据必须仍有效"
        );

        let mut inserted = base.clone();
        inserted.insert(1, user("迟到回答"));
        assert_ne!(
            CompactionPrefix::of(&inserted[..3]),
            expected,
            "插入改前缀，凭据必须失效"
        );

        // 长度也是凭据的一部分：短前缀不能冒充长前缀。
        assert_ne!(CompactionPrefix::of(&base[..2]), expected);
    }

    /// `from_group` 一次复制齐全：会话、切片长度、前缀指纹、代次都来自
    /// 预览组——调用方无法"漏传"校验凭据。
    #[test]
    fn from_group_copies_the_preview_credential() {
        let replaced = vec![user("a"), user("b")];
        let group = CompactGroup {
            session: Some("s".into()),
            replaced_count: replaced.len(),
            replaced: replaced.clone(),
            users: 2,
            tools: 0,
            prefix_digest: CompactionPrefix::of(&replaced),
            history_epoch: 42,
        };
        let summary = CompactionSummary::from_group(&group, "两条的摘要");
        assert_eq!(summary.session.as_deref(), Some("s"));
        assert_eq!(summary.replaced_count, 2);
        assert_eq!(summary.expected_prefix, group.prefix_digest);
        assert_eq!(summary.history_epoch, 42);
        assert_eq!(summary.summary, "两条的摘要");
    }
}
