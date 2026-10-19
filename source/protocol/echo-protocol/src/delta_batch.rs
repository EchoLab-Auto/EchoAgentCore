//! 流式增量合批（2026-10）：把窗口内的同类增量合并成一帧再下发。
//!
//! ## 为什么在**下发侧**合批
//!
//! provider 的流式节奏很碎：一段中等长度的回答实测产生 20–60 个增量，每个
//! 增量在 Panel 侧都是一次完整链路——JSON 解析 → `reduce` → 合并
//! `sessionHistory`（O(窗口)）→ computed → 组件 diff。单项成本已经压到
//! ~1 ms，但**次数**是乘数：一次回答累计 20–60 ms 主线程，长回答更多；同时
//! 每个增量都是一帧 WS 消息（分配 + 序列化 + 网络 + 对端解析）。
//!
//! 合批把这个乘数降下来，而不改变任何语义：合并后的 `delta` 是原增量的
//! 顺序拼接，Panel 本来就只做「追加到当前流式行」，因此逐字到达与成块到达
//! 的最终文本完全一致。
//!
//! ## 保序是硬约束
//!
//! 只合并**连续的同类增量**（会话 + 分支 + 角色一致）。任何非增量事件
//! （`AgentOutput` / `ToolCall` / `ToolResult` / 生命周期事件……）到达时，
//! 先把挂起的增量**全部冲出**再发它——否则会出现"工具调用先到、它前面的
//! 叙述晚到"，而 Panel 的 `freezeStreams` 正是以工具开始为分步边界。
//!
//! ## 谁受影响、谁不受
//!
//! 合批发生在**前端扇出**这一层：时间线投影（`TimelineProjector`）与其它
//! 进程内观察者收到的事件不变（它们本来也忽略增量）。因此持久化时间线、
//! TUI 等消费方行为完全不受影响。

use std::time::{Duration, Instant};

use crate::event::BackendEvent;

/// 一条待下发的合并事件。
#[derive(Debug, Clone)]
pub struct Emit {
    pub event: BackendEvent,
    pub entry: Option<String>,
}

/// 合批窗口（同一段连续的增量在该窗口内的部分合并为一帧）。
pub const DEFAULT_BATCH_WINDOW: Duration = Duration::from_millis(60);

/// 挂起队列的长度上限。正常情况只有 1–2 条（一个会话的一段正文 + 一段推理）；
/// 上限只用于防御异常输入——超限时按最旧优先冲出，绝不无界增长。
const MAX_PENDING: usize = 32;

/// 增量合批器。**不是**线程安全的：调用方（WS 扇出的单任务循环）独占。
#[derive(Debug)]
pub struct DeltaBatcher {
    window: Duration,
    pending: Vec<Pending>,
}

