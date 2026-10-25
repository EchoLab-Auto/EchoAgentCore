//! 按会话派生**显示历史**（事件日志 → 显示时间线形状）。
//!
//! ## 为什么在这里
//!
//! 显示时间线是每人格一条、上限 1024 条的 FIFO 窗口——长期无活动的会话条目
//! 会被后续会话的活动整体挤出，面板切回该会话时只能显示「暂无消息」。事件
//! 日志（唯一事实来源）按会话分区且完整保留，所以显示历史**按需从事件日志
//! 派生**：实时投影（[`super::timeline`]）负责窗口，本模块负责回填旧会话。
//!
//! 两者是**同一件事的两个方向**，共用同一批展示规则（工具概要截断 / 敏感键
//! 隐藏 / 定时信封解析 / 单行折叠），所以同处一个 crate。此前派生住在宿主
//! `session/history.rs`（611 行），只因为它要读 `TrunkStore` 的事件日志——
//! 现在它吃 `&[SessionEvent]`，宿主一行转发。
//!
//! ## 与实时投影的已知差异（接受）
//!
//! - 归档提示行不在事件日志中 → 派生不含；
//! - `ToolResult` 事件没有 `timed_out` 字段 → 派生条目的 `failed` 只按
//!   `"error:"` 前缀判定（实时投影还有超时维度）。
//!
//! ## 推理**会**派生（2026-10 根治）
//!
//! 此前推理从不落事件日志——活链路的 `AssistantMessage` 恒为
//! `reasoning_content: None`，于是派生结果里没有推理，前端只能靠实时窗口
//! 补，导致「刷新后思考全堆到正文后面」的时序错乱无法根除。
//!
//! 现在每段推理以 [`echo_session::SessionEvent::Reasoning`] 落日志（模型不可
//! 见，投影跳过），派生按到达顺序给出 `kind = "reasoning"` 条目，与工具调用
//! 真实交错。
//!
//! ## ⚠️ 水位线：仍排除推理（保守）
//!
//! 前端用 `SessionHistory.max_seq` 作合并水位线（`seq <= baseSeq` 视为已被
//! 快照覆盖）。历史派生**曾**不含推理，把它计入水位线会让前端把实时推理判成
//! "快照已有"而整条丢弃（「看不到思考 chip」，2026-10 事故）。现在虽然派生
//! 含推理，旧日志里却没有 `Reasoning` 事件——保持排除是安全的；前端另有
//! 「推理永不按 seq 过滤」的独立防线，两处刻意互不依赖。

use std::collections::HashMap;

use echo_protocol::{TimelineMessage, TimelineSource, TimelineTool};

use crate::timeline::{summarize_timeline_value, timeline_timer_task, truncate_one_line};

