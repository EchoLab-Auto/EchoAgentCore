//! Projection from the event log to model-facing messages, plus compaction.

use echo_defs::message::ChatMessage;
use echo_defs::token::{estimate_history_tokens, estimate_message_tokens};

use crate::event::{
    assistant_message, tool_result_message, user_message, CompactionEvent, SessionEvent,
};

/// A compaction result: the events replaced and the summary that stands in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactedRange {
    /// Number of leading events replaced.
    pub replaced_count: usize,
    /// The summary text replacing them.
    pub summary: String,
}

/// Project the full model-facing message list from the log, honoring any
/// compaction events.
///
/// A compaction event covers the `replaced_count` events **before it**: those
/// events' projections are dropped and the summary stands in their place.
/// This is the lossless projection — nothing is trimmed for token budget
/// here; [`derive_messages`] applies the budget on top.
pub fn project_messages(log: &[SessionEvent]) -> Vec<ChatMessage> {
    // Mark event indices covered by compaction events.
    let mut covered = vec![false; log.len()];
    for (index, event) in log.iter().enumerate() {
        if let SessionEvent::Compaction(CompactionEvent { replaced_count, .. }) = event {
            let start = index.saturating_sub(*replaced_count);
            covered[start..index].fill(true);
        }
    }
    let mut messages = Vec::with_capacity(log.len());
    // The agent records each tool call as its own ToolCall event (the
    // assistant text reply is a separate event), so the log never contains an
    // assistant message carrying the tool_use blocks. Without a synthesized
    // assistant tool_use message, every following tool_result would orphan its
    // `tool_use_id` after a reload and the provider rejects the request.
    // Call ids already carried by a real AssistantMessage event are skipped.
    let mut covered_call_ids: std::collections::HashSet<String> = Default::default();
    let mut pending_tool_calls: Vec<echo_defs::message::ToolCall> = Vec::new();
    for (index, event) in log.iter().enumerate() {
        if covered[index] {
            continue;
        }
        match event {
            SessionEvent::UserMessage(event) => {
                flush_tool_calls(&mut messages, &mut pending_tool_calls, &covered_call_ids);
                messages.push(user_message(event));
            }
            SessionEvent::AssistantMessage(event) => {
                flush_tool_calls(&mut messages, &mut pending_tool_calls, &covered_call_ids);
                if !event.tool_calls.is_empty() {
                    covered_call_ids.extend(event.tool_calls.iter().map(|call| call.id.clone()));
                }
                messages.push(assistant_message(event));
            }
            SessionEvent::ToolResult(event) => {
                flush_tool_calls(&mut messages, &mut pending_tool_calls, &covered_call_ids);
                messages.push(tool_result_message(event));
            }
            SessionEvent::ToolCall(event) => {
                pending_tool_calls.push(echo_defs::message::ToolCall {
                    id: event.id.clone(),
                    name: event.name.clone(),
                    arguments: event.arguments.clone(),
                });
            }
            SessionEvent::Compaction(CompactionEvent { summary, .. }) => {
                flush_tool_calls(&mut messages, &mut pending_tool_calls, &covered_call_ids);
                messages.push(ChatMessage::user(format!("[历史摘要] {summary}")));
            }
        }
    }
    flush_tool_calls(&mut messages, &mut pending_tool_calls, &covered_call_ids);
    messages
}

/// Emit buffered `ToolCall` events as one assistant tool_use message, unless
/// every pending call id is already covered by a real assistant message.
fn flush_tool_calls(
    messages: &mut Vec<ChatMessage>,
    pending: &mut Vec<echo_defs::message::ToolCall>,
    covered_call_ids: &std::collections::HashSet<String>,
) {
    if pending.is_empty() {
        return;
    }
    let calls: Vec<echo_defs::message::ToolCall> = pending
        .drain(..)
        .filter(|call| !covered_call_ids.contains(&call.id))
        .collect();
    if calls.is_empty() {
        return;
    }
    messages.push(ChatMessage {
        role: echo_defs::message::ChatRole::Assistant,
        content: String::new(),
        reasoning_content: None,
        tool_calls: Some(calls),
        tool_call_id: None,
        images: vec![],
    });
}

