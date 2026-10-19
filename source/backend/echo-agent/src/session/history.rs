//! 按会话派生**显示历史**（事件日志 → 显示时间线形状）。
//!
//! 2026-10 面板「会话无消息」修复：显示时间线（`TrunkStore.timeline`）是
//! 每人格一条、跨全部会话共享、上限 `TRUNK_TIMELINE_MAX`（1024 条）的
//! FIFO 窗口——长期无活动的会话条目会被后续会话的活动整体挤出，面板
//! 切回该会话时只能显示「暂无消息」。事件日志（唯一事实来源）按会话分区
//! 且完整保留，因此显示历史改为**按需从事件日志派生**（镜像
//! [`TimelineProjector`](crate::timeline::TimelineProjector) 的展示形状；
//! 实时投影负责窗口，派生负责回填旧会话）。
//!
//! 已知差异（接受）：
//! - 归档提示行不在事件日志中 → 派生不含；
//! - `ToolResult` 事件没有 `timed_out` 字段 → 派生条目的 `failed` 只按
//!   `"error:"` 前缀判定（实时投影还有超时维度）。
//!
//! ## 推理现在**会**派生（2026-10 根治）
//!
//! 此前推理（thinking）从不落事件日志——活链路的 `AssistantMessage` 恒为
//! `reasoning_content: None`（`agent/inbound.rs::record_assistant_reply`
//! 只落最终回答），于是派生结果里没有推理，前端只能靠实时 timeline 窗口
//! 补，导致「刷新后思考全堆到正文后面」的时序错乱无法根除。
//!
//! 现在每段推理以 [`SessionEvent::Reasoning`] 落日志（模型不可见，投影跳过），
//! 派生按到达顺序给出 `kind = "reasoning"` 条目，与工具调用真实交错。
//!
//! ## ⚠️ 水位线：仍排除推理（保守）
//!
//! 前端用 `SessionHistory.max_seq` 作合并水位线（`seq <= baseSeq` 视为已被
//! 快照覆盖）。历史派生**曾**不含推理，把它计入水位线会让前端把实时推理
//! 判成"快照已有"而整条丢弃（「看不到思考 chip」，2026-10 事故）。
//!
//! 现在派生已包含推理，理论上可以计入；但**旧日志**（本次改动之前）里没有
//! `Reasoning` 事件，计入后那些会话仍会踩同一个坑。所以 [`history_watermark`]
//! 继续排除推理，与前端"推理永不按 seq 过滤"构成双保险——两处判据刻意
//! 不依赖彼此的细节。新增"派生不含"的 kind 时必须同步排除。

use std::collections::HashMap;

use crate::event::{TimelineMessage, TimelineSource, TimelineTool};
use crate::timeline::{summarize_timeline_value, timeline_timer_task, truncate_one_line};

use super::TrunkStore;

