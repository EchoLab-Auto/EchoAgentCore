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
