//! Read-only semantic projections for agent-facing session inspection.
//!
//! These functions read the live `TrunkStore`; they deliberately know nothing
//! about tool schemas, transports, or the on-disk JSON envelope.

use std::collections::HashMap;

use serde_json::{json, Value};

use echo_defs::session_query::SessionQueryView;
use echo_defs::token::{estimate_history_tokens, estimate_message_tokens};
use echo_session::SessionEvent;

use super::TrunkStore;

/// List a persona's sessions, most recently active first.
pub fn list_sessions(trunk: &TrunkStore, limit: usize) -> Value {
    let mut sessions = trunk.all();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.last_active()));
    let total = sessions.len();
    let sessions: Vec<Value> = sessions
        .into_iter()
        .take(limit)
        .map(|session| {
            let last_message = session
                .history
                .lock()
                .last()
                .map(|message| message.content.clone())
                .unwrap_or_default();
            json!({
                "session_id": session.id,
                "platform": session.session_key.platform,
                "scope": session.session_key.scope,
                "user_id": session.session_key.user_id,
                "nickname": session.nickname,
                "group_name": session.group_name,
                "last_active": session.last_active(),
                "last_message": last_message,
            })
        })
        .collect();
    json!({ "sessions": sessions, "total": total, "truncated": total > limit })
}

/// Read either the model context or the lossless audit projection.
pub fn read_session(
    trunk: &TrunkStore,
    session_id: &str,
    view: SessionQueryView,
    limit: usize,
    include_tool_results: bool,
) -> Result<Value, String> {
    match view {
        SessionQueryView::Model => {
            let history = trunk
                .history_for(session_id)
                .ok_or_else(|| format!("会话不存在或尚未加载: {session_id}"))?
                .lock()
                .clone();
            let total = history.len();
            let messages: Vec<Value> = history
                .iter()
                .skip(total.saturating_sub(limit))
                .map(|message| {
                    json!({
                        "role": format!("{:?}", message.role).to_lowercase(),
                        "content": message.content,
                        "tokens": estimate_message_tokens(message),
                        "tool_calls": message.tool_calls,
                        "tool_call_id": message.tool_call_id,
                    })
                })
                .collect();
            Ok(json!({
                "session_id": session_id,
                "view": "model",
                "messages": messages,
                "total_messages": total,
                "total_tokens": estimate_history_tokens(&history),
                "limit_tokens": trunk.memory_limit_tokens(),
                "truncated": total > limit,
                "note": "模型可见投影：已应用压缩与 token 预算，不含模型步骤观测。",
            }))
        }
        SessionQueryView::Audit => {
            let events = trunk.events_for_session(session_id);
            if events.is_empty() && trunk.get(session_id).is_none() {
                return Err(format!("会话不存在或尚未加载: {session_id}"));
            }
            Ok(audit_projection(
                session_id,
                &events,
                limit,
                include_tool_results,
            ))
        }
    }
}

fn tail<T: Clone>(items: &[T], limit: usize) -> Vec<T> {
    items[items.len().saturating_sub(limit)..].to_vec()
}