impl TrunkStore {
    /// 从事件日志为单个会话派生显示历史（镜像 `TimelineProjector` 的展示
    /// 形状；最近 `limit` 条，时间升序）。已知差异（接受）：推理条目与
    /// 归档提示行不在事件日志中，派生不含；ToolResult 无 timed_out 字段，
    /// failed 以 "error:" 前缀判定。
    /// 返回 (messages, total, truncated)。
    pub fn display_history_for_session(
        &self,
        session_id: &str,
        limit: usize,
    ) -> (Vec<TimelineMessage>, usize, bool) {
        let mut messages: Vec<TimelineMessage> = Vec::new();
        // 已发出、等待结果的工具条目：tool_call_id → 条目索引。结果到达时
        // 就地补丁（与实时投影同一配对语义）；id 为空的旧版调用不入索引。
        let mut open: HashMap<String, usize> = HashMap::new();
        // carry-forward 时间游标：backend/compaction 条目自身不带时间，
        // 沿用最近一条带时间事件的时间（与实时投影的"当前时间"同义）。
        let mut last_time: i64 = 0;
        for event in self.event_log() {
            if event.session() != Some(session_id) {
                continue;
            }
            match event {
                echo_session::SessionEvent::UserMessage(user) => {
                    match user
                        .source
                        .as_ref()
                        .map(|source| source.instance_name.as_str())
                    {
                        // 内部父上下文 hook，不是会话内容（与实时投影一致）。
                        Some("background") => continue,
                        Some("timer") => {
                            let task = timeline_timer_task(&user.content);
                            let content = match task {
                                Some(task) => {
                                    format!("定时任务触发 · {}", truncate_one_line(&task, 120))
                                }
                                None => "定时任务触发".into(),
                            };
                            messages.push(TimelineMessage::system(
                                content,
                                session_id,
                                user.timestamp,
                            ));
                        }
                        _ => {
                            // 图片规范化（与实时投影一致）：媒体引用（`/media/<id>`）
                            // 原样保留，遗留的内嵌 data URI 落盘后改写为引用。
                            let images: Vec<String> = user
                                .images
                                .iter()
                                .map(|image| echo_defs::media_store::spill_or_keep(image))
                                .collect();
                            let source = user.source.as_ref().map(|source| TimelineSource {
                                instance_name: source.instance_name.clone(),
                                platform: source.platform.clone(),
                                user_id: source.user_id.clone(),
                                user_name: source.user_name.clone(),
                                channel: source.channel.clone(),
                                group_name: source.group_name.clone(),
                                received_at_ms: source.received_at_ms,
                                message_sequence: source.message_sequence,
                            });
                            messages.push(TimelineMessage::user(
                                user.content.clone(),
                                session_id,
                                user.timestamp,
                                source,
                                images,
                            ));
                        }
                    }
                    // 两类用户事件都带真实时间戳：推进 carry-forward 游标
                    // （timer 的系统条目同样算"最近已知时间"）。
                    last_time = last_time.max(user.timestamp);
                }
                echo_session::SessionEvent::AssistantMessage(assistant) => {
                    // 空正文（纯 tool-call 轮）不落显示条目；tool 条目由
                    // ToolCall 事件产生，这里不再展开 `tool_calls` 以免重复。
                    if assistant.content.trim().is_empty() {
                        continue;
                    }
                    // 推理不落日志：`reasoning_content` 不派生（见模块注释）。
                    messages.push(TimelineMessage::backend(
                        assistant.content.clone(),
                        session_id,
                        last_time,
                        None,
                    ));
                }
                echo_session::SessionEvent::Reasoning(reasoning) => {
                    // 推理落独立条目（与实时投影同形状）：按到达顺序与工具
                    // 调用真实交错。时间用事件自带的产出时刻；旧数据缺失时
                    // 退回 carry-forward（与 backend/compaction 条目同规则）。
                    let content = reasoning.content.trim();
                    if content.is_empty() {
                        continue;
                    }
                    let time = if reasoning.timestamp > 0 {
                        last_time = last_time.max(reasoning.timestamp);
                        reasoning.timestamp
                    } else {
                        last_time
                    };
                    messages.push(TimelineMessage {
                        kind: "reasoning".into(),
                        content: content.to_string(),
                        session_id: session_id.to_string(),
                        time,
                        source: None,
                        reasoning: None,
                        tool: None,
                        images: None,
                        seq: 0,
                    });
                }
                echo_session::SessionEvent::ToolCall(call) => {
                    let started_secs = call
                        .started_at_ms
                        .map(|ms| ms / 1000)
                        .filter(|secs| *secs > 0);
                    let time = started_secs.unwrap_or(last_time);
                    if let Some(secs) = started_secs {
                        last_time = last_time.max(secs);
                    }
                    messages.push(TimelineMessage::tool(
                        call.name.clone(),
                        session_id,
                        time,
                        TimelineTool {
                            name: call.name.clone(),
                            input: summarize_timeline_value(&call.arguments, 160),
                            output: None,
                            failed: false,
                            tool_call_id: call.id.clone(),
                            timed_out: false,
                            started_at_ms: call.started_at_ms,
                            elapsed_ms: None,
                        },
                    ));
                    if !call.id.is_empty() {
                        open.insert(call.id, messages.len() - 1);
                    }
                }
                echo_session::SessionEvent::ToolResult(result) => {
                    let output = summarize_timeline_value(&result.result, 200);
                    let failed = result.result.trim_start().starts_with("error:");
                    match open.remove(&result.tool_call_id) {
                        Some(index) => {
                            // 命中未完成的工具条目：就地补丁（镜像实时投影的
                            // update_timeline_tool）。`timed_out` 无日志依据，
                            // 保持 false。
                            if let Some(tool) = messages
                                .get_mut(index)
                                .and_then(|message| message.tool.as_mut())
                            {
                                tool.output = Some(output);
                                tool.failed = failed;
                                tool.elapsed_ms = result.elapsed_ms;
                            }
                        }
                        None => {
                            // 未命中（旧版日志缺 ToolCall 事件 / 重复结果）：
                            // 追加已完成条目，镜像 update_timeline_tool 的
                            // fallback。ToolResult 事件不带工具名——以 id 兜底
                            // （纯空 id 的旧版事件用占位名）。
                            let name = if result.tool_call_id.is_empty() {
                                "tool".to_string()
                            } else {
                                result.tool_call_id.clone()
                            };
                            messages.push(TimelineMessage::tool(
                                name.clone(),
                                session_id,
                                last_time,
                                TimelineTool {
                                    name,
                                    input: String::new(),
                                    output: Some(output),
                                    failed,
                                    tool_call_id: result.tool_call_id.clone(),
                                    timed_out: false,
                                    started_at_ms: None,
                                    elapsed_ms: result.elapsed_ms,
                                },
                            ));
                        }
                    }
                }
                // 模型往返观测（2026-10-19）：审计用事实，不是对话内容——
                // 显示时间线不为它造条目（与实时投影保持一致：实时也不发）。
                echo_session::SessionEvent::ModelStep(_) => {}
                echo_session::SessionEvent::Compaction(compaction) => {
                    messages.push(TimelineMessage::system(
                        format!("上下文已压缩 · 替换 {} 条事件", compaction.replaced_count),
                        session_id,
                        last_time,
                    ));
                }
            }
        }
        let total = messages.len();
        let truncated = total > limit;
        if truncated {
            // 取**最后** limit 条（最近的活动），保持时间升序。
            messages.drain(..total - limit);
        }
        (messages, total, truncated)
    }
}

