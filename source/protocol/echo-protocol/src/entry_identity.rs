//! 条目身份追踪（EntryId）——把「同一逻辑条目的多个事件」绑到同一个 id 上。
//!
//! 归属（2026-10，解耦计划 Phase 3 ⑧）：本模块产出的是**线上信封字段**
//! ——它和 `seq` 一样附加在帧顶层（见本 crate 的 `serialize_event_identified`），
//! 不是某个消费者的内部实现。因此它属于协议层：管理面插件与任何未来的
//! 前端通道都共用同一套 id 规则，不各生成一份。
//!
//! ## 为什么需要
//!
//! Panel 的实时通道与时间线通道是两条独立的路：同一件事既有 `AgentOutput`
//! 这样的**实时事件**，又有随后从时间线拉取的**权威条目**。而实时事件里
//! 没有任何身份字段——Panel 只能靠「同角色 + 同时间 + 内容前缀互包含」去猜
//! 哪条实时行对应哪条权威条目（`state.ts` 的 `coveredBy`）。
//!
//! 这套推断的每个边界场景都变成过线上事故：思考 chip 消失、思考���到正文
//! 后面、工具行一直读秒、同一条回答显示两遍……（背景与全案见
//! `document/design/entry-identity-p1.md`）。
//!
//! ## 做法：在**信封层**注入，不动事件契约
//!
//! 与 `seq` 同一手法（`bridge.rs::serialize_event_with_seq`）：id 作为
//! **顶层字段**附加在帧上，不塞进 `BackendEvent` 各变体。好处是零构造点
//! 改动（`BackendEvent` 有 70+ 处构造）、旧版 Panel 直接忽略多余字段。
//!
//! ## id 的规则
//!
//! | 事件 | entry |
//! | --- | --- |
//! | `ToolCall` / `ToolResult`（`tool_call_id` 非空） | `tool:<session>:<tool_call_id>` |
//! | 同上但 `tool_call_id` 为空 | 不分配（Panel 回退"同名最后一个 running"兜底） |
//! | `AgentContentDelta` / `AgentOutput` | `backend:<session>:<branch>:<n>` |
//! | `AgentReasoningDelta` / `AgentReasoning` | `reasoning:<session>:<branch>:<n>` |
//!
//! `<n>` 是**进程级单调计数**（不是每会话计数）：只要求"同一段流式共享同一个
//! n"，全局递增天然满足，且不需要按会话维护计数器表（避免会话数增长带来的
//! 无界状态）。因此 n 在同一 (session, branch) 内会跳跃——这不影响任何语义。
//!
//! 三点语义：
//!
//! 1. 流式增量与它的**终值共享同一个 n**（关键：Panel 据此把终值当作同一行
//!    的收敛，而不是"新开一行再对账"）；终值到达即封口，下一段开新 n。
//! 2. `ToolCall` 出现即**封口**该会话的正文/推理条目——与 Panel 的
//!    `freezeStreams` 语义一致（工具开始后该步叙述不会再有终值）。
//! 3. 工具类走 `tool_call_id` 而不是计数器：`ToolResult` 可能先于 `ToolCall`
//!    到达（供应商乱序/丢帧），用 id 才能在后到的那条上得到同一个值。

use std::collections::HashMap;

use crate::event::BackendEvent;

const KIND_BACKEND: &str = "backend";
const KIND_REASONING: &str = "reasoning";

/// 条目身份追踪器。
///
/// 只被 WS 扇出（`EventBroker`）持有——每条事件只观察一次，因此计数推进与
/// 所有订阅者看到的 id 严格一致（多窗口不会各算一套）。
#[derive(Default)]
pub struct EntryTracker {
    /// `(session, branch)` → 当前**未封口**的正文条目号。
    backend: HashMap<(String, String), u64>,
    /// `(session, branch)` → 当前**未封口**的推理条目号。
    reasoning: HashMap<(String, String), u64>,
    /// session → 最近创建/更新的正文条目 `(branch, turn)`。
    ///
    /// 用途：`AgentOutput` 的 `branch_id` 是 `Option`（后端路径不带分支），
    /// 此时要落在"该会话最新的那段正文"上——与 Panel 侧"branch_id 缺失时
    /// 取最新流式行"的对账语义一致。
    last_backend: HashMap<String, (String, u64)>,
    /// 进程级条目号计数（见模块注释：全局递增，避免按会话维护计数器表）。
    next_turn: u64,
}

impl std::fmt::Debug for EntryTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryTracker")
            .field("live_backend", &self.backend.len())
            .field("live_reasoning", &self.reasoning.len())
            .field("last_backend", &self.last_backend.len())
            .field("next_turn", &self.next_turn)
            .finish()
    }
}

