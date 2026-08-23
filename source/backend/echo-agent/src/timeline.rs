//! Display-timeline projection from backend events.
//!
//! The persisted display timeline (`TrunkStore.timeline`) is a projection of
//! [`BackendEvent`]s, not a second source of truth: a [`TimelineProjector`]
//! subscribes to the [`EventBus`] as an observe listener and records only
//! conversation content (user messages with provenance, timer summaries, tool
//! calls/results, final agent outputs with reasoning). Background-task hooks
//! and lifecycle noise are deliberately skipped.
//!
//! This is the first consumer migrated onto the event bus: `Agent::emit`
//! broadcasts through the bus, and this projector (like the frontend bridge
//! later) is just another listener.

use std::collections::{HashMap, VecDeque};

use echo_context::EventBus;
use echo_protocol::{BackendEvent, TimelineMessage, TimelineSource, TimelineTool};

use crate::session::TrunkStore;

/// Pending reasoning fragments keyed by session; each queue holds
/// (branch_id, fragments in arrival order).
type ReasoningByBranch = HashMap<String, VecDeque<(String, Vec<String>)>>;

/// Records conversation content from backend events into the trunk timeline.
///
/// Holds the reasoning-correlation state (pending fragments per branch and
/// completed-branch FIFOs) that used to live on `Agent`; the agent no longer
/// mirrors the TUI's reasoning queues.
#[derive(Clone)]
pub struct TimelineProjector {
    trunk: TrunkStore,
    timeline_pending_reasoning: std::sync::Arc<std::sync::Mutex<ReasoningByBranch>>,
    timeline_completed_branches:
        std::sync::Arc<std::sync::Mutex<HashMap<String, VecDeque<String>>>>,
}

impl TimelineProjector {
    pub fn new(trunk: TrunkStore) -> Self {
        Self {
            trunk,
            timeline_pending_reasoning: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            timeline_completed_branches: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
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
                    images.clone(),
                ));
            }
            BackendEvent::AgentReasoning {
                session_id,
                branch_id,
                content,
            } => {
                let content = content.trim().to_string();
                if content.is_empty() {
                    return;
                }
                if let Ok(mut pending) = self.timeline_pending_reasoning.lock() {
                    let queue = pending.entry(session_id.clone()).or_default();
                    if let Some((_, reasoning)) = queue.back_mut() {
                        reasoning.push(content);
                    } else {
                        queue.push_back((branch_id.clone(), vec![content]));
                    }
                }
            }
            BackendEvent::ReplyBranchCompleted {
                session_id,
                branch_id,
                success,
                cancelled,
                ..
            } => {
                // The suggested collapse uses let-chains, which require
                // edition 2024; the nested guard is the readable 2021 form.
                #[allow(clippy::collapsible_match)]
                if *success && !*cancelled {
                    if let Ok(mut completed) = self.timeline_completed_branches.lock() {
                        completed
                            .entry(session_id.clone())
                            .or_default()
                            .push_back(branch_id.clone());
                    }
                }
            }
            BackendEvent::AgentOutput {
                session_id,
                content,
                branch_id,
            } => {
                let reasoning = self.take_timeline_reasoning(session_id, branch_id.as_deref());
                let reasoning = (!reasoning.is_empty()).then_some(reasoning);
                self.trunk.push_timeline(TimelineMessage::backend(
                    content.clone(),
                    session_id.clone(),
                    chrono::Utc::now().timestamp(),
                    reasoning,
                ));
            }
            BackendEvent::ToolCall {
                session_id,
                tool_name,
                arguments,
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
                    },
                ));
            }
            BackendEvent::ToolResult {
                session_id,
                tool_name,
                result,
                ..
            } => {
                let failed = result.trim_start().starts_with("error:");
                self.update_timeline_tool(session_id, tool_name, result, failed);
            }
            _ => {}
        }
    }

    /// Consume pending reasoning for an agent output. With a branch id the
    /// matching branch is removed directly; without one, the oldest completed
    /// branch of that session is consumed first (mirrors the TUI behaviour).
    fn take_timeline_reasoning(&self, session_id: &str, branch_id: Option<&str>) -> Vec<String> {
        let target = if let Some(branch_id) = branch_id {
            Some(branch_id.to_string())
        } else if let Ok(mut completed) = self.timeline_completed_branches.lock() {
            completed
                .get_mut(session_id)
                .and_then(|queue| queue.pop_front())
        } else {
            None
        };
        let Some(target) = target else {
            return Vec::new();
        };
        if let Ok(mut pending) = self.timeline_pending_reasoning.lock() {
            let Some(queue) = pending.get_mut(session_id) else {
                return Vec::new();
            };
            let Some(index) = queue.iter().position(|(branch, _)| *branch == target) else {
                return Vec::new();
            };
            let Some((_, reasoning)) = queue.remove(index) else {
                return Vec::new();
            };
            if queue.is_empty() {
                pending.remove(session_id);
            }
            reasoning
        } else {
            Vec::new()
        }
    }

    /// Attach the outcome to the newest still-running timeline tool entry with
    /// the same name (mirrors the TUI's `finish_tool_entry`).
    fn update_timeline_tool(&self, session_id: &str, tool_name: &str, result: &str, failed: bool) {
        let mut timeline = match self.trunk.timeline_mut() {
            Some(guard) => guard,
            None => return,
        };
        if let Some(entry) = timeline.iter_mut().rev().find(|entry| {
            entry.kind == "tool"
                && entry.session_id == session_id
                && entry
                    .tool
                    .as_ref()
                    .is_some_and(|tool| tool.name == tool_name && tool.output.is_none())
        }) {
            if let Some(tool) = entry.tool.as_mut() {
                tool.output = Some(summarize_timeline_value(result, 200));
                tool.failed = failed;
            }
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
            },
        ));
    }
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
        });
        projector.record(&BackendEvent::AgentOutput {
            session_id: "qq:dm::1".into(),
            content: "回复".into(),
            branch_id: None,
        });
        let timeline = store.timeline_snapshot();
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
                content: "通过总线".into(),
                branch_id: None,
            },
            DispatchMode::Observe,
        );
        let timeline = store.timeline_snapshot();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].content, "通过总线");
    }

    #[test]
    fn background_hooks_are_skipped() {
        let store = trunk();
        let projector = TimelineProjector::new(store.clone());
        projector.record(&BackendEvent::MessageReceived {
            session_id: "s".into(),
            adapter_name: "background".into(),
            platform: "local".into(),
            user_id: "u".into(),
            user_name: "bg".into(),
            channel: "direct".into(),
            group_name: None,
            content: "<background_task_event>…</background_task_event>".into(),
            images: vec![],
            timestamp: 0,
            received_at_ms: 0,
            message_sequence: 0,
        });
        assert!(store.timeline_snapshot().is_empty());
    }
}
