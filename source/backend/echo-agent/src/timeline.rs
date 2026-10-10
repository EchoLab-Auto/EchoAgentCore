//! Display-timeline projection from backend events.
//!
//! The persisted display timeline (`TrunkStore.timeline`) is a projection of
//! [`BackendEvent`]s, not a second source of truth: a [`TimelineProjector`]
//! subscribes to the [`EventBus`] as an observe listener and records only
//! conversation content (user messages with provenance, timer summaries, tool
//! calls/results, final agent outputs). Background-task hooks
//! and lifecycle noise are deliberately skipped. 推理按事件到达顺序直接落
//! 为独立 reasoning 条目（不再缓冲到 backend 尾部），保证持久化时间线与
//! 实时时序一致（推理真实穿插在工具调用之间）。
//!
//! This is the first consumer migrated onto the event bus: `Agent::emit`
//! broadcasts through the bus, and this projector (like the frontend bridge
//! later) is just another listener.

use echo_context::EventBus;
use echo_protocol::{BackendEvent, TimelineMessage, TimelineSource, TimelineTool};

use crate::session::TrunkStore;

/// Records conversation content from backend events into the trunk timeline.
///
/// Holds the reasoning-correlation state (pending fragments per branch and
/// completed-branch FIFOs) that used to live on `Agent`; the agent no longer
/// mirrors the TUI's reasoning queues.
#[derive(Clone)]
pub struct TimelineProjector {
    trunk: TrunkStore,
}

impl TimelineProjector {
    pub fn new(trunk: TrunkStore) -> Self {
        Self { trunk }
    }

    /// Subscribe this projector to the bus as an observe listener on
    /// [`BackendEvent`]. Returns the disposer.
    pub fn subscribe(
        self: &std::sync::Arc<Self>,
        bus: &std::sync::Arc<EventBus>,
    ) -> echo_context::Disposer {
        let projector = self.clone();
        bus.observe::<BackendEvent, _>(move |event| {
            projector.record(event);
        })
    }

    /// Record one event into the persisted display timeline.
    fn record(&self, event: &BackendEvent) {
        match event {
            BackendEvent::MessageReceived {
                session_id,
                adapter_name,
                platform,
                user_id,
                user_name,
                channel,
                group_name,
                content,
                images,
                timestamp,
                received_at_ms,
                message_sequence,
                ..
            } => {
                if adapter_name == "background" {
                    return; // internal parent-context hook, not conversation content
                }
                if adapter_name == "timer" {
                    let task = timeline_timer_task(content);
                    let content = match task {
                        Some(task) => format!("定时任务触发 · {}", truncate_one_line(&task, 120)),
                        None => "定时任务触发".into(),
                    };
                    self.trunk.push_timeline(TimelineMessage::system(
                        content,
                        session_id.clone(),
                        *timestamp,
                    ));
                    return;
                }
                // 图片规范化（2026-09-24）：媒体引用（`/media/<id>`）原样
                // 保留（轻量，面板懒加载）；遗留的内嵌 data URI 落盘后改
                // 写为引用（落盘失败则省略占位）——时间线是"显示历史"，
                // 不因一张图写不进去而丢整条消息。
                let images: Vec<String> = images
                    .iter()
                    .map(|image| echo_defs::media_store::spill_or_keep(image))
                    .collect();
                self.trunk.push_timeline(TimelineMessage::user(
                    content.clone(),
                    session_id.clone(),
                    *timestamp,
                    Some(TimelineSource {
                        adapter_name: adapter_name.clone(),
                        platform: platform.clone(),
                        user_id: user_id.clone(),
                        user_name: user_name.clone(),
                        channel: channel.clone(),
                        group_name: group_name.clone(),
                        received_at_ms: *received_at_ms,
                        message_sequence: *message_sequence,
                    }),
                    images,
                ));
            }
            BackendEvent::AgentReasoning {
                session_id,
                branch_id: _,
                team_id: _,
                content,
            } => {
                let content = content.trim().to_string();
                if content.is_empty() {
                    return;
                }
                // 按到达顺序直接落独立条目：推理与工具调用在时间线中真实交错。
                self.trunk.push_timeline(TimelineMessage {
                    kind: "reasoning".into(),
                    content,
                    session_id: session_id.clone(),
                    time: chrono::Utc::now().timestamp(),
                    source: None,
                    reasoning: None,
                    tool: None,
                    images: None,
                    seq: 0, // 由 push_timeline 赋递增序号
                });
            }
            BackendEvent::AgentOutput {
                session_id,
                team_id: _,
                content,
                branch_id: _,
            } => {
                // 推理已作为独立 reasoning 条目落库（见 AgentReasoning 分支）；
                // backend 条目不再附加 reasoning（旧数据仍保留字段兼容）。
                self.trunk.push_timeline(TimelineMessage::backend(
                    content.clone(),
                    session_id.clone(),
                    chrono::Utc::now().timestamp(),
                    None,
                ));
            }
            BackendEvent::ToolCall {
                session_id,
                tool_name,
                arguments,
                tool_call_id,
                started_at_ms,
                ..
            } => {
                self.trunk.push_timeline(TimelineMessage::tool(
                    tool_name.clone(),
                    session_id.clone(),
                    chrono::Utc::now().timestamp(),
                    TimelineTool {
                        name: tool_name.clone(),
                        input: summarize_timeline_value(arguments, 160),
                        output: None,
                        failed: false,
                        tool_call_id: tool_call_id.clone(),
                        timed_out: false,
                        started_at_ms: *started_at_ms,
                        elapsed_ms: None,
                    },
                ));
            }
            BackendEvent::ToolResult {
                session_id,
                tool_name,
                result,
                tool_call_id,
                timed_out,
                elapsed_ms,
                ..
            } => {
                let failed = *timed_out || result.trim_start().starts_with("error:");
                self.update_timeline_tool(
                    session_id,
                    tool_name,
                    tool_call_id,
                    result,
                    failed,
                    *timed_out,
                    *elapsed_ms,
                );
            }
            _ => {}
        }
    }