impl EntryTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 观察一条事件，返回它所属条目的 id（无身份概念的事件返回 `None`）。
    ///
    /// **有副作用**（推进/封口条目号）：每条事件必须只观察一次。
    pub fn observe(&mut self, event: &BackendEvent) -> Option<String> {
        match event {
            // ── 工具：id 由 tool_call_id 决定（与到达顺序无关）──
            BackendEvent::ToolResult {
                session_id,
                tool_call_id,
                ..
            } => tool_entry(session_id, tool_call_id),

            // ── 工具开始：先封口该会话的正文/推理，再给出工具条目 id ──
            BackendEvent::ToolCall {
                session_id,
                tool_call_id,
                ..
            } => {
                // 工具开始即断定"这一步的叙述不再有终值"（与 Panel 的
                // freezeStreams 一致）：此后到达的增量开新条目。
                self.backend.retain(|(s, _), _| s != session_id);
                self.reasoning.retain(|(s, _), _| s != session_id);
                self.last_backend.remove(session_id);
                tool_entry(session_id, tool_call_id)
            }

            // ── 正文流：增量与终值共享条目号；终值封口 ──
            BackendEvent::AgentContentDelta {
                session_id,
                branch_id,
                ..
            } => Some(self.live(session_id, branch_id, KIND_BACKEND)),
            BackendEvent::AgentOutput {
                session_id,
                branch_id,
                ..
            } => {
                // `branch_id` 缺失（后端路径）时落在"该会话最新的那段正文"上。
                let branch = match branch_id {
                    Some(branch) => branch.clone(),
                    None => self.last_backend.get(session_id).map(|(b, _)| b.clone())?,
                };
                let id = self.live(session_id, &branch, KIND_BACKEND);
                self.backend.remove(&(session_id.clone(), branch));
                Some(id)
            }

            // ── 推理流：同上 ──
            BackendEvent::AgentReasoningDelta {
                session_id,
                branch_id,
                ..
            } => Some(self.live(session_id, branch_id, KIND_REASONING)),
            BackendEvent::AgentReasoning {
                session_id,
                branch_id,
                ..
            } => {
                let id = self.live(session_id, branch_id, KIND_REASONING);
                self.reasoning
                    .remove(&(session_id.clone(), branch_id.clone()));
                Some(id)
            }

            _ => None,
        }
    }

    /// 取（必要时创建）某 `(session, branch, kind)` 的当前条目号并拼 id。
    fn live(&mut self, session_id: &str, branch_id: &str, kind: &'static str) -> String {
        let key = (session_id.to_string(), branch_id.to_string());
        let slot = if kind == KIND_BACKEND {
            &mut self.backend
        } else {
            &mut self.reasoning
        };
        let turn = match slot.get(&key) {
            Some(turn) => *turn,
            None => {
                self.next_turn += 1;
                let turn = self.next_turn;
                slot.insert(key, turn);
                turn
            }
        };
        if kind == KIND_BACKEND {
            self.last_backend
                .insert(session_id.to_string(), (branch_id.to_string(), turn));
        }
        format!("{kind}:{session_id}:{branch_id}:{turn}")
    }
}