pub fn display_history_for_session(
events: &[echo_session::SessionEvent],
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
    for event in events {
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
                // 时间取**回答自身的落盘时刻**（2026-10 显示顺序根治）。
                //
                // 此前一律用 `last_time`（carry-forward），对 backend 条目那是
                // 错的语义：它拿的是"上一条带时间的事件"的时间，通常就是上一个
                // 工具的 `started_at_ms`，可以比回答真实产出时刻早**数十分钟**。
                // 而实时投影（`timeline.rs`）给同一个回答的是 `Utc::now()`。
                // 两侧时间不等 → 面板的 `(role, time)` 认不出是同一条 → 同一条
                // 回答显示两次、其中一份被插到列表靠后位置。
                //
                // 旧日志没有该字段（`timestamp == 0`）→ 退回 carry-forward，
                // 行为与改前一致（那批老数据由前端的内容兜底负责）。
                let time = if assistant.timestamp > 0 {
                    last_time = last_time.max(assistant.timestamp);
                    assistant.timestamp
                } else {
                    last_time
                };
                // 推理不落日志：`reasoning_content` 不派生（见模块注释）。
                messages.push(TimelineMessage::backend(
                    assistant.content.clone(),
                    session_id,
                    time,
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
                    ord: 0, // 派生条目：非时间线缓冲条目，无需分页锚点
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
                    open.insert(call.id.clone(), messages.len() - 1);
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
                        // 未命中（旧版日志缺 ToolCall 事件 / 压缩吃掉了调用 /
                        // 重复结果）：追加已完成条目，镜像 update_timeline_tool
                        // 的 fallback。
                        //
                        // 名字用**中性占位**（2026-10 修正）：`ToolResult` 事件
                        // 不带工具名，此前拿 `tool_call_id` 顶上，界面上就出现
                        // 一个叫 `tool_N1QCMouhKj5KR7WuskLdDcYa` 的"工具"——
                        // 既不是工具名也不是给人看的字。`id` 仍原样保留在
                        // `tool_call_id` 里供配对，展示层不需要它。
                        let name = "tool".to_string();
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
    use echo_session::{SessionEvent, UserMessage};

    fn user_event(session: &str, content: &str, timestamp: i64) -> SessionEvent {
        let mut event = UserMessage {
            content: content.into(),
            timestamp,
            message_sequence: None,
            source: None,
            images: Vec::new(),
            session: Some(session.into()),
        };
        event.timestamp = timestamp;
        SessionEvent::UserMessage(event)
    }

    /// 派生只取目标会话的事件（其它会话的条目不串进来）。
    #[test]
    fn derivation_is_scoped_to_one_session() {
        let events = vec![
            user_event("a", "甲的消息", 10),
            user_event("b", "乙的消息", 20),
            user_event("a", "甲的第二条", 30),
        ];
        let (messages, total, _) = display_history_for_session(&events, "a", 100);
        assert_eq!(total, 2, "只算 a 会话");
        assert!(messages.iter().all(|m| m.session_id == "a"));
        assert_eq!(messages[0].content, "甲的消息");
        assert_eq!(messages[1].content, "甲的第二条");
    }

    /// `limit` 取**最近**的若干条（时间升序返回，truncated 标记为真）。
    #[test]
    fn limit_keeps_the_newest_and_flags_truncation() {
        let events: Vec<SessionEvent> = (0..10)
            .map(|index| user_event("a", &format!("第{index}条"), index as i64))
            .collect();
        let (messages, total, truncated) = display_history_for_session(&events, "a", 3);
        assert_eq!(total, 10);
        assert!(truncated, "超出 limit 必须标记截断");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].content, "第9条", "最新的必须保留");
    }

    /// 后台输入不派生（与实时投影一致：内部父上下文 hook 不是对话内容）。
    #[test]
    fn background_input_is_not_derived() {
        let mut event = UserMessage {
            content: "internal".into(),
            timestamp: 1,
            message_sequence: None,
            source: Some(echo_session::event::MessageSource {
                instance_name: "background".into(),
                platform: "local".into(),
                user_id: "u".into(),
                user_name: "u".into(),
                channel: "dm".into(),
                group_name: None,
                received_at_ms: 0,
                message_sequence: 1,
            }),
            images: Vec::new(),
            session: Some("a".into()),
        };
        event.timestamp = 1;
        let events = vec![SessionEvent::UserMessage(event)];
        let (messages, _, _) = display_history_for_session(&events, "a", 100);
        assert!(messages.is_empty(), "内部 hook 不该进显示历史");
    }

    /// 水位线排除推理（旧日志没有 Reasoning 事件的保守口径）。
    #[test]
    fn watermark_excludes_reasoning_entries() {
        let messages = vec![
            TimelineMessage {
                kind: "user".into(),
                content: "x".into(),
                session_id: "s".into(),
                time: 0,
                source: None,
                reasoning: None,
                tool: None,
                images: None,
                seq: 5,
                ord: 0,
            },
            TimelineMessage {
                kind: "reasoning".into(),
                content: "想".into(),
                session_id: "s".into(),
                time: 0,
                source: None,
                reasoning: None,
                tool: None,
                images: None,
                seq: 99,
                ord: 1,
            },
        ];
        assert_eq!(history_watermark(messages, "s"), 5, "推理不计入水位线");
    }

    fn user_event_with_source(
        session: &str,
        content: &str,
        timestamp: i64,
        instance_name: &str,
    ) -> SessionEvent {
        SessionEvent::UserMessage(UserMessage {
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

    fn reasoning_event(session: &str, content: &str, timestamp: i64) -> SessionEvent {
        SessionEvent::Reasoning(echo_session::event::ReasoningEvent {
            content: content.into(),
            session: Some(session.into()),
            timestamp,
        })
    }

    fn assistant_event(session: &str, content: &str, timestamp: i64) -> SessionEvent {
        SessionEvent::AssistantMessage(echo_session::event::AssistantMessage {
            timestamp,
            content: content.into(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            session: Some(session.into()),
        })
    }

    /// 回答条目的时间 = **回答自身的落盘时刻**（2026-10 显示顺序根治）。
    ///
    /// 事故形态：派生给 backend 用 carry-forward（= 上一个工具的开始时刻），
    /// 实时投影给的是 `Utc::now()`。同一个回合两侧相差数十分钟，面板拿
    /// `(role, time)` 对齐时认不出是同一条 → 同一条回答显示两次、其中一份被
    /// 插到列表靠后（用户报「会话区显示顺序出了很大的问题」）。
    #[test]
    fn backend_entry_uses_its_own_timestamp_not_carry_forward() {
        let target = "local:workspace:systemx/base:local_user";
        let events = vec![
            user_event(target, "跑一下", 1_700_000_000),
            SessionEvent::ToolCall(echo_session::event::ToolCallEvent {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"sleep 600"}"#.into(),
                session: Some(target.into()),
                // 工具在 t+10 开始，跑了 10 分钟
                started_at_ms: Some(1_700_000_010_000),
            }),
            // 回答在 t+610 落盘（工具开始后 10 分钟）
            assistant_event(target, "跑完了", 1_700_000_610),
        ];
        let (messages, _, _) = display_history_for_session(&events, target, 500);
        let backend = messages
            .iter()
            .find(|m| m.kind == "backend")
            .expect("缺少 backend 条目");
        assert_eq!(
            backend.time, 1_700_000_610,
            "回答必须用自身落盘时刻，不能沿用工具开始时刻（实得 {}）",
            backend.time
        );
    }

    /// 旧日志没有 `timestamp`（=0）：仍按 carry-forward，行为不变（向后兼容）。
    #[test]
    fn backend_entry_without_timestamp_falls_back_to_carry_forward() {
        let target = "s";
        let events = vec![
            user_event(target, "问", 1_700_000_000),
            assistant_event(target, "答", 0),
        ];
        let (messages, _, _) = display_history_for_session(&events, target, 500);
        let backend = messages
            .iter()
            .find(|m| m.kind == "backend")
            .expect("缺少 backend 条目");
        assert_eq!(backend.time, 1_700_000_000, "缺时间戳时沿用最近已知时间");
    }

    /// 孤儿工具结果（压缩吃掉了 ToolCall）：名字用中性占位，**不能**把
    /// `tool_call_id` 当工具名显示（界面上曾出现 `tool_N1QCMouhKj5...`）。
    #[test]
    fn orphan_tool_result_uses_neutral_name_and_keeps_the_id() {
        let target = "s";
        let events = vec![SessionEvent::ToolResult(
            echo_session::event::ToolResultEvent {
                tool_call_id: "tool_N1QCMouhKj5KR7WuskLdDcYa".into(),
                result: "done".into(),
                images: Vec::new(),
                session: Some(target.into()),
                elapsed_ms: Some(3),
            },
        )];
        let (messages, _, _) = display_history_for_session(&events, target, 500);
        let tool = messages[0].tool.as_ref().expect("缺少 tool 载荷");
        assert_eq!(tool.name, "tool", "展示名必须是中性占位");
        assert_eq!(
            tool.tool_call_id, "tool_N1QCMouhKj5KR7WuskLdDcYa",
            "id 仍保留供配对"
        );
    }

    /// 单会话派生：user → tool（就地补丁结果）→ backend；旁观会话被过滤。
    #[test]
    fn derives_user_tool_and_backend_entries_for_one_session() {
        let target = "qq:dm::123";
        let events = vec![
            user_event(target, "你好", 1_700_000_000),
            SessionEvent::ToolCall(echo_session::event::ToolCallEvent {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
                session: Some(target.into()),
                started_at_ms: Some(1_700_000_000_500),
            }),
            SessionEvent::ToolResult(echo_session::event::ToolResultEvent {
                tool_call_id: "call-1".into(),
                result: "ok output".into(),
                images: Vec::new(),
                session: Some(target.into()),
                elapsed_ms: Some(12),
            }),
            SessionEvent::AssistantMessage(echo_session::event::AssistantMessage {
                timestamp: 0,
                content: "最终回复".into(),
                reasoning_content: Some("推理不派生".into()),
                tool_calls: Vec::new(),
                session: Some(target.into()),
            }),
            user_event("qq:group:9:456", "旁观会话", 1_700_000_001),
        ];
        let (messages, total, truncated) = display_history_for_session(&events, target, 500);
        assert_eq!(total, 3, "只取目标会话：{messages:?}");
        assert!(!truncated);
        assert_eq!(messages[0].kind, "user");
        assert_eq!(messages[0].content, "你好");
        assert!(messages[0].source.is_none());
        assert_eq!(messages[1].kind, "tool");
        assert_eq!(
            messages[1].tool.as_ref().and_then(|t| t.output.as_deref()),
            Some("ok output"),
            "工具结果必须就地补丁到调用条目上"
        );
        assert_eq!(messages[2].kind, "backend");
        assert_eq!(messages[2].content, "最终回复");
    }

    /// 定时任务 → system 条目；后台 hook 跳过。
    #[test]
    fn timer_becomes_system_entry_and_background_is_skipped() {
        let events = vec![
            user_event_with_source("s", "plain", 10, "qq"),
            user_event_with_source(
                "s",
                r#"<timer_event>{"task":"提醒喝水"}</timer_event>"#,
                11,
                "timer",
            ),
            user_event_with_source("s", "internal", 12, "background"),
        ];
        let (messages, _, _) = display_history_for_session(&events, "s", 100);
        assert_eq!(messages.len(), 2, "后台 hook 不派生：{messages:?}");
        assert_eq!(messages[0].kind, "user");
        assert_eq!(messages[1].kind, "system");
        assert!(messages[1].content.contains("提醒喝水"), "{}", messages[1].content);
    }

    /// 压缩事件 → system 条目（含替换条数）。
    #[test]
    fn compaction_renders_a_system_entry() {
        let events = vec![SessionEvent::Compaction(echo_session::event::CompactionEvent {
            replaced_count: 5,
            summary: "摘要体".into(),
            archive: None,
            session: Some("s".into()),
        })];
        let (messages, total, _) = display_history_for_session(&events, "s", 500);
        assert_eq!(total, 1);
        assert_eq!(messages[0].kind, "system");
        assert!(messages[0].content.contains("替换 5"), "{}", messages[0].content);
    }

    /// 推理与工具**真实交错**（按到达顺序，不是全堆到末尾）。
    #[test]
    fn reasoning_events_derive_interleaved_with_tools() {
        let events = vec![
            user_event("s", "问", 1),
            reasoning_event("s", "想一", 2),
            SessionEvent::ToolCall(echo_session::event::ToolCallEvent {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
                session: Some("s".into()),
                started_at_ms: Some(3_000),
            }),
            reasoning_event("s", "想二", 4),
        ];
        let (messages, _, _) = display_history_for_session(&events, "s", 100);
        let kinds: Vec<&str> = messages.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(kinds, vec!["user", "reasoning", "tool", "reasoning"]);
    }

    /// 推理缺时间戳时沿用 carry-forward 游标（不是 0）。
    #[test]
    fn reasoning_without_timestamp_falls_back_to_carry_forward() {
        let events = vec![
            user_event("s", "问", 1_700_000_000),
            reasoning_event("s", "想", 0),
        ];
        let (messages, _, _) = display_history_for_session(&events, "s", 100);
        assert_eq!(messages[1].time, 1_700_000_000, "必须沿用最近已知时间");
    }

    /// 空白推理与其它会话的推理都不派生。
    #[test]
    fn blank_and_foreign_reasoning_is_skipped() {
        let events = vec![
            reasoning_event("s", "   ", 1),
            reasoning_event("other", "别人的思考", 2),
        ];
        let (messages, _, _) = display_history_for_session(&events, "s", 100);
        assert!(messages.is_empty(), "{messages:?}");
    }

    /// 水位线按会话过滤。
    #[test]
    fn watermark_filters_by_session() {
        let mut mine = TimelineMessage::backend("x".to_string(), "a".to_string(), 0, None);
        mine.seq = 3;
        let mut theirs = TimelineMessage::backend("y".to_string(), "b".to_string(), 0, None);
        theirs.seq = 9;
        assert_eq!(history_watermark(vec![mine, theirs], "a"), 3);
    }

    /// 空输入 / 无可派生条目 → 水位线 0。
    #[test]
    fn watermark_is_zero_for_empty_input() {
        assert_eq!(history_watermark(vec![], "s"), 0);
    }
}
