//! 跨机子代理结果聚合（P3-3）：大脑侧按父 turn 分组追踪派生的全部远程
//! 子代理，组内全部终态后产出一条聚合摘要（各节点结果摘要 + 成败统计）。
//!
//! 缺口背景：并行 `spawn_subagent node=A + node=B` 时，主 agent 只逐条
//! 收到 `<subagent_event>`，没有"全部完成"的统一汇报。本模块在大脑侧
//! 维护 `父 turn（session_id + parent_branch_id）→ 挂起 call_id 集合`
//! 的分组表：
//!
//! - **登记**：`SpawnSubagentTool::spawn` 受理远程委派（`node=Some`）时
//!   调用 [`register_remote_subagent`]；
//! - **销账**：终态 `SubagentEvent` 到达时调用 [`settle_remote_subagent`]
//!   （组合根两处：大脑侧本地完成经 notifier 出口前、联邦泵收到对端
//!   `SubagentEvent` 帧——后者服务执行端真正执行并回报的场景）；
//! - **聚合**：组内最后一员销账时返回 [`AggregateSummary`]，由组合根
//!   投递到父会话 timeline（assistant 系统消息）。
//!
//! 聚合只覆盖**远程**子代理；本机子代理的逐条 hook 回灌语义不变。

use std::sync::Arc;

use dashmap::DashMap;

use echo_federation::SubagentStatus;

/// 聚合摘要中单个成员结果的字符上限（防单个长结果烧掉聚合消息）。
const MEMBER_RESULT_MAX_CHARS: usize = 500;

/// 聚合摘要中任务描述的字符上限（单行摘要）。
const MEMBER_TASK_MAX_CHARS: usize = 80;

/// 组内一名成员（一次远程委派）。
struct Member {
    call_id: String,
    peer: String,
    task: String,
    /// 终态（Completed/Failed/Cancelled）+ 结果文本；None = 仍在飞。
    terminal: Option<(SubagentStatus, Option<String>)>,
}

/// 同一父 turn 派生的远程子代理组。
struct Group {
    session_id: String,
    members: Vec<Member>,
}

/// 组内全部终态后产出的聚合摘要（组合根投递到父会话 timeline）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateSummary {
    /// 父会话 id（投递目标）。
    pub session_id: String,
    /// 分组键（`{session_id}::{parent_branch_id}`），观测/日志用。
    pub group_key: String,
    /// 汇总文本（各 peer 状态 + 结果前 500 字 + 成功 x/y）。
    pub text: String,
}

/// 跨机子代理聚合器（进程级一份，见 [`remote_aggregator`]）。
pub struct RemoteSubagentAggregator {
    /// 分组键 → 组。
    groups: DashMap<String, Group>,
    /// call_id → 分组键（销账寻址）。
    by_call: DashMap<String, String>,
}