/// 会话历史合并水位线：`timeline` 中该会话、**派生会覆盖的 kind** 的最大 seq。
///
/// 前端拿它过滤实时条目（`seq <= 水位线` = 快照已覆盖）。
///
/// 推理**仍排除**（见模块注释）：派生现已包含推理，但旧日志里没有
/// `Reasoning` 事件；计入后那些会话的实时推理会被误判为已覆盖而整条消失。
/// 前端另有"推理永不按 seq 过滤"的独立防线，两处刻意互不依赖。
pub fn history_watermark(
    timeline: impl IntoIterator<Item = TimelineMessage>,
    session_id: &str,
) -> u64 {
    timeline
        .into_iter()
        .filter(|message| message.session_id == session_id)
        .filter(|message| message.kind != "reasoning")
        .map(|message| message.seq)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trunk() -> TrunkStore {
        TrunkStore::new(100_000)
    }

    fn user_event(session: &str, content: &str, timestamp: i64) -> echo_session::SessionEvent {
        echo_session::SessionEvent::UserMessage(echo_session::UserMessage {
            content: content.into(),
            timestamp,
            message_sequence: None,
            source: None,
            images: Vec::new(),
            session: Some(session.into()),
        })
    }

    fn user_event_with_source(
        session: &str,
        content: &str,
        timestamp: i64,
        instance_name: &str,
    ) -> echo_session::SessionEvent {
        echo_session::SessionEvent::UserMessage(echo_session::UserMessage {
            content: content.into(),
            timestamp,
            message_sequence: None,
            source: Some(echo_session::event::MessageSource {
                instance_name: instance_name.into(),
                platform: "core".into(),
                user_id: String::new(),
                user_name: String::new(),
                channel: "direct".into(),
                group_name: None,
                received_at_ms: 0,
                message_sequence: 0,
            }),
            images: Vec::new(),
            session: Some(session.into()),
        })
    }

    /// 单会话派生：user → tool（就地补丁结果）→ backend；旁观会话被过滤。
    #[test]
    fn derives_user_tool_and_backend_entries_for_one_session() {
        let store = trunk();
        let target = "qq:dm::123";
        let watcher = "qq:group:9:456";
        store.append_event(user_event(target, "你好", 1_700_000_000));
        store.append_event(echo_session::SessionEvent::ToolCall(
            echo_session::ToolCallEvent {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
                session: Some(target.into()),
                started_at_ms: Some(1_700_000_000_500),
            },
        ));
        store.append_event(echo_session::SessionEvent::ToolResult(
            echo_session::ToolResultEvent {
                tool_call_id: "call-1".into(),
                result: "ok output".into(),
                images: Vec::new(),
                session: Some(target.into()),
                elapsed_ms: Some(12),
            },
        ));
        store.append_event(echo_session::SessionEvent::AssistantMessage(
            echo_session::AssistantMessage {
                content: "最终回复".into(),
                reasoning_content: Some("推理不派生".into()),
                tool_calls: Vec::new(),
                session: Some(target.into()),
            },
        ));
        store.append_event(user_event(watcher, "旁观会话", 1_700_000_001));

        let (messages, total, truncated) = store.display_history_for_session(target, 500);
        assert_eq!(total, 3, "only the target session's entries: {messages:?}");
        assert!(!truncated);
        assert_eq!(messages.len(), 3);

        assert_eq!(messages[0].kind, "user");
        assert_eq!(messages[0].content, "你好");
        assert_eq!(messages[0].session_id, target);
        assert!(messages[0].source.is_none());

        assert_eq!(messages[1].kind, "tool");
        let tool = messages[1].tool.as_ref().expect("tool payload");
        assert_eq!(tool.name, "bash");
        assert_eq!(tool.tool_call_id, "call-1");
        assert_eq!(tool.started_at_ms, Some(1_700_000_000_500));
        assert!(
            tool.output
                .as_deref()
                .is_some_and(|o| o.contains("ok output")),
            "patched output: {:?}",
            tool.output
        );
        assert!(!tool.failed);
        assert_eq!(tool.elapsed_ms, Some(12));
        assert!(tool.input.contains("command"));

        assert_eq!(messages[2].kind, "backend");
        assert_eq!(messages[2].content, "最终回复");
        assert!(
            messages[2].time >= 1_700_000_000,
            "backend carries the last known time: {}",
            messages[2].time
        );
        assert!(
            messages.iter().all(|m| m.session_id == target),
            "watcher session must be filtered out"
        );
    }

    /// limit 截断：取最后 limit 条，total 报告截断前总数。
    #[test]
    fn truncates_to_the_most_recent_entries() {
        let store = trunk();
        let target = "local:tui::local_user";
        for index in 0..10 {
            let content = format!("消息 {index}");
            store.append_event(user_event(target, &content, 1_700_000_000 + index as i64));
        }
        let (messages, total, truncated) = store.display_history_for_session(target, 4);
        assert_eq!(total, 10);
        assert!(truncated);
        assert_eq!(messages.len(), 4);
        let contents: Vec<&str> = messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, ["消息 6", "消息 7", "消息 8", "消息 9"]);
    }

    /// timer 事件 → system 条目；background hook 跳过。
    #[test]
    fn timer_becomes_system_entry_and_background_is_skipped() {
        let store = trunk();
        let target = "local:tui::local_user";
        let timer_content = format!(
            "{}{}{}",
            crate::input_marker::TIMER_EVENT_OPEN,
            r#"{"task":"x"}"#,
            crate::input_marker::TIMER_EVENT_CLOSE
        );
        store.append_event(user_event_with_source(
            target,
            &timer_content,
            1_700_000_000,
            "timer",
        ));
        store.append_event(user_event_with_source(
            target,
            "<backend_message_hook>内部父上下文</backend_message_hook>",
            1_700_000_001,
            "background",
        ));

        let (messages, total, truncated) = store.display_history_for_session(target, 500);
        assert_eq!(total, 1, "background hook must not produce an entry");
        assert!(!truncated);
        assert_eq!(messages[0].kind, "system");
        assert!(
            messages[0].content.starts_with("定时任务触发"),
            "{}",
            messages[0].content
        );
        assert!(messages[0].content.contains('x'), "{}", messages[0].content);
        assert_eq!(messages[0].time, 1_700_000_000);
    }

    /// 压缩事件 → system 条目（含替换条数）。
    #[test]
    fn compaction_renders_a_system_entry() {
        let store = trunk();
        let target = "local:tui::local_user";
        store.append_event(echo_session::SessionEvent::Compaction(
            echo_session::CompactionEvent {
                replaced_count: 5,
                summary: "摘要体".into(),
                archive: None,
                session: Some(target.into()),
            },
        ));
        let (messages, total, truncated) = store.display_history_for_session(target, 500);
        assert_eq!(total, 1);
        assert!(!truncated);
        assert_eq!(messages[0].kind, "system");
        assert!(
            messages[0].content.contains("替换 5"),
            "{}",
            messages[0].content
        );
    }
    // ── 合并水位线（2026-10「看不到思考 chip」回归）──────────────────────

    fn entry(kind: &str, session: &str, seq: u64) -> TimelineMessage {
        TimelineMessage {
            kind: kind.into(),
            content: format!("{kind}-{seq}"),
            session_id: session.into(),
            time: 0,
            source: None,
            reasoning: None,
            tool: None,
            images: None,
            seq,
        }
    }

    #[test]
    fn watermark_excludes_reasoning() {
        // 派生不含推理 → 水位线不得把推理的 seq 算进去，否则前端会把
        // 实时推理判成"快照已覆盖"而整条丢弃。
        let timeline = vec![
            entry("user", "s1", 10),
            entry("reasoning", "s1", 95), // 最大 seq，但派生不含
            entry("backend", "s1", 40),
            entry("tool", "s1", 60),
        ];
        assert_eq!(
            super::history_watermark(timeline.clone(), "s1"),
            60,
            "水位线应停在派生能覆盖的最大 seq（tool 60），不含 reasoning 95"
        );
    }

    #[test]
    fn watermark_filters_by_session() {
        let timeline = vec![entry("backend", "s1", 30), entry("backend", "s2", 900)];
        assert_eq!(super::history_watermark(timeline, "s1"), 30);
    }

    #[test]
    fn watermark_is_zero_without_derivable_entries() {
        // 只有推理（例如刷新后 timeline 里仅剩推理）→ 0，前端据此不过滤。
        let timeline = vec![entry("reasoning", "s1", 500)];
        assert_eq!(super::history_watermark(timeline, "s1"), 0);
    }

    #[test]
    fn watermark_handles_empty_input() {
        assert_eq!(super::history_watermark(Vec::new(), "s1"), 0);
    }

    // ── 推理派生（2026-10 根治：推理进事件日志）────────────────────────

    fn reasoning_event(session: &str, content: &str, timestamp: i64) -> echo_session::SessionEvent {
        echo_session::SessionEvent::Reasoning(echo_session::ReasoningEvent {
            content: content.into(),
            timestamp,
            session: Some(session.into()),
        })
    }

    #[test]
    fn reasoning_events_derive_interleaved_with_tools() {
        let store = trunk();
        let target = "local:tui::local_user";
        store.append_event(user_event(target, "问题", 100));
        store.append_event(reasoning_event(target, "第一段思考", 105));
        store.append_event(echo_session::SessionEvent::ToolCall(
            echo_session::ToolCallEvent {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
                session: Some(target.into()),
                started_at_ms: Some(110_000),
            },
        ));
        store.append_event(echo_session::SessionEvent::ToolResult(
            echo_session::ToolResultEvent {
                tool_call_id: "c1".into(),
                result: "ok".into(),
                images: Vec::new(),
                session: Some(target.into()),
                elapsed_ms: Some(5),
            },
        ));
        store.append_event(reasoning_event(target, "第二段思考", 120));
        store.append_event(echo_session::SessionEvent::AssistantMessage(
            echo_session::AssistantMessage {
                content: "回答".into(),
                reasoning_content: None,
                tool_calls: Vec::new(),
                session: Some(target.into()),
            },
        ));

        let (messages, total, _) = store.display_history_for_session(target, 500);
        let kinds: Vec<&str> = messages.iter().map(|m| m.kind.as_str()).collect();
        // 5 条：ToolResult 就地补丁 ToolCall 条目（不新增），其余各一条。
        assert_eq!(total, 5);
        assert_eq!(
            kinds,
            vec!["user", "reasoning", "tool", "reasoning", "backend"],
            "推理必须与工具调用按到达顺序真实交错：{messages:?}"
        );
        // 推理自带时间戳（不是 carry-forward 的猜测值）
        assert_eq!(messages[1].time, 105);
        assert_eq!(messages[1].content, "第一段思考");
        assert_eq!(messages[3].time, 120);
    }

    #[test]
    fn reasoning_without_timestamp_falls_back_to_carry_forward() {
        let store = trunk();
        let target = "local:tui::local_user";
        store.append_event(user_event(target, "问题", 200));
        store.append_event(reasoning_event(target, "旧数据没有时间", 0));
        let (messages, _, _) = store.display_history_for_session(target, 500);
        assert_eq!(messages[1].kind, "reasoning");
        assert_eq!(messages[1].time, 200, "缺失时间沿用最近已知时间");
    }

    #[test]
    fn blank_reasoning_is_skipped() {
        let store = trunk();
        let target = "local:tui::local_user";
        store.append_event(user_event(target, "问题", 10));
        store.append_event(reasoning_event(target, "   ", 11));
        let (messages, total, _) = store.display_history_for_session(target, 500);
        assert_eq!(total, 1, "空白推理不落显示条目：{messages:?}");
    }

    #[test]
    fn reasoning_from_other_session_is_not_derived() {
        let store = trunk();
        store.append_event(user_event("s1", "问题", 10));
        store.append_event(reasoning_event("s2", "别的会话的思考", 11));
        let (messages, total, _) = store.display_history_for_session("s1", 500);
        assert_eq!(total, 1, "{messages:?}");
    }
}