fn audit_projection(
    session_id: &str,
    events: &[SessionEvent],
    limit: usize,
    include_tool_results: bool,
) -> Value {
    let results: HashMap<&str, &echo_session::ToolResultEvent> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolResult(result) => Some((result.tool_call_id.as_str(), result)),
            _ => None,
        })
        .collect();
    let calls: Vec<&echo_session::ToolCallEvent> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();
    let total_calls = calls.len();
    let tool_calls: Vec<Value> = tail(&calls, limit)
        .into_iter()
        .map(|call| {
            let result = results.get(call.id.as_str()).copied();
            json!({
                "id": call.id,
                "name": call.name,
                "arguments": call.arguments,
                "started_at_ms": call.started_at_ms,
                "elapsed_ms": result.and_then(|result| result.elapsed_ms),
                "completed": result.is_some(),
                "succeeded": result.map(|result| !result.result.starts_with("error:")),
                "result": if include_tool_results {
                    result.map(|result| Value::String(result.result.clone())).unwrap_or(Value::Null)
                } else {
                    Value::Null
                },
            })
        })
        .collect();

    let steps: Vec<&echo_session::event::ModelStepEvent> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ModelStep(step) => Some(step),
            _ => None,
        })
        .collect();
    let total_steps = steps.len();
    let model_steps: Vec<Value> = tail(&steps, limit)
        .into_iter()
        .map(|step| {
            json!({
                "step_index": step.step_index,
                "model": step.model,
                "prompt_tokens": step.prompt_tokens,
                "completion_tokens": step.completion_tokens,
                "elapsed_ms": step.elapsed_ms,
                "started_at_ms": step.started_at_ms,
                "tool_call_ids": step.tool_call_ids,
                "turn": step.turn,
            })
        })
        .collect();

    let messages: Vec<Value> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::UserMessage(message) => Some(json!({
                "kind": "user", "content": message.content, "timestamp": message.timestamp,
                "message_sequence": message.message_sequence,
            })),
            SessionEvent::AssistantMessage(message) => Some(json!({
                "kind": "assistant", "content": message.content, "timestamp": message.timestamp,
                "reasoning_content": message.reasoning_content,
            })),
            SessionEvent::Reasoning(message) => Some(json!({
                "kind": "reasoning", "content": message.content, "timestamp": message.timestamp,
            })),
            SessionEvent::Compaction(message) => Some(json!({
                "kind": "compaction", "content": message.summary,
                "replaced_count": message.replaced_count,
            })),
            _ => None,
        })
        .collect();
    let total_messages = messages.len();

    json!({
        "session_id": session_id,
        "view": "audit",
        "messages": tail(&messages, limit),
        "tool_calls": tool_calls,
        "model_steps": model_steps,
        "total_messages": total_messages,
        "total_tool_calls": total_calls,
        "total_model_steps": total_steps,
        "truncated": total_messages > limit || total_calls > limit || total_steps > limit,
        "tool_results_included": include_tool_results,
        "note": "原始事件审计投影：耗时为 null 表示旧日志未记录或调用尚无结果。",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_session::event::ModelStepEvent;
    use echo_session::{AssistantMessage, ToolCallEvent, ToolResultEvent, UserMessage};

    fn fixture() -> TrunkStore {
        let trunk = TrunkStore::new(100_000);
        let session = Some("local:tui::local_user".to_string());
        trunk.append_event(SessionEvent::UserMessage(UserMessage {
            content: "查一下".into(),
            timestamp: 1,
            message_sequence: Some(1),
            source: None,
            images: vec![],
            files: vec![],
            session: session.clone(),
        }));
        trunk.append_event(SessionEvent::ModelStep(ModelStepEvent {
            step_index: 0,
            model: "test-model".into(),
            prompt_tokens: 10,
            completion_tokens: 5,
            tool_call_ids: vec!["c1".into()],
            turn: None,
            elapsed_ms: Some(20),
            started_at_ms: 2,
            session: session.clone(),
        }));
        trunk.append_event(SessionEvent::ToolCall(ToolCallEvent {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"a.rs\"}".into(),
            session: session.clone(),
            started_at_ms: Some(3),
        }));
        trunk.append_event(SessionEvent::ToolResult(ToolResultEvent {
            tool_call_id: "c1".into(),
            result: "fn main() {}".into(),
            images: vec![],
            session: session.clone(),
            elapsed_ms: Some(7),
        }));
        trunk.append_event(SessionEvent::AssistantMessage(AssistantMessage {
            content: "完成".into(),
            timestamp: 4,
            reasoning_content: None,
            tool_calls: vec![],
            session,
        }));
        trunk
    }

    #[test]
    fn audit_joins_tool_results_and_model_steps_without_reading_disk() {
        let value = read_session(
            &fixture(),
            "local:tui::local_user",
            SessionQueryView::Audit,
            50,
            true,
        )
        .unwrap();
        assert_eq!(value["tool_calls"][0]["name"], "read_file");
        assert_eq!(value["tool_calls"][0]["elapsed_ms"], 7);
        assert_eq!(value["tool_calls"][0]["result"], "fn main() {}");
        assert_eq!(value["model_steps"][0]["elapsed_ms"], 20);
    }

    #[test]
    fn model_and_audit_have_distinct_semantics() {
        let trunk = fixture();
        let model = read_session(
            &trunk,
            "local:tui::local_user",
            SessionQueryView::Model,
            50,
            false,
        )
        .unwrap();
        assert!(model.get("model_steps").is_none());
        assert!(model["total_tokens"].as_u64().unwrap() > 0);
        let audit = read_session(
            &trunk,
            "local:tui::local_user",
            SessionQueryView::Audit,
            50,
            false,
        )
        .unwrap();
        assert_eq!(audit["tool_calls"][0]["result"], Value::Null);
        assert_eq!(audit["tool_calls"][0]["completed"], true);
    }

    #[test]
    fn unknown_session_is_an_error_not_an_empty_context() {
        let error = read_session(
            &TrunkStore::new(1000),
            "missing",
            SessionQueryView::Audit,
            50,
            false,
        )
        .unwrap_err();
        assert!(error.contains("missing"));
    }
}
