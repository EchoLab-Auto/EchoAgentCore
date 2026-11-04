//! 压缩的**词汇**（计划 / 摘要 / 落地结果）。
//!
//! ## 为什么在 `echo-session`
//!
//! 这四种类型是"事件日志将要被压缩"这件事的**契约**：
//!
//! - [`CompactGroup`] / [`CompactPreview`]：`TrunkStore::compact_preview` 的
//!   产出（谁会被替换、替换多少）——日志自己算出来的；
//! - [`CompactionSummary`]：调用方（压缩策略）填回来的摘要，交给
//!   `apply_compaction` 落地；
//! - [`CompactionApplied`]：落地的结果。
//!
//! 它们此前住在 `echo-agent::session`，于是**压缩策略**（怎么渲染 transcript、
//! 怎么分层裁剪、怎么让 LLM 出摘要）无法独立于宿主（全插件化 S3 的阻塞点）。
//! 事件日志的压缩契约本来就该和 `SessionEvent` 同层——这是词汇，不是实现。
//!
//! ## 与投影的关系
//!
//! 压缩**不改写**原始事件（原件归档在 `archives/`），只在日志里追加一条
//! [`crate::event::CompactionEvent`]；投影按"压缩范围 + 摘要"重放出可见历史。
//! 所以摘要正文**不带** `[历史摘要]` 标记——那是投影时加的。

use serde::{Deserialize, Serialize};

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

    /// 摘要正文不含标记：标记由投影加（否则重放会得到两层标记）。
    #[test]
    fn summary_carries_no_marker() {
        let summary = CompactionSummary {
            session: Some("s".into()),
            summary: "【当前目标】做完了".into(),
            replaced_count: 3,
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
        let summary = CompactionSummary {
            session: None,
            summary: "x".into(),
            replaced_count: 7,
        };
        assert_eq!(summary.replaced_count, 7);
    }
}