impl RemoteSubagentAggregator {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            groups: DashMap::new(),
            by_call: DashMap::new(),
        })
    }

    /// 分组键：父 turn = 父会话 + 父分支（branch id 每 turn 唯一）。
    fn group_key(session_id: &str, parent_branch_id: &str) -> String {
        format!("{session_id}::{parent_branch_id}")
    }

    /// 登记一次远程委派（spawn_subagent node=<peer> 受理时调用）。
    pub fn register(
        &self,
        session_id: &str,
        parent_branch_id: &str,
        call_id: &str,
        peer: &str,
        task: &str,
    ) {
        let key = Self::group_key(session_id, parent_branch_id);
        self.by_call.insert(call_id.to_string(), key.clone());
        let mut group = self.groups.entry(key).or_insert_with(|| Group {
            session_id: session_id.to_string(),
            members: Vec::new(),
        });
        group.members.push(Member {
            call_id: call_id.to_string(),
            peer: peer.to_string(),
            task: task.to_string(),
            terminal: None,
        });
    }

    /// 终态销账：标记该 call_id 终态；若其所在组全部终态，移除该组并
    /// 返回聚合摘要（恰好一次——组已移除，重复销账返回 None）。
    /// 未知 call_id（本机子代理、非聚合场景）返回 None。
    pub fn settle(
        &self,
        call_id: &str,
        status: SubagentStatus,
        result: Option<String>,
    ) -> Option<AggregateSummary> {
        // Running 等非终态不销账（SubagentSpawn 观测帧也可能走到这里）。
        if matches!(status, SubagentStatus::Running) {
            return None;
        }
        let key = self.by_call.get(call_id).map(|k| k.clone())?;
        let all_terminal = {
            let mut group = self.groups.get_mut(&key)?;
            let member = group.members.iter_mut().find(|m| m.call_id == call_id)?;
            // 幂等：已终态的成员不重复记账（重复帧不触发二次聚合——
            // 组在全部终态时已移除，此处只是防"部分终态时重复销账"
            // 把统计搞乱）。
            if member.terminal.is_some() {
                return None;
            }
            member.terminal = Some((status, result));
            group.members.iter().all(|m| m.terminal.is_some())
        };
        if !all_terminal {
            return None;
        }
        let (_, group) = self.groups.remove(&key)?;
        for member in &group.members {
            self.by_call.remove(&member.call_id);
        }
        Some(Self::summarize(&key, group))
    }

    /// 汇总文本：逐成员 `peer 状态 + 任务/结果摘要`，末尾成功 x/y。
    fn summarize(key: &str, group: Group) -> AggregateSummary {
        let total = group.members.len();
        let succeeded = group
            .members
            .iter()
            .filter(|m| matches!(m.terminal, Some((SubagentStatus::Completed, _))))
            .count();
        let mut lines = vec![format!(
            "🛰 跨机子代理全部完成（成功 {succeeded}/{total}，{total} 个远程任务）"
        )];
        for member in &group.members {
            let (status, result) = member
                .terminal
                .as_ref()
                .expect("all members terminal at summarize");
            let (icon, label) = match status {
                SubagentStatus::Completed => ("✅", "完成"),
                SubagentStatus::Failed => ("❌", "失败"),
                SubagentStatus::Cancelled => ("⛔", "已取消"),
                SubagentStatus::Running => unreachable!("terminal only"),
            };
            lines.push(format!(
                "- {icon} [{}] {}：{}",
                member.peer,
                label,
                crate::llm::truncate(&member.task, MEMBER_TASK_MAX_CHARS),
            ));
            if let Some(text) = result {
                let text = text.trim();
                if !text.is_empty() {
                    lines.push(format!(
                        "  结果：{}",
                        crate::llm::truncate(text, MEMBER_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
        AggregateSummary {
            session_id: group.session_id,
            group_key: key.to_string(),
            text: lines.join("\n"),
        }
    }

    /// 观测：当前挂起的组数（测试用）。
    #[cfg(test)]
    fn pending_groups(&self) -> usize {
        self.groups.len()
    }
}

// ── 进程级聚合器（与 REMOTE_SUBAGENT_NOTIFIER 同模式）──
//
// 登记点（subagent 包）与销账/投递点（core 组合根）分属不同 crate 层，
// 经懒初始化的全局句柄解耦；联邦关闭时登记仍发生，但远程 spawn 本身
// 就依赖联邦（无代理工具时子任务立即失败 → 终态销账 → 聚合照常闭合）。

static AGGREGATOR: std::sync::OnceLock<Arc<RemoteSubagentAggregator>> =
    std::sync::OnceLock::new();

/// 进程级聚合器句柄（首次调用惰性创建）。
pub fn remote_aggregator() -> Arc<RemoteSubagentAggregator> {
    AGGREGATOR
        .get_or_init(RemoteSubagentAggregator::new)
        .clone()
}

/// 登记远程委派（`SpawnSubagentTool::spawn` 受理 `node=Some` 时调用）。
pub fn register_remote_subagent(
    session_id: &str,
    parent_branch_id: &str,
    call_id: &str,
    peer: &str,
    task: &str,
) {
    remote_aggregator().register(session_id, parent_branch_id, call_id, peer, task);
}

/// 终态销账（组合根：notifier 出口 / 联邦泵 SubagentEvent 分支调用）；
/// 返回 Some 时由调用方把摘要投递到父会话 timeline。
pub fn settle_remote_subagent(
    call_id: &str,
    status: SubagentStatus,
    result: Option<String>,
) -> Option<AggregateSummary> {
    remote_aggregator().settle(call_id, status, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P3-3 验收场景：两个并行远程子代理（A 成功 B 失败），第二个终态
    /// 到达后聚合消息发出，内容含两节点摘要与成败统计。
    #[test]
    fn aggregates_when_all_members_terminal() {
        let agg = RemoteSubagentAggregator::new();
        agg.register("sess-1", "turn-1", "call-a", "node-a", "在 A 上检索日志");
        agg.register("sess-1", "turn-1", "call-b", "node-b", "在 B 上跑基准");
        assert_eq!(agg.pending_groups(), 1);

        // 第一个终态（成功）：不聚合。
        assert!(agg
            .settle(
                "call-a",
                SubagentStatus::Completed,
                Some("A 节点结论：日志无异常".into())
            )
            .is_none());
        assert_eq!(agg.pending_groups(), 1);

        // 第二个终态（失败）：聚合发出。
        let summary = agg
            .settle(
                "call-b",
                SubagentStatus::Failed,
                Some("B 节点报错：磁盘满".into()),
            )
            .expect("all terminal → aggregate");
        assert_eq!(summary.session_id, "sess-1");
        assert!(summary.text.contains("成功 1/2"), "统计: {}", summary.text);
        assert!(summary.text.contains("node-a"), "A 摘要: {}", summary.text);
        assert!(summary.text.contains("node-b"), "B 摘要: {}", summary.text);
        assert!(
            summary.text.contains("A 节点结论：日志无异常"),
            "A 结果: {}",
            summary.text
        );
        assert!(
            summary.text.contains("B 节点报错：磁盘满"),
            "B 结果: {}",
            summary.text
        );
        assert!(summary.text.contains('✅') && summary.text.contains('❌'));
        // 组已移除：重复销账不再聚合。
        assert!(agg
            .settle("call-b", SubagentStatus::Failed, None)
            .is_none());
        assert_eq!(agg.pending_groups(), 0);
    }

    #[test]
    fn single_member_group_aggregates_immediately() {
        let agg = RemoteSubagentAggregator::new();
        agg.register("s", "t", "c1", "node-x", "任务");
        let summary = agg
            .settle("c1", SubagentStatus::Completed, Some("done".into()))
            .expect("single member");
        assert!(summary.text.contains("成功 1/1"), "{}", summary.text);
    }

    #[test]
    fn unknown_or_running_events_do_not_settle() {
        let agg = RemoteSubagentAggregator::new();
        agg.register("s", "t", "c1", "node-x", "任务");
        // 未知 call_id。
        assert!(agg
            .settle("nope", SubagentStatus::Completed, None)
            .is_none());
        // Running 非终态。
        assert!(agg.settle("c1", SubagentStatus::Running, None).is_none());
        assert_eq!(agg.pending_groups(), 1);
        // 重复终态幂等（第一次销账但未闭合；第二次同成员不重复记账）。
        assert!(agg.settle("c1", SubagentStatus::Cancelled, None).is_some());
        assert!(agg.settle("c1", SubagentStatus::Cancelled, None).is_none());
    }

    #[test]
    fn groups_are_isolated_by_parent_turn() {
        let agg = RemoteSubagentAggregator::new();
        agg.register("s", "turn-1", "c1", "node-a", "任务一");
        agg.register("s", "turn-2", "c2", "node-a", "任务二");
        assert_eq!(agg.pending_groups(), 2);
        // turn-1 闭合不影响 turn-2。
        let s1 = agg
            .settle("c1", SubagentStatus::Completed, Some("r1".into()))
            .expect("turn-1 closes");
        assert!(s1.group_key.ends_with("turn-1"));
        assert!(agg
            .settle("c2", SubagentStatus::Completed, Some("r2".into()))
            .is_some());
    }

    #[test]
    fn long_member_results_are_truncated() {
        let agg = RemoteSubagentAggregator::new();
        agg.register("s", "t", "c1", "node-a", "任务");
        let long = "x".repeat(MEMBER_RESULT_MAX_CHARS * 3);
        let summary = agg
            .settle("c1", SubagentStatus::Completed, Some(long))
            .expect("aggregate");
        assert!(
            summary.text.chars().count() < MEMBER_RESULT_MAX_CHARS + 200,
            "truncated: {} chars",
            summary.text.chars().count()
        );
    }
}