/// Derive the model-facing context under a token budget.
///
/// Trimming happens here, at projection time, never on the log itself. The
/// projection is split at message boundaries so a single oversized message is
/// truncated in place (the same behaviour as the pre-event-sourced trunk).
///
/// The last step repairs tool-call pairing on the projected copy: the durable
/// log can legitimately end mid-pair (a tool that is cancelled, times out, or
/// is killed mid-execution records its `ToolCall` event but its `ToolResult`
/// never lands), and trimming or a compaction boundary can cut between a
/// tool_use message and its result. Providers reject both shapes (Anthropic:
/// HTTP 400 "tool_use ids were found without tool_result blocks"), so dangling
/// calls gain a synthetic error result and orphan results are dropped.
pub fn derive_messages(log: &[SessionEvent], token_budget: usize) -> Vec<ChatMessage> {
    let mut messages = project_messages(log);
    merge_consecutive_assistant_messages(&mut messages);
    trim_to_budget(&mut messages, token_budget);
    repair_tool_pairing(&mut messages);
    messages
}

/// Repair tool-call pairing on a projected message list.
///
/// Runs on the projected copy, never on the log. A tool message is kept only
/// when its `tool_call_id` answers a tool call still waiting for its result;
/// any call left waiting when the pairing breaks (another message role
/// intervenes, or the list simply ends) is closed with a synthetic error
/// result so the model sees the call as interrupted instead of the provider
/// rejecting the whole request.
fn repair_tool_pairing(messages: &mut Vec<ChatMessage>) {
    let mut repaired: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    // Tool calls whose result has not appeared yet, in call order.
    let mut pending: Vec<(String, String)> = Vec::new();
    for message in messages.drain(..) {
        if message.role == echo_defs::message::ChatRole::Tool {
            let position = message
                .tool_call_id
                .as_ref()
                .and_then(|id| pending.iter().position(|(pending_id, _)| pending_id == id));
            // A missing position means an orphan result: its tool_use was
            // trimmed or compacted away. Drop it or the provider rejects the
            // request.
            if let Some(position) = position {
                pending.remove(position);
                repaired.push(message);
            }
            continue;
        }
        close_dangling_calls(&mut repaired, &mut pending);
        if message.role == echo_defs::message::ChatRole::Assistant {
            if let Some(calls) = &message.tool_calls {
                pending.extend(
                    calls
                        .iter()
                        .map(|call| (call.id.clone(), call.name.clone())),
                );
            }
        }
        repaired.push(message);
    }
    close_dangling_calls(&mut repaired, &mut pending);
    *messages = repaired;
}

/// Append a synthetic error result for every tool call that never got one.
/// The `error:` prefix matches the runtime's own failure convention, so the
/// call reads as failed rather than successful-but-empty.
fn close_dangling_calls(messages: &mut Vec<ChatMessage>, pending: &mut Vec<(String, String)>) {
    for (id, name) in pending.drain(..) {
        messages.push(ChatMessage::tool(
            format!("error: tool '{name}' was interrupted before its result was recorded"),
            id,
        ));
    }
}

/// Merge consecutive assistant messages so the request alternates roles.
///
/// The reply-merge path can place an assistant text next to a synthesized
/// tool_use, so two assistant messages can stack up; merging keeps the
/// tool_use attached to the text it accompanies. Tool messages are left
/// untouched (the provider serializer groups them into one user message),
/// and user messages are left untouched because the concurrent-reply merge
/// keys off the per-message sequence marker inside each user message.
fn merge_consecutive_assistant_messages(messages: &mut Vec<ChatMessage>) {
    let mut merged: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    for message in messages.drain(..) {
        match merged.last_mut() {
            Some(last)
                if last.role == message.role
                    && last.role == echo_defs::message::ChatRole::Assistant =>
            {
                last.content.push_str(&message.content);
                match (&mut last.tool_calls, message.tool_calls) {
                    (Some(existing), Some(added)) => existing.extend(added),
                    (existing @ None, added) => *existing = added,
                    _ => {}
                }
                if last.reasoning_content.is_none() {
                    last.reasoning_content = message.reasoning_content;
                }
            }
            _ => merged.push(message),
        }
    }
    *messages = merged;
}

/// Trim a projected message list to the token budget, oldest first.
///
/// Non-destructive with respect to the log: it only mutates the projected
/// copy. Keeps at least the newest message (truncating it in place if needed).
pub fn trim_to_budget(messages: &mut Vec<ChatMessage>, token_budget: usize) {
    let mut total = estimate_history_tokens(messages);
    let mut head = 0usize;
    while messages.len() - head > 1 && total > token_budget {
        total = total.saturating_sub(estimate_message_tokens(&messages[head]));
        head += 1;
    }
    if head > 0 {
        messages.drain(..head);
    }
    if let Some(last) = messages.last_mut() {
        if estimate_message_tokens(last) > token_budget {
            echo_defs::token::truncate_message_to_tokens(last, token_budget);
        }
    }
}