fn tool_entry(session_id: &str, tool_call_id: &str) -> Option<String> {
    if tool_call_id.is_empty() {
        // 旧 Core / 供应商未回 id：Panel 侧按"同名最后一个 running"兜底。
        // 这里不分配，避免给出会错配的假身份。
        return None;
    }
    Some(format!("tool:{session_id}:{tool_call_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(session: &str, branch: &str, text: &str) -> BackendEvent {
        BackendEvent::AgentContentDelta {
            session_id: session.into(),
            branch_id: branch.into(),
            delta: text.into(),
            team_id: None,
        }
    }

    fn output(session: &str, branch: &str, text: &str) -> BackendEvent {
        BackendEvent::AgentOutput {
            session_id: session.into(),
            branch_id: Some(branch.into()),
            content: text.into(),
            team_id: None,
        }
    }

    fn reasoning_delta(session: &str, branch: &str) -> BackendEvent {
        BackendEvent::AgentReasoningDelta {
            session_id: session.into(),
            branch_id: branch.into(),
            delta: "think".into(),
            team_id: None,
        }
    }

    fn tool_call(session: &str, id: &str) -> BackendEvent {
        BackendEvent::ToolCall {
            session_id: session.into(),
            team_id: None,
            tool_name: "bash".into(),
            arguments: "{}".into(),
            tool_call_id: id.into(),
            branch_id: String::new(),
            started_at_ms: None,
        }
    }

    fn tool_result(session: &str, id: &str, text: &str) -> BackendEvent {
        BackendEvent::ToolResult {
            session_id: session.into(),
            team_id: None,
            tool_name: "bash".into(),
            result: text.into(),
            tool_call_id: id.into(),
            timed_out: false,
            branch_id: String::new(),
            elapsed_ms: None,
        }
    }

    #[test]
    fn streaming_deltas_and_final_output_share_one_entry() {
        let mut t = EntryTracker::new();
        let a = t.observe(&delta("s", "b", "he")).unwrap();
        let b = t.observe(&delta("s", "b", "llo")).unwrap();
        assert_eq!(a, b, "同一段流式输出必须共享条目 id");
        let fin = t.observe(&output("s", "b", "hello")).unwrap();
        assert_eq!(
            fin, a,
            "终值必须落在同一条目上（Panel 据此收敛而非新开一行）"
        );
        // 终值后封口：下一段正文是新条目
        let next = t.observe(&delta("s", "b", "second")).unwrap();
        assert_ne!(next, a);
    }

    #[test]
    fn non_streaming_output_gets_its_own_entry() {
        let mut t = EntryTracker::new();
        let a = t.observe(&output("s", "b", "no deltas")).unwrap();
        let b = t.observe(&output("s", "b", "another")).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn tool_call_and_result_share_entry_regardless_of_order() {
        let mut t = EntryTracker::new();
        let call = t.observe(&tool_call("s", "call-1")).unwrap();
        let result = t.observe(&tool_result("s", "call-1", "ok")).unwrap();
        assert_eq!(call, result);
        assert_eq!(call, "tool:s:call-1");
        // 乱序：结果先到，id 仍然一致（id 由 tool_call_id 决定）
        let mut t2 = EntryTracker::new();
        let r = t2.observe(&tool_result("s", "call-9", "ok")).unwrap();
        let c = t2.observe(&tool_call("s", "call-9")).unwrap();
        assert_eq!(r, c);
    }

    #[test]
    fn empty_tool_call_id_has_no_entry() {
        let mut t = EntryTracker::new();
        assert!(t.observe(&tool_call("s", "")).is_none());
        assert!(t.observe(&tool_result("s", "", "ok")).is_none());
    }

    #[test]
    fn tool_call_seals_live_streams() {
        let mut t = EntryTracker::new();
        let before = t.observe(&delta("s", "b", "narrating")).unwrap();
        let _ = t.observe(&tool_call("s", "call-2"));
        // 封口后新正文是新条目（工具开始即断定该步叙述不再有终值）
        let after = t.observe(&delta("s", "b", "next step")).unwrap();
        assert_ne!(after, before);
    }

    #[test]
    fn tool_call_seals_only_its_own_session() {
        let mut t = EntryTracker::new();
        let other = t.observe(&delta("s2", "b", "x")).unwrap();
        let mine = t.observe(&delta("s1", "b", "y")).unwrap();
        let _ = t.observe(&tool_call("s1", "call-3"));
        // 另一会话未受影响：同一段继续沿用原条目
        let other_again = t.observe(&delta("s2", "b", "x2")).unwrap();
        assert_eq!(other_again, other);
        // 本会话已封口
        let mine_again = t.observe(&delta("s1", "b", "y2")).unwrap();
        assert_ne!(mine_again, mine);
    }

    #[test]
    fn reasoning_and_backend_are_independent() {
        let mut t = EntryTracker::new();
        let body = t.observe(&delta("s", "b", "answer")).unwrap();
        let think = t.observe(&reasoning_delta("s", "b")).unwrap();
        assert_ne!(body, think);
        assert!(think.starts_with("reasoning:"));
        assert!(body.starts_with("backend:"));
        // 推理终值封口推理条目不影���正文条目
        let think_fin = t
            .observe(&BackendEvent::AgentReasoning {
                session_id: "s".into(),
                branch_id: "b".into(),
                content: "think".into(),
                team_id: None,
            })
            .unwrap();
        assert_eq!(think_fin, think);
        let body_fin = t.observe(&output("s", "b", "answer")).unwrap();
        assert_eq!(body_fin, body);
    }

    #[test]
    fn branches_and_sessions_are_isolated() {
        let mut t = EntryTracker::new();
        let a = t.observe(&delta("s1", "b1", "x")).unwrap();
        let b = t.observe(&delta("s1", "b2", "x")).unwrap();
        let c = t.observe(&delta("s2", "b1", "x")).unwrap();
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn live_state_does_not_grow_with_turn_count() {
        let mut t = EntryTracker::new();
        for i in 0..500 {
            let _ = t.observe(&delta("s", "b", "x"));
            let _ = t.observe(&output("s", "b", &format!("ans{i}")));
        }
        // 封口后 live 表为空（无按会话累积的计数器表 → 无界状态不存在）
        assert_eq!(t.backend.len(), 0);
        assert_eq!(t.reasoning.len(), 0);
    }

    #[test]
    fn output_without_branch_uses_latest_backend_entry() {
        let mut t = EntryTracker::new();
        let streamed = t.observe(&delta("s", "b1", "he")).unwrap();
        // 后端路径的终值不带 branch_id → 落在最新那段正文上
        let fin = t
            .observe(&BackendEvent::AgentOutput {
                session_id: "s".into(),
                branch_id: None,
                content: "hello".into(),
                team_id: None,
            })
            .unwrap();
        assert_eq!(fin, streamed);
        // 该会话从未有过正文条目 → 无身份可给（不伪造）
        let mut fresh = EntryTracker::new();
        assert!(fresh
            .observe(&BackendEvent::AgentOutput {
                session_id: "s".into(),
                branch_id: None,
                content: "x".into(),
                team_id: None,
            })
            .is_none());
    }

    #[test]
    fn unrelated_events_have_no_entry() {
        let mut t = EntryTracker::new();
        assert!(t
            .observe(&BackendEvent::AgentThinking {
                session_id: "s".into(),
                team_id: None,
            })
            .is_none());
    }
}
