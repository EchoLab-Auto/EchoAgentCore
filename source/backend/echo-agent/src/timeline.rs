//! 显示时间线的**宿主端口**与兼容路径（策略在 [`echo_plugin_management::timeline`]）。
//!
//! ## 搬家后的分工
//!
//! | 策略（插件里） | 宿主（本文件） |
//! |---|---|
//! | 哪些事件进时间线（后台 hook / 定时任务 / 生命周期怎么处理） | 时间线存在哪（`TrunkStore`：限长、落盘、dirty） |
//! | 字段怎么映射（来源标签、图片规范化、工具概要截断） | 单调序号（增量同步游标） |
//! | 工具完成态如何在原地合并 | 就地更新的**原子操作**（找运行中条目 + 推进 seq） |
//! | 推理条目发往前端时的瘦身阈值 | — |
//!
//! 端口刻意**不给 `timeline_mut()`**：那会让插件认识宿主的锁类型，"seam 变
//! 影子"就是这么发生的。就地更新被收敛成 [`TimelineHost::complete_running_tool`]
//! 一个语义方法。
//!
//! 本模块保留 `crate::timeline::…` 这条路径（`session::history` 的派生显示
//! 历史与 `commands.rs` 的 wire 瘦身都在用），内容全部是 re-export。

use echo_plugin_management::timeline::{TimelineHost, ToolCompletion};
use echo_protocol::TimelineMessage;

use crate::session::TrunkStore;

pub use echo_plugin_management::timeline::{
    elide_reasoning_for_wire, summarize_timeline_value, timeline_timer_task, truncate_one_line,
    TimelineProjector, WIRE_REASONING_HEAD_CHARS, WIRE_REASONING_KEEP_FULL,
};

/// 宿主侧端口实现：把投影器要的三件事接到 `TrunkStore`。
///
/// 包一层 `Arc<TrunkStore>` 而不是直接给 `TrunkStore` 是因为投影器要求
/// `Arc<H>`（它被事件总线闭包共享）。
pub struct TrunkTimelineHost {
    trunk: TrunkStore,
}

impl TrunkTimelineHost {
    pub fn new(trunk: TrunkStore) -> Self {
        Self { trunk }
    }
}

impl TimelineHost for TrunkTimelineHost {
    fn push(&self, message: TimelineMessage) {
        self.trunk.push_timeline(message);
    }

    fn next_seq(&self) -> u64 {
        self.trunk.bump_timeline_seq()
    }

    fn complete_running_tool(&self, patch: ToolCompletion<'_>) -> bool {
        let mut timeline = self.trunk.timeline_mut();
        let found = if patch.tool_call_id.is_empty() {
            // 2026-09 前的工具事件不带 call id：按「同会话 + 同工具名 +
            // 尚无 output」退化匹配。
            timeline.iter_mut().rev().find(|entry| {
                entry.kind == "tool"
                    && entry.session_id == patch.session_id
                    && entry.tool.as_ref().is_some_and(|tool| {
                        tool.name == patch.tool_name && tool.output.is_none()
                    })
            })
        } else {
            timeline.iter_mut().rev().find(|entry| {
                entry.kind == "tool"
                    && entry.session_id == patch.session_id
                    && entry.tool.as_ref().is_some_and(|tool| {
                        tool.tool_call_id == patch.tool_call_id && tool.output.is_none()
                    })
            })
        };
        let Some(entry) = found else {
            return false;
        };
        if let Some(tool) = entry.tool.as_mut() {
            tool.output = Some(patch.output.to_string());
            tool.failed = patch.failed;
            tool.timed_out = patch.timed_out;
            tool.elapsed_ms = patch.elapsed_ms;
        }
        // 就地更新也推进条目 seq，否则增量同步（since_seq）永远看不到这次
        // 完成态，前端会一直显示 running。
        let seq = self.trunk.bump_timeline_seq();
        entry.seq = seq;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_protocol::TimelineTool;

    fn trunk() -> TrunkStore {
        TrunkStore::new(100_000)
    }

    fn running_tool(trunk: &TrunkStore, call_id: &str) {
        trunk.push_timeline(TimelineMessage::tool(
            "read_file",
            "s".to_string(),
            0,
            TimelineTool {
                name: "read_file".into(),
                input: "{}".into(),
                output: None,
                failed: false,
                tool_call_id: call_id.into(),
                timed_out: false,
                started_at_ms: None,
                elapsed_ms: None,
            },
        ));
    }

    /// 按 `tool_call_id` 就地更新，并**推进 seq**（增量同步必须看到完成态）。
    #[test]
    fn completing_a_running_tool_bumps_its_seq() {
        let trunk = trunk();
        running_tool(&trunk, "c1");
        let before = trunk.timeline_snapshot()[0].seq;
        let host = TrunkTimelineHost::new(trunk.clone());
        let updated = host.complete_running_tool(ToolCompletion {
            session_id: "s",
            tool_name: "read_file",
            tool_call_id: "c1",
            output: "done",
            failed: false,
            timed_out: false,
            elapsed_ms: Some(7),
        });
        assert!(updated, "应找到运行中的条目");
        let snapshot = trunk.timeline_snapshot();
        assert_eq!(snapshot.len(), 1, "就地更新不是追加");
        assert_eq!(snapshot[0].tool.as_ref().unwrap().output.as_deref(), Some("done"));
        assert!(
            snapshot[0].seq > before,
            "完成态必须推进 seq（否则前端一直显示 running）"
        );
    }

    /// 空 call id：退化匹配同会话同工具名（旧事件兼容）。
    #[test]
    fn empty_call_id_falls_back_to_name_matching() {
        let trunk = trunk();
        running_tool(&trunk, "");
        let host = TrunkTimelineHost::new(trunk.clone());
        assert!(host.complete_running_tool(ToolCompletion {
            session_id: "s",
            tool_name: "read_file",
            tool_call_id: "",
            output: "done",
            failed: false,
            timed_out: false,
            elapsed_ms: None,
        }));
    }

    /// 没有运行中的条目：返回 `false`（调用方追加完成态条目）。
    #[test]
    fn no_running_entry_reports_false() {
        let host = TrunkTimelineHost::new(trunk());
        assert!(!host.complete_running_tool(ToolCompletion {
            session_id: "s",
            tool_name: "read_file",
            tool_call_id: "ghost",
            output: "done",
            failed: false,
            timed_out: false,
            elapsed_ms: None,
        }));
    }
}