/// Build a compaction event replacing the leading `replaced_count` events
/// with `summary`. The log must have at least that many events.
pub fn compact_prefix(
    log: &[SessionEvent],
    replaced_count: usize,
    summary: impl Into<String>,
) -> Option<CompactionEvent> {
    if replaced_count == 0 || replaced_count > log.len() {
        return None;
    }
    Some(CompactionEvent {
        replaced_count,
        summary: summary.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AssistantMessage, ToolCallEvent, ToolResultEvent, UserMessage};

    fn user(content: &str) -> SessionEvent {
        SessionEvent::UserMessage(UserMessage {
            content: content.into(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
        })
    }

    fn assistant(content: &str) -> SessionEvent {
        SessionEvent::AssistantMessage(AssistantMessage {
            content: content.into(),
            reasoning_content: None,
            tool_calls: vec![],
        })
    }

    #[test]
    fn projection_maps_user_and_assistant() {
        let log = vec![user("你好"), assistant("回复")];
        let messages = project_messages(&log);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "你好");
        assert_eq!(messages[1].content, "回复");
    }

    #[test]
    fn projection_preserves_tool_results_after_assistant_calls() {
        let log = vec![
            user("计算"),
            SessionEvent::AssistantMessage(AssistantMessage {
                content: String::new(),
                reasoning_content: None,
                tool_calls: vec![echo_defs::message::ToolCall {
                    id: "c1".into(),
                    name: "calc".into(),
                    arguments: r#"{"expr":"1+1"}"#.into(),
                }],
            }),
            SessionEvent::ToolCall(ToolCallEvent {
                id: "c1".into(),
                name: "calc".into(),
                arguments: r#"{"expr":"1+1"}"#.into(),
            }),
            SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "c1".into(),
                result: "2".into(),
                images: vec![],
            }),
            assistant("结果是 2"),
        ];
        let messages = project_messages(&log);
        // user + assistant(with tool_calls) + tool(result) + assistant
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(messages[2].role, echo_defs::message::ChatRole::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn tool_call_events_synthesize_assistant_tool_use() {
        // The agent's real log pattern: each tool call is its own ToolCall
        // event and the assistant text reply is a separate event, so no
        // assistant message carries the tool_use blocks. The projection must
        // synthesize one, or the tool_result orphans its `tool_use_id` after
        // a reload (HTTP 400 from the provider).
        let log = vec![
            user("列出文件"),
            SessionEvent::ToolCall(ToolCallEvent {
                id: "call_00_abc".into(),
                name: "run_command".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            }),
            SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "call_00_abc".into(),
                result: "a.txt".into(),
                images: vec![],
            }),
            assistant("已列出"),
        ];
        let messages = project_messages(&log);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, echo_defs::message::ChatRole::User);
        assert_eq!(messages[1].role, echo_defs::message::ChatRole::Assistant);
        let tool_use = messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(tool_use.len(), 1);
        assert_eq!(tool_use[0].id, "call_00_abc");
        assert_eq!(messages[2].role, echo_defs::message::ChatRole::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_00_abc"));
        assert_eq!(messages[3].content, "已列出");

        // The pairing survives a serialize → deserialize reload of the log.
        let json = serde_json::to_string(&log).unwrap();
        let restored: Vec<SessionEvent> = serde_json::from_str(&json).unwrap();
        let messages = project_messages(&restored);
        assert_eq!(
            messages[1].tool_calls.as_ref().unwrap()[0].id,
            "call_00_abc"
        );
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_00_abc"));
    }

    #[test]
    fn parallel_tool_calls_pair_each_result_with_its_tool_use() {
        // The tool loop runs calls sequentially, so the log interleaves
        // ToolCall/ToolResult per call. Each result must still be preceded by
        // an assistant tool_use carrying its id.
        let log = vec![
            user("查一下"),
            SessionEvent::ToolCall(ToolCallEvent {
                id: "call_A".into(),
                name: "adapter_status".into(),
                arguments: "{}".into(),
            }),
            SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "call_A".into(),
                result: "ok".into(),
                images: vec![],
            }),
            SessionEvent::ToolCall(ToolCallEvent {
                id: "call_B".into(),
                name: "run_command".into(),
                arguments: r#"{"command":"date"}"#.into(),
            }),
            SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "call_B".into(),
                result: "now".into(),
                images: vec![],
            }),
            assistant("完毕"),
        ];
        let messages = project_messages(&log);
        // user, assistant(A), tool(A), assistant(B), tool(B), assistant
        assert_eq!(messages.len(), 6);
        assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].id, "call_A");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_A"));
        assert_eq!(messages[3].tool_calls.as_ref().unwrap()[0].id, "call_B");
        assert_eq!(messages[4].tool_call_id.as_deref(), Some("call_B"));
    }

    #[test]
    fn assistant_message_with_tool_calls_is_not_duplicated() {
        // When a real AssistantMessage event already carries the call, the
        // duplicate ToolCall event must not synthesize a second tool_use.
        let log = vec![
            user("计算"),
            SessionEvent::AssistantMessage(AssistantMessage {
                content: String::new(),
                reasoning_content: None,
                tool_calls: vec![echo_defs::message::ToolCall {
                    id: "c1".into(),
                    name: "calc".into(),
                    arguments: r#"{"expr":"1+1"}"#.into(),
                }],
            }),
            SessionEvent::ToolCall(ToolCallEvent {
                id: "c1".into(),
                name: "calc".into(),
                arguments: r#"{"expr":"1+1"}"#.into(),
            }),
            SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "c1".into(),
                result: "2".into(),
                images: vec![],
            }),
            assistant("结果是 2"),
        ];
        let messages = project_messages(&log);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].id, "c1");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn consecutive_assistant_messages_are_merged_for_alternation() {
        // The reply-merge path can place an assistant text before the tool
        // events it precedes; the derived context must merge the synthesized
        // tool_use into that text instead of stacking two assistant messages.
        // (Consecutive user messages are preserved: the concurrent-reply
        // merge keys off the per-message sequence marker inside each one.)
        let log = vec![
            user("第一次"),
            user("第二次"),
            SessionEvent::AssistantMessage(AssistantMessage {
                content: "检查结果".into(),
                reasoning_content: None,
                tool_calls: vec![],
            }),
            SessionEvent::ToolCall(ToolCallEvent {
                id: "call_00_x".into(),
                name: "run_command".into(),
                arguments: "{}".into(),
            }),
            SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "call_00_x".into(),
                result: "ok".into(),
                images: vec![],
            }),
            assistant("完毕"),
        ];
        let messages = derive_messages(&log, 100_000);
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| match m.role {
                echo_defs::message::ChatRole::User => "user",
                echo_defs::message::ChatRole::Assistant => "assistant",
                echo_defs::message::ChatRole::Tool => "tool",
                _ => "other",
            })
            .collect();
        assert_eq!(
            roles,
            vec!["user", "user", "assistant", "tool", "assistant"]
        );
        assert_eq!(messages[0].content, "第一次");
        assert_eq!(messages[1].content, "第二次");
        // The tool_use merged into the preceding assistant text, so no two
        // assistant messages stack.
        assert_eq!(messages[2].tool_calls.as_ref().unwrap()[0].id, "call_00_x");
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_00_x"));
        assert_eq!(messages[4].content, "完毕");
    }

    #[test]
    fn compaction_replaces_covered_prefix() {
        // Compaction covers the `replaced_count` events before it: the a/b/c
        // events are replaced by the summary, and assistant("d") survives.
        let log = vec![
            user("a"),
            user("b"),
            user("c"),
            SessionEvent::Compaction(CompactionEvent {
                replaced_count: 3,
                summary: "前三条已压缩".into(),
            }),
            assistant("d"),
        ];
        let messages = project_messages(&log);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].content.contains("前三条已压缩"));
        assert_eq!(messages[1].content, "d");
    }

    #[test]
    fn derive_trims_oldest_first() {
        // 600 CJK chars ≈ 600 tokens per message; budget 100 keeps the last.
        let log = vec![user(&"x".repeat(600)), assistant("最终回复")];
        let messages = derive_messages(&log, 100);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "最终回复");
    }

    #[test]
    fn compact_prefix_validates_range() {
        let log = vec![user("a"), user("b")];
        assert!(compact_prefix(&log, 0, "s").is_none());
        assert!(compact_prefix(&log, 3, "s").is_none());
        assert_eq!(compact_prefix(&log, 2, "s").unwrap().replaced_count, 2);
    }

    /// Runtime invariant: every projected message is reconstructable from the
    /// log — a round-trip through the event log preserves all model-visible
    /// facts (including tool calls and tool results).
    #[test]
    fn invariant_model_visible_means_logged() {
        let log = vec![
            user("你好"),
            SessionEvent::AssistantMessage(AssistantMessage {
                content: String::new(),
                reasoning_content: Some("思考中".into()),
                tool_calls: vec![echo_defs::message::ToolCall {
                    id: "c1".into(),
                    name: "calc".into(),
                    arguments: r#"{"expr":"1+1"}"#.into(),
                }],
            }),
            SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "c1".into(),
                result: "2".into(),
                images: vec![],
            }),
            assistant("结果是 2"),
        ];
        // Serialize → deserialize the log (simulates reload).
        let json = serde_json::to_string(&log).unwrap();
        let restored: Vec<SessionEvent> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, log, "log survives reload");

        // The projection from the restored log must contain every
        // model-visible fact with its structure intact.
        let messages = project_messages(&restored);
        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[1].tool_calls.as_ref().unwrap()[0].id,
            "c1",
            "tool call survives reload + projection"
        );
        assert_eq!(
            messages[1].reasoning_content.as_deref(),
            Some("思考中"),
            "reasoning survives reload + projection"
        );
        assert_eq!(
            messages[2].tool_call_id.as_deref(),
            Some("c1"),
            "tool result linkage survives"
        );
    }

    #[test]
    fn dangling_tool_call_gains_synthetic_result() {
        // A cancelled / timed-out / killed tool records its ToolCall event but
        // the ToolResult never lands. Left as-is, the projection ends with an
        // assistant tool_use that no tool_result answers and the provider
        // rejects the whole request (HTTP 400). The derived context must close
        // the pair with a synthetic error result.
        let log = vec![
            user("跑个命令"),
            SessionEvent::ToolCall(ToolCallEvent {
                id: "call_00_dead".into(),
                name: "run_command".into(),
                arguments: r#"{"command":"sleep 999"}"#.into(),
            }),
        ];
        let messages = derive_messages(&log, 100_000);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].role, echo_defs::message::ChatRole::Assistant);
        assert_eq!(
            messages[1].tool_calls.as_ref().unwrap()[0].id,
            "call_00_dead"
        );
        assert_eq!(messages[2].role, echo_defs::message::ChatRole::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_00_dead"));
        assert!(messages[2].content.starts_with("error:"));
    }

    #[test]
    fn dangling_tool_call_is_closed_before_the_next_message() {
        // The synthetic result must sit directly after the assistant tool_use,
        // before any later user message, or the provider still rejects it.
        let log = vec![
            user("跑个命令"),
            SessionEvent::ToolCall(ToolCallEvent {
                id: "call_00_dead".into(),
                name: "run_command".into(),
                arguments: "{}".into(),
            }),
            user("先别管了"),
        ];
        let messages = derive_messages(&log, 100_000);
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| match m.role {
                echo_defs::message::ChatRole::User => "user",
                echo_defs::message::ChatRole::Assistant => "assistant",
                echo_defs::message::ChatRole::Tool => "tool",
                _ => "other",
            })
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "tool", "user"]);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_00_dead"));
        assert_eq!(messages[3].content, "先别管了");
    }

    #[test]
    fn orphan_tool_result_is_dropped() {
        // A tool result whose tool_use never made it into the context (e.g.
        // trimming cut between them) must be dropped: the provider rejects a
        // tool_result that answers no tool_use.
        let mut messages = vec![
            ChatMessage::tool("ok", "call_orphan"),
            ChatMessage::user("最新"),
        ];
        repair_tool_pairing(&mut messages);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, echo_defs::message::ChatRole::User);
    }

    #[test]
    fn compaction_orphaned_tool_result_is_dropped() {
        // A compaction boundary between a ToolCall and its ToolResult covers
        // the call but leaves the result: the orphan must be dropped, and the
        // summary itself must not gain dangling calls.
        let log = vec![
            user("旧消息"),
            SessionEvent::ToolCall(ToolCallEvent {
                id: "call_c".into(),
                name: "run_command".into(),
                arguments: "{}".into(),
            }),
            SessionEvent::Compaction(CompactionEvent {
                replaced_count: 2,
                summary: "已压缩".into(),
            }),
            SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "call_c".into(),
                result: "ok".into(),
                images: vec![],
            }),
            assistant("新回复"),
        ];
        let messages = derive_messages(&log, 100_000);
        assert!(messages
            .iter()
            .all(|m| m.role != echo_defs::message::ChatRole::Tool));
        assert!(messages
            .iter()
            .all(|m| m.tool_call_id.as_deref() != Some("call_c")));
    }

    #[test]
    fn paired_calls_survive_repair_untouched() {
        // Repair must not reorder or alter a healthy paired exchange.
        let log = vec![
            user("查一下"),
            SessionEvent::ToolCall(ToolCallEvent {
                id: "call_A".into(),
                name: "adapter_status".into(),
                arguments: "{}".into(),
            }),
            SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "call_A".into(),
                result: "ok".into(),
                images: vec![],
            }),
            assistant("完毕"),
        ];
        let messages = derive_messages(&log, 100_000);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].id, "call_A");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_A"));
        assert_eq!(messages[2].content, "ok");
    }
}