    /// Attach the outcome to the newest still-running timeline tool entry.
    /// 按 tool_call_id 精确配对（同名并行调用不再配错对）；id 为空
    /// （理论不出现）时回退按名字匹配。就地更新会推进条目 seq，已同步过
    /// 的前端可在下次增量窗口中收到该条目的完成态。
    #[allow(clippy::too_many_arguments)]
    fn update_timeline_tool(
        &self,
        session_id: &str,
        tool_name: &str,
        tool_call_id: &str,
        result: &str,
        failed: bool,
        timed_out: bool,
        elapsed_ms: Option<u64>,
    ) {
        let mut timeline = match self.trunk.timeline_mut() {
            Some(guard) => guard,
            None => return,
        };
        let found = if tool_call_id.is_empty() {
            timeline.iter_mut().rev().find(|entry| {
                entry.kind == "tool"
                    && entry.session_id == session_id
                    && entry
                        .tool
                        .as_ref()
                        .is_some_and(|tool| tool.name == tool_name && tool.output.is_none())
            })
        } else {
            timeline.iter_mut().rev().find(|entry| {
                entry.kind == "tool"
                    && entry.session_id == session_id
                    && entry.tool.as_ref().is_some_and(|tool| {
                        tool.tool_call_id == tool_call_id && tool.output.is_none()
                    })
            })
        };
        if let Some(entry) = found {
            if let Some(tool) = entry.tool.as_mut() {
                tool.output = Some(summarize_timeline_value(result, 200));
                tool.failed = failed;
                tool.timed_out = timed_out;
                tool.elapsed_ms = elapsed_ms;
            }
            // 就地更新也推进条目 seq，否则增量同步（since_seq）永远看不到
            // 这次完成态，前端会一直显示 running。
            let seq = self.trunk.bump_timeline_seq();
            entry.seq = seq;
            return;
        }
        // No running entry found — record a completed tool entry directly.
        drop(timeline);
        self.trunk.push_timeline(TimelineMessage::tool(
            tool_name.to_string(),
            session_id.to_string(),
            chrono::Utc::now().timestamp(),
            TimelineTool {
                name: tool_name.to_string(),
                input: String::new(),
                output: Some(summarize_timeline_value(result, 200)),
                failed,
                tool_call_id: tool_call_id.to_string(),
                timed_out,
                started_at_ms: None,
                elapsed_ms,
            },
        ));
    }
}