#[derive(Debug)]
struct Pending {
    /// 合并键：只有键相同的**连续**增量才合并（保序）。
    key: (String, String, &'static str),
    entry: Option<String>,
    /// 合并中的事件（同一 key 的 delta 已拼接）。
    event: BackendEvent,
    /// 该条**必须**冲出的时刻（从它第一条增量算起，不随合并顺延）。
    deadline: Instant,
}

/// 增量（可合批）事件 → (会话, 分支, 角色)；其它事件返回 `None`。
fn batch_key(event: &BackendEvent) -> Option<(String, String, &'static str)> {
    match event {
        BackendEvent::AgentContentDelta {
            session_id,
            branch_id,
            ..
        } => Some((session_id.clone(), branch_id.clone(), "backend")),
        BackendEvent::AgentReasoningDelta {
            session_id,
            branch_id,
            ..
        } => Some((session_id.clone(), branch_id.clone(), "reasoning")),
        _ => None,
    }
}

/// 把 `next` 的增量文本接到 `into` 上（同 key 已校验）。
fn append_delta(into: &mut BackendEvent, next: &BackendEvent) {
    match (into, next) {
        (
            BackendEvent::AgentContentDelta { delta: acc, .. },
            BackendEvent::AgentContentDelta { delta, .. },
        )
        | (
            BackendEvent::AgentReasoningDelta { delta: acc, .. },
            BackendEvent::AgentReasoningDelta { delta, .. },
        ) => acc.push_str(delta),
        _ => {
            debug_assert!(false, "append_delta: mismatched kinds");
        }
    }
}

impl Default for DeltaBatcher {
    fn default() -> Self {
        Self::new(DEFAULT_BATCH_WINDOW)
    }
}

impl DeltaBatcher {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: Vec::new(),
        }
    }

    /// 当前挂起的条目数（测试/观测用）。
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// 收一条事件，返回**现在就该下发**的事件（按序）。
    ///
    /// - 增量且可与队尾合并 → 返回空（等窗口或后续事件触发）；
    /// - 增量但键不同（另一分支/另一段推理）→ 也挂起（返回空）。注意只合并
    ///   **连续**同键增量：交错到达的两条流各自单发（不比改前差），绝不把
    ///   不同分支的文本拼进同一帧（否则 Panel 分不清它属于哪一行）；
    /// - 队列满 → 先冲出全部挂起，再挂起新的；
    /// - 非增量 → 先冲出全部挂起，再返回它本身（**严格保序**：工具开始是
    ///   分步边界，不能越过它前面的叙述）。
    pub fn push(
        &mut self,
        event: BackendEvent,
        entry: Option<String>,
        now: Instant,
    ) -> Vec<Emit> {
        let Some(key) = batch_key(&event) else {
            // 非增量：冲出挂起，再发自己。
            let mut out = self.drain();
            out.push(Emit { event, entry });
            return out;
        };

        // 与队尾同类且同 entry → 合并（这是合批收益的来源）。
        if let Some(last) = self.pending.last_mut() {
            if last.key == key && last.entry == entry {
                append_delta(&mut last.event, &event);
                // deadline 不推迟：从第一条增量算起，保证最坏延迟 = window。
                return Vec::new();
            }
        }

        // 队列将满：先把旧的冲出（防御异常输入，避免无界）。
        let mut out = if self.pending.len() >= MAX_PENDING {
            self.drain()
        } else {
            Vec::new()
        };
        self.pending.push(Pending {
            key,
            entry,
            event,
            deadline: now + self.window,
        });
        out.shrink_to_fit();
        out
    }

    /// 取出已到期的挂起条目（按挂起顺序）。
    pub fn take_due(&mut self, now: Instant) -> Vec<Emit> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].deadline <= now {
                let item = self.pending.remove(i);
                out.push(Emit {
                    event: item.event,
                    entry: item.entry,
                });
                // remove 后下标不前进（后续元素前移）
            } else {
                i += 1;
            }
        }
        out
    }

    /// 冲出全部挂起条目（按挂起顺序）。
    pub fn drain(&mut self) -> Vec<Emit> {
        self.pending
            .drain(..)
            .map(|item| Emit {
                event: item.event,
                entry: item.entry,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(session: &str, branch: &str, delta: &str) -> BackendEvent {
        BackendEvent::AgentContentDelta {
            session_id: session.into(),
            branch_id: branch.into(),
            delta: delta.into(),
            team_id: None,
        }
    }

    fn reasoning(session: &str, branch: &str, delta: &str) -> BackendEvent {
        BackendEvent::AgentReasoningDelta {
            session_id: session.into(),
            branch_id: branch.into(),
            delta: delta.into(),
            team_id: None,
        }
    }

    fn tool_call(session: &str) -> BackendEvent {
        BackendEvent::ToolCall {
            session_id: session.into(),
            team_id: None,
            tool_name: "bash".into(),
            arguments: "{}".into(),
            tool_call_id: "c1".into(),
            branch_id: String::new(),
            started_at_ms: None,
        }
    }

    fn delta_text(event: &BackendEvent) -> &str {
        match event {
            BackendEvent::AgentContentDelta { delta, .. }
            | BackendEvent::AgentReasoningDelta { delta, .. } => delta,
            other => panic!("not a delta: {other:?}"),
        }
    }

    #[test]
    fn consecutive_deltas_of_one_entry_merge_into_one_emit() {
        let mut b = DeltaBatcher::new(Duration::from_millis(60));
        let t0 = Instant::now();
        assert!(b.push(content("s", "b", "he"), Some("e1".into()), t0).is_empty());
        assert!(b.push(content("s", "b", "llo"), Some("e1".into()), t0).is_empty());
        assert!(b.push(content("s", "b", " world"), Some("e1".into()), t0).is_empty());
        assert_eq!(b.pending_len(), 1);
        let out = b.take_due(t0 + Duration::from_millis(61));
        assert_eq!(out.len(), 1);
        assert_eq!(delta_text(&out[0].event), "hello world");
        assert_eq!(out[0].entry.as_deref(), Some("e1"));
    }

    #[test]
    fn nothing_emitted_before_deadline() {
        let mut b = DeltaBatcher::new(Duration::from_millis(60));
        let t0 = Instant::now();
        b.push(content("s", "b", "x"), None, t0);
        assert!(b.take_due(t0 + Duration::from_millis(30)).is_empty());
        assert_eq!(b.pending_len(), 1);
    }

    #[test]
    fn interleaved_branches_are_not_merged_but_keep_exact_order() {
        let mut b = DeltaBatcher::new(Duration::from_millis(60));
        let t0 = Instant::now();
        // 只合并**连续**同键增量（严格保序）。两个分支交错时各自单发——
        // 不比改前差，且绝不把不同分支的文本拼进同一帧。
        assert!(b.push(content("s", "b1", "a"), None, t0).is_empty());
        assert!(b.push(content("s", "b2", "b"), None, t0).is_empty());
        assert!(b.push(content("s", "b1", "A"), None, t0).is_empty());
        assert!(b.push(content("s", "b2", "B"), None, t0).is_empty());
        let out = b.drain();
        assert_eq!(out.len(), 4);
        let texts: Vec<&str> = out.iter().map(|e| delta_text(&e.event)).collect();
        assert_eq!(texts, vec!["a", "b", "A", "B"]);
    }

    #[test]
    fn different_entries_are_never_merged_into_one_frame() {
        let mut b = DeltaBatcher::new(Duration::from_millis(60));
        let t0 = Instant::now();
        b.push(content("s", "b", "a"), Some("e1".into()), t0);
        // 不同 entry 绝不能进同一帧——否则 Panel 无法判断这段文本属于哪一行
        assert!(b.push(content("s", "b", "b"), Some("e2".into()), t0).is_empty());
        let out = b.drain();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].entry.as_deref(), Some("e1"));
        assert_eq!(delta_text(&out[0].event), "a");
        assert_eq!(out[1].entry.as_deref(), Some("e2"));
        assert_eq!(delta_text(&out[1].event), "b");
    }

    #[test]
    fn reasoning_and_backend_are_distinct_streams() {
        let mut b = DeltaBatcher::new(Duration::from_millis(60));
        let t0 = Instant::now();
        b.push(content("s", "b", "answer"), None, t0);
        b.push(reasoning("s", "b", "think"), None, t0);
        let out = b.drain();
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0].event, BackendEvent::AgentContentDelta { .. }));
        assert!(matches!(out[1].event, BackendEvent::AgentReasoningDelta { .. }));
    }

    #[test]
    fn non_delta_event_flushes_pending_first_keeping_order() {
        let mut b = DeltaBatcher::new(Duration::from_millis(60));
        let t0 = Instant::now();
        b.push(content("s", "b", "叙述"), None, t0);
        let out = b.push(tool_call("s"), Some("tool:s:c1".into()), t0);
        // 顺序：先叙述（合并后的一帧），再工具调用
        assert_eq!(out.len(), 2);
        assert_eq!(delta_text(&out[0].event), "叙述");
        assert!(matches!(out[1].event, BackendEvent::ToolCall { .. }));
        assert_eq!(out[1].entry.as_deref(), Some("tool:s:c1"));
        assert_eq!(b.pending_len(), 0);
    }

    #[test]
    fn deadline_is_not_extended_by_later_deltas() {
        let mut b = DeltaBatcher::new(Duration::from_millis(60));
        let t0 = Instant::now();
        b.push(content("s", "b", "a"), None, t0);
        // 后续增量晚到 50ms，仍不应把 deadline 推到 110ms
        b.push(content("s", "b", "b"), None, t0 + Duration::from_millis(50));
        // 61ms 时该条已到期（最坏延迟 = 一个窗口）
        assert_eq!(b.take_due(t0 + Duration::from_millis(61)).len(), 1);
    }

    #[test]
    fn queue_is_bounded_and_flushes_oldest_first() {
        let mut b = DeltaBatcher::new(Duration::from_millis(60));
        let t0 = Instant::now();
        let mut emitted: Vec<String> = Vec::new();
        for i in 0..(MAX_PENDING + 5) {
            let out = b.push(content(&format!("s{i}"), "b", &format!("d{i}")), None, t0);
            for emit in out {
                emitted.push(delta_text(&emit.event).to_string());
            }
            assert!(b.pending_len() <= MAX_PENDING);
        }
        // 超出上限时按最旧优先冲出（顺序 = 到达顺序）
        assert!(emitted.len() >= 5);
        assert_eq!(emitted[0], "d0");
    }

    #[test]
    fn drain_returns_everything_in_order() {
        let mut b = DeltaBatcher::new(Duration::from_millis(60));
        let t0 = Instant::now();
        b.push(content("s", "b1", "one"), None, t0);
        b.push(content("s", "b2", "two"), None, t0);
        let out = b.drain();
        assert_eq!(out.len(), 2);
        assert_eq!(delta_text(&out[0].event), "one");
        assert_eq!(delta_text(&out[1].event), "two");
        assert_eq!(b.pending_len(), 0);
    }
}