/// 推理条目的 wire 瘦身阈值（2026-09，刷新加速）：推理正文占时间线快照的
/// 约七成（实测 1024 条里 1MB+），而滚动历史里的思维链几乎不回看——
/// 最近 [`WIRE_REASONING_KEEP_FULL`] 条保留全文，更早的只留开头
/// [`WIRE_REASONING_HEAD_CHARS`] 字 + 截断标注。持久化/内存中的原文不动，
/// 只瘦身发往前端的快照（`RequestTrunkTimeline` 的两种路径共用）。
pub const WIRE_REASONING_KEEP_FULL: usize = 40;
/// 被截断条目保留的开头字符数。
pub const WIRE_REASONING_HEAD_CHARS: usize = 240;

/// 按“新近程度”瘦身推理条目（见上方常量说明）。非推理条目原样保留。
pub fn elide_reasoning_for_wire(messages: Vec<TimelineMessage>) -> Vec<TimelineMessage> {
    let reasoning: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.kind == "reasoning" && !message.content.is_empty())
        .map(|(index, _)| index)
        .collect();
    if reasoning.len() <= WIRE_REASONING_KEEP_FULL {
        return messages;
    }
    let mut messages = messages;
    let cutoff = reasoning.len() - WIRE_REASONING_KEEP_FULL;
    for &index in &reasoning[..cutoff] {
        let total = messages[index].content.chars().count();
        if total <= WIRE_REASONING_HEAD_CHARS {
            continue;
        }
        let head: String = messages[index]
            .content
            .chars()
            .take(WIRE_REASONING_HEAD_CHARS)
            .collect();
        messages[index].content = format!("{head}…\n（较早的推理已截断：原文 {total} 字）");
    }
    messages
}

/// Extract a timer task summary from a `<timer_event>` payload, if parseable.
fn timeline_timer_task(content: &str) -> Option<String> {
    let body = content
        .strip_prefix(crate::input_marker::TIMER_EVENT_OPEN)?
        .strip_suffix(crate::input_marker::TIMER_EVENT_CLOSE)?;
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value.get("task")?.as_str().map(str::to_string)
}

/// Collapse whitespace and truncate to one display line.
fn truncate_one_line(value: &str, max_chars: usize) -> String {
    let collapsed: String = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        collapsed.chars().take(max_chars).collect::<String>() + "…"
    }
}

/// Truncate a tool input/output for display, collapsing whitespace.
fn summarize_timeline_value(value: &str, max_chars: usize) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(value) {
        Ok(serde_json::Value::Object(map)) => {
            let summarized = map
                .iter()
                .take(8)
                .map(|(key, value)| {
                    let rendered = if is_sensitive_timeline_key(key) {
                        "[已隐藏]".into()
                    } else {
                        match value {
                            serde_json::Value::String(s) => truncate_one_line(s, 48),
                            serde_json::Value::Array(items) => format!("[{}]", items.len()),
                            other => other.to_string(),
                        }
                    };
                    format!("{key}={rendered}")
                })
                .collect::<Vec<_>>()
                .join(" · ");
            format!("{{{summarized}}}")
        }
        Ok(other) => other.to_string(),
        Err(_) => truncate_one_line(value, max_chars),
    };
    truncate_one_line(&compact, max_chars)
}

fn is_sensitive_timeline_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["token", "secret", "password", "api_key", "access_key"]
        .iter()
        .any(|needle| key.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trunk() -> TrunkStore {
        TrunkStore::new(1000)
    }

    #[test]
    fn timer_task_extracted_from_payload() {
        let task = timeline_timer_task(
            r#"<timer_event>{"task":"提醒喝水","due_at":"2026-01-01T00:00:00Z"}</timer_event>"#,
        );
        assert_eq!(task.as_deref(), Some("提醒喝水"));
    }

    #[test]
    fn malformed_timer_payload_is_none() {
        assert!(timeline_timer_task("<timer_event>not json</timer_event>").is_none());
        assert!(timeline_timer_task("plain text").is_none());
    }

    #[test]
    fn truncate_one_line_collapses_and_cuts() {
        assert_eq!(truncate_one_line("a\nb   c", 10), "a b c");
        let cut = truncate_one_line("一二三四五六七八九十", 5);
        assert_eq!(cut.chars().count(), 6, "5 chars + ellipsis");
        assert!(cut.ends_with('…'));
    }

    #[tokio::test]
    async fn projector_records_user_message_and_output() {
        let store = trunk();
        let projector = TimelineProjector::new(store.clone());
        projector.record(&BackendEvent::MessageReceived {
            session_id: "qq:dm::1".into(),
            adapter_name: "qq".into(),
            platform: "qq".into(),
            user_id: "1".into(),
            user_name: "alice".into(),
            channel: "direct".into(),
            group_name: None,
            content: "你好".into(),
            images: vec![],
            timestamp: 1700000000,
            received_at_ms: 1700000000123,
            message_sequence: 1,
            team_id: None,
        });
        projector.record(&BackendEvent::AgentOutput {
            session_id: "qq:dm::1".into(),
            team_id: None,
            content: "回复".into(),
            branch_id: None,
        });
        let timeline = store.timeline_snapshot().unwrap_or_default();
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[0].kind, "user");
        assert_eq!(timeline[0].content, "你好");
        assert_eq!(timeline[1].kind, "backend");
        assert_eq!(timeline[1].content, "回复");
    }

    #[tokio::test]
    async fn bus_subscription_records_events() {
        use echo_context::DispatchMode;
        let store = trunk();
        let bus: std::sync::Arc<EventBus> = std::sync::Arc::new(EventBus::default());
        let projector: std::sync::Arc<TimelineProjector> =
            std::sync::Arc::new(TimelineProjector::new(store.clone()));
        let _keep = projector.subscribe(&bus);

        bus.emit_sync(
            BackendEvent::AgentOutput {
                session_id: "qq:dm::1".into(),
                team_id: None,
                content: "通过总线".into(),
                branch_id: None,
            },
            DispatchMode::Observe,
        );
        let timeline = store.timeline_snapshot().unwrap_or_default();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].content, "通过总线");
    }

    // ── 推理条目 wire 瘦身 ──

    fn reasoning_with(len: usize) -> TimelineMessage {
        TimelineMessage {
            kind: "reasoning".into(),
            content: "思".repeat(len),
            session_id: "local:tui::local_user".into(),
            time: 1,
            source: None,
            reasoning: None,
            tool: None,
            images: None,
            seq: 0,
        }
    }

    #[test]
    fn elide_keeps_recent_reasoning_untouched() {
        let mut messages: Vec<TimelineMessage> = (0..WIRE_REASONING_KEEP_FULL)
            .map(|_| reasoning_with(1000))
            .collect();
        let before = messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>();
        messages = elide_reasoning_for_wire(messages);
        let after = messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>();
        assert_eq!(before, after, "<= keep_full entries pass through unchanged");
    }

    #[test]
    fn elide_truncates_older_reasoning_with_marker() {
        // 10 条旧的 + 40 条新的：仅最旧的 10 条被截断。
        let mut messages: Vec<TimelineMessage> = (0..50).map(|_| reasoning_with(1200)).collect();
        messages = elide_reasoning_for_wire(messages);
        for (index, message) in messages.iter().enumerate() {
            let total = message.content.chars().count();
            if index < 10 {
                assert!(
                    message
                        .content
                        .ends_with("（较早的推理已截断：原文 1200 字）"),
                    "old entry {index} must carry the marker"
                );
                assert!(total < 1200, "old entry {index} is truncated");
            } else {
                assert_eq!(total, 1200, "recent entry {index} stays intact");
            }
        }
    }

    #[test]
    fn elide_leaves_short_and_non_reasoning_entries_alone() {
        let mut messages = vec![
            TimelineMessage::user("你说", "s", 1, None, vec![]),
            reasoning_with(50), // 旧且本身很短 → 不动
            TimelineMessage::user("我说", "s", 2, None, vec![]),
        ];
        for _ in 0..WIRE_REASONING_KEEP_FULL {
            messages.push(reasoning_with(800));
        }
        let original_user: Vec<String> = messages
            .iter()
            .filter(|m| m.kind == "user")
            .map(|m| m.content.clone())
            .collect();
        let elided = elide_reasoning_for_wire(messages);
        let user_after: Vec<String> = elided
            .iter()
            .filter(|m| m.kind == "user")
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(original_user, user_after, "non-reasoning entries untouched");
        assert_eq!(
            elided[1].content.chars().count(),
            50,
            "short old reasoning stays as-is"
        );
    }
}
