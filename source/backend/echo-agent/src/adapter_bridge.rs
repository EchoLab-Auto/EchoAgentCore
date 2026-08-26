//! Platform adapter → Agent inbound message hook.
//!
//! [`AgentMessageHook`] implements [`echo_adapter::InboundMessageHook`] so
//! adapters can deliver structured events without receiving an implicit reply.
//! Platform output remains available only through explicit tools.

use std::sync::Arc;

use async_trait::async_trait;
use echo_adapter::types::{ChannelType, IncomingMessage};
use echo_adapter::InboundMessageHook;

use crate::agent::Agent;
use crate::event::BackendEvent;
use crate::session::SessionKey;

const QQ_WAIT_REPLY_AFTER: std::time::Duration = std::time::Duration::from_secs(20);

/// Wraps an `Arc<Agent>` behind the adapter-facing inbound hook.
pub struct AgentMessageHook {
    agent: Arc<Agent>,
}

impl AgentMessageHook {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self { agent }
    }
}

#[async_trait]
impl InboundMessageHook for AgentMessageHook {
    async fn on_incoming_message(&self, msg: IncomingMessage) -> Result<(), String> {
        let agent = &self.agent;
        let key = SessionKey {
            platform: msg.platform.clone(),
            scope: if msg.channel.is_group() {
                "group".into()
            } else {
                "dm".into()
            },
            scope_id: msg.channel.group_id().unwrap_or("").to_string(),
            user_id: msg.user_id.clone(),
        };
        let session =
            agent
                .trunk
                .get_or_create(&key, msg.user_name.clone(), msg.group_name.clone());
        let session_id = session.id.clone();
        let received_at_ms = chrono::Utc::now().timestamp_millis();
        let message_sequence = agent.next_message_sequence();

        tracing::info!(
            message_sequence,
            adapter = %msg.adapter_name,
            platform = %msg.platform,
            user = %msg.user_id,
            channel = %msg.channel,
            session = %session_id,
            source_timestamp = msg.timestamp,
            received_at_ms,
            receive_lag_ms = received_at_ms.saturating_sub(msg.timestamp.saturating_mul(1000)),
            "inbound adapter message received"
        );

        agent.emit(BackendEvent::SessionUpdated {
            session: session.info(String::new()),
        });
        agent.emit(BackendEvent::MessageReceived {
            session_id: session_id.clone(),
            adapter_name: msg.adapter_name.clone(),
            platform: msg.platform.clone(),
            user_id: msg.user_id.clone(),
            user_name: msg.user_name.clone(),
            channel: format!("{}", msg.channel),
            group_name: msg.group_name.clone(),
            content: msg.content.clone(),
            images: msg.images.clone(),
            timestamp: msg.timestamp,
            received_at_ms,
            message_sequence,
            team_id: None,
        });

        let hook_input = format_hook_input(&msg, message_sequence, received_at_ms);

        // Cancellation is a transport-level control command. Handle it before
        // reserving a conversation turn so it can interrupt work immediately.
        if let Some(cancel_all) = recall_command_scope(&msg.content) {
            agent
                .record_incoming_and_snapshot(&session, &hook_input)
                .await;
            let cancelled = agent.cancel_requested_work(&session_id, cancel_all).await;
            tracing::info!(
                session = %session_id,
                message_sequence,
                cancel_all,
                cancelled,
                "QQ cancellation command handled"
            );
            let reply = if cancelled > 0 {
                if cancel_all && cancelled > 1 {
                    format!("已停止 {cancelled} 个进行中的任务。")
                } else {
                    "已停止当前任务。".into()
                }
            } else {
                "当前没有可停止的任务。".into()
            };
            let group_id = msg.channel.group_id().map(str::to_string);
            let agent = Arc::clone(agent);
            tokio::spawn(async move {
                let sent = agent
                    .send_control_reply(&session, group_id.as_deref(), &reply)
                    .await;
                agent
                    .record_control_reply(&session, reply.clone(), message_sequence)
                    .await;
                if let Err(error) = sent {
                    tracing::warn!(%error, session = %session_id, "cancellation acknowledgement failed");
                    agent.emit(BackendEvent::Error {
                        session_id: Some(session_id),
                        message: error,
                    });
                }
            });
            return Ok(());
        }

        // Every inbound conversation turn runs as an isolated snapshot branch
        // through the agent's single lifecycle implementation.
        let group_id = msg.channel.group_id().map(str::to_string);
        Arc::clone(agent)
            .process_inbound_branch(
                &session,
                &hook_input,
                message_sequence,
                group_id,
                QQ_WAIT_REPLY_AFTER,
            )
            .await;
        Ok(())
    }

    fn on_connection_state(&self, adapter_name: &str, connected: bool, self_id: Option<String>) {
        self.agent.emit(BackendEvent::AdapterStateChanged {
            adapter_name: adapter_name.into(),
            connected,
            self_id,
        });
    }
}

/// Encode an adapter event as a clearly marked, structured hook input.
/// Message content stays inside JSON so it cannot be confused with source
/// metadata such as sender or group identifiers.
fn format_hook_input(msg: &IncomingMessage, message_sequence: u64, received_at_ms: i64) -> String {
    let channel = match &msg.channel {
        ChannelType::Direct => serde_json::json!({
            "type": "private"
        }),
        ChannelType::Group { group_id } => serde_json::json!({
            "type": "group",
            "group_id": group_id,
            "group_name": msg.group_name
        }),
    };
    let payload = serde_json::json!({
        "event": format!("{}_message", msg.platform),
        "adapter": msg.adapter_name,
        "platform": msg.platform,
        "channel": channel,
        "sender": {
            "user_id": msg.user_id,
            "name": msg.user_name
        },
        "at_me": msg.at_me,
        "timestamp": msg.timestamp,
        "received_at_ms": received_at_ms,
        "message_sequence": message_sequence,
        "content": msg.content,
        "metadata": msg.metadata,
        "images": msg.images
    });
    crate::input_marker::wrap_hook_value(&msg.platform, &payload)
}

/// Returns `Some(true)` for "cancel all" and `Some(false)` for the newest task.
fn recall_command_scope(content: &str) -> Option<bool> {
    let normalized = content
        .trim()
        .to_lowercase()
        .chars()
        .filter(|ch| !ch.is_whitespace() && !"，。！？,.!?；;：:".contains(*ch))
        .collect::<String>();
    let cancel_all = [
        "全部停止",
        "停止全部任务",
        "取消全部任务",
        "撤回全部任务",
        "终止全部任务",
        "cancelall",
        "/cancelall",
        "/stopall",
    ];
    if cancel_all.contains(&normalized.as_str()) {
        return Some(true);
    }
    let cancel_one = [
        "停止任务",
        "停止当前任务",
        "停下当前任务",
        "取消任务",
        "取消当前任务",
        "取消刚才的任务",
        "撤回任务",
        "撤回刚才的任务",
        "终止任务",
        "终止当前任务",
        "/cancel",
        "/stop",
        "/abort",
    ];
    cancel_one.contains(&normalized.as_str()).then_some(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{
        ChatChunk, ChatMessage, ChatRequest, ChatResponse, LlmError, LlmProvider, ToolCall, Usage,
    };
    use crate::tool::{Tool, ToolError};
    use echo_adapter::types::{ChannelType, IncomingMessage};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct BridgeProvider {
        calls: AtomicUsize,
        reply: String,
    }

    #[async_trait]
    impl LlmProvider for BridgeProvider {
        fn name(&self) -> &str {
            "bridge-test"
        }

        fn default_model(&self) -> &str {
            "bridge-test"
        }

        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                let is_group = request
                    .messages
                    .iter()
                    .any(|message| message.content.contains(r#""type": "group""#));
                let (name, arguments) = if is_group {
                    ("send_group_msg", r#"{"group_id":999,"content":"ok"}"#)
                } else {
                    ("send_private_msg", r#"{"user_id":123456,"content":"ok"}"#)
                };
                return Ok(ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: "send".into(),
                        name: name.into(),
                        arguments: arguments.into(),
                    }],
                    usage: Usage::default(),
                });
            }
            Ok(ChatResponse {
                content: Some(self.reply.clone()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: Usage::default(),
            })
        }

        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<(), LlmError> {
            Ok(())
        }
    }

    struct SendTool(&'static str);

    #[async_trait]
    impl Tool for SendTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "test QQ send"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, _arguments: serde_json::Value) -> Result<String, ToolError> {
            Ok("message sent".into())
        }
    }

    struct BlockingProvider {
        entered: Arc<AtomicUsize>,
        seen_sequences: Arc<std::sync::Mutex<Vec<Vec<u64>>>>,
    }

    #[async_trait]
    impl LlmProvider for BlockingProvider {
        fn name(&self) -> &str {
            "blocking-test"
        }

        fn default_model(&self) -> &str {
            "blocking-test"
        }

        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            let sequences = request
                .messages
                .iter()
                .filter_map(|message| crate::agent::structured_message_sequence(&message.content))
                .collect::<Vec<_>>();
            self.seen_sequences
                .lock()
                .expect("seen sequences poisoned")
                .push(sequences);
            self.entered.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }

        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<(), LlmError> {
            Ok(())
        }
    }

    struct CountingSendTool {
        name: &'static str,
        calls: Arc<AtomicUsize>,
    }

    struct WaitReplyProvider {
        calls: Arc<AtomicUsize>,
        reply: String,
    }

    #[async_trait]
    impl LlmProvider for WaitReplyProvider {
        fn name(&self) -> &str {
            "wait-reply-test"
        }

        fn default_model(&self) -> &str {
            "wait-reply-test"
        }

        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(request.tools.is_none());
            assert!(request.messages[0]
                .content
                .contains("Avoid fixed-template wording"));
            Ok(ChatResponse {
                content: Some(self.reply.clone()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: Usage::default(),
            })
        }

        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<(), LlmError> {
            Ok(())
        }
    }

    struct RecordingSendTool {
        contents: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Tool for RecordingSendTool {
        fn name(&self) -> &str {
            "send_private_msg"
        }

        fn description(&self) -> &str {
            "record test sends"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, arguments: serde_json::Value) -> Result<String, ToolError> {
            self.contents.lock().expect("recorded sends poisoned").push(
                arguments["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
            Ok("message sent".into())
        }
    }

    #[async_trait]
    impl Tool for CountingSendTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "count test sends"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, _arguments: serde_json::Value) -> Result<String, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("message sent".into())
        }
    }

    fn mock_agent(reply: &str) -> Arc<Agent> {
        let provider = Arc::new(BridgeProvider {
            calls: AtomicUsize::new(0),
            reply: reply.to_string(),
        });
        let mut tools = crate::tool::ToolRegistry::new();
        tools.register(Arc::new(SendTool("send_private_msg")));
        tools.register(Arc::new(SendTool("send_group_msg")));
        Arc::new(Agent::new(
            provider,
            crate::config::AgentConfig::default(),
            crate::skill::SkillRegistry::new(),
            tools,
            Arc::new(echo_adapter::registry::AdapterRegistry::new()),
        ))
    }

    fn dm_message(content: &str) -> IncomingMessage {
        IncomingMessage {
            adapter_name: "qq".into(),
            platform: "qq".into(),
            user_id: "123456".into(),
            user_name: "tester".into(),
            channel: ChannelType::Direct,
            group_name: None,
            content: content.into(),
            timestamp: 1700000000,
            at_me: false,
            metadata: serde_json::Value::Null,
            images: vec![],
        }
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !predicate() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition should become true");
    }

    #[tokio::test]
    async fn routes_message_through_one_way_hook() {
        let hook = AgentMessageHook::new(mock_agent("后台完成"));
        let result = hook.on_incoming_message(dm_message("hello")).await;
        assert_eq!(result, Ok(()));
        wait_until(|| {
            hook.agent
                .trunk
                .all()
                .first()
                .and_then(|session| session.history.try_lock().ok())
                .is_some_and(|history| history.len() >= 4)
        })
        .await;

        // The session was created with the QQ scoping.
        let sessions = hook.agent.trunk.all();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.session_key.platform, "qq");
        assert_eq!(session.session_key.scope, "dm");
        assert_eq!(session.session_key.user_id, "123456");
        let history = session.history.lock().await;
        assert!(history[0].content.starts_with("<qq_message_hook>"));
        assert!(history[0].content.contains("\"event\": \"qq_message\""));
        assert!(history[0].content.contains("\"user_id\": \"123456\""));
        assert!(history[0].content.contains("\"content\": \"hello\""));
        // user + synthesized assistant tool_use + tool result + final reply.
        assert_eq!(history[1].role, crate::llm::ChatRole::Assistant);
        assert_eq!(
            history[1].tool_calls.as_ref().unwrap()[0].name,
            "send_private_msg"
        );
        assert_eq!(history[2].role, crate::llm::ChatRole::Tool);
        assert_eq!(history[3].content, "后台完成");
    }

    #[tokio::test]
    async fn group_message_uses_group_scope() {
        let hook = AgentMessageHook::new(mock_agent("ok"));
        let mut msg = dm_message("hello");
        msg.channel = ChannelType::Group {
            group_id: "999".into(),
        };
        let _ = hook.on_incoming_message(msg).await;

        let sessions = hook.agent.trunk.all();
        let session = &sessions[0];
        assert_eq!(session.session_key.scope, "group");
        assert_eq!(session.session_key.scope_id, "999");
        let history = session.history.lock().await;
        assert!(history[0].content.contains("\"type\": \"group\""));
        assert!(history[0].content.contains("\"group_id\": \"999\""));
    }

    #[tokio::test]
    async fn connection_state_propagates() {
        let hook = AgentMessageHook::new(mock_agent("x"));
        // Must not panic with no handle attached.
        hook.on_connection_state("qq", true, Some("10001".into()));
    }

    #[tokio::test]
    async fn contextual_wait_reply_is_generated_once_and_suppressed_when_obsolete() {
        assert_eq!(QQ_WAIT_REPLY_AFTER, std::time::Duration::from_secs(20));
        let model_calls = Arc::new(AtomicUsize::new(0));
        let sent_contents = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(WaitReplyProvider {
            calls: Arc::clone(&model_calls),
            reply: "我正在核对你提到的配置差异，整理好就回复你。".into(),
        });
        let mut tools = crate::tool::ToolRegistry::new();
        tools.register(Arc::new(RecordingSendTool {
            contents: Arc::clone(&sent_contents),
        }));
        let agent = Arc::new(Agent::new(
            provider,
            crate::config::AgentConfig::default(),
            crate::skill::SkillRegistry::new(),
            tools,
            Arc::new(echo_adapter::registry::AdapterRegistry::new()),
        ));
        let session = agent.trunk.get_or_create(
            &SessionKey::parse("qq:dm::123456").unwrap(),
            "tester".into(),
            None,
        );

        let (_reply_tx, reply_rx) = tokio::sync::watch::channel(false);
        let completed = tokio_util::sync::CancellationToken::new();
        let snapshot = vec![ChatMessage::user("帮我核对这两份配置的差异")];
        crate::agent::spawn_contextual_wait_reply(
            Arc::clone(&agent),
            session.clone(),
            "branch-1".into(),
            None,
            snapshot.clone(),
            reply_rx,
            completed,
            std::time::Duration::from_millis(10),
        );
        wait_until(|| sent_contents.lock().expect("recorded sends poisoned").len() == 1).await;
        assert_eq!(model_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            sent_contents
                .lock()
                .expect("recorded sends poisoned")
                .as_slice(),
            ["我正在核对你提到的配置差异，整理好就回复你。"]
        );

        let (reply_tx, reply_rx) = tokio::sync::watch::channel(false);
        reply_tx.send(true).unwrap();
        crate::agent::spawn_contextual_wait_reply(
            Arc::clone(&agent),
            session.clone(),
            "branch-2".into(),
            None,
            snapshot.clone(),
            reply_rx,
            tokio_util::sync::CancellationToken::new(),
            std::time::Duration::from_millis(10),
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(model_calls.load(Ordering::SeqCst), 1);

        let (_reply_tx, reply_rx) = tokio::sync::watch::channel(false);
        let completed = tokio_util::sync::CancellationToken::new();
        completed.cancel();
        crate::agent::spawn_contextual_wait_reply(
            agent,
            session,
            "branch-3".into(),
            None,
            snapshot,
            reply_rx,
            completed,
            std::time::Duration::from_millis(10),
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(model_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn successful_qq_delivery_marks_the_branch_as_replied() {
        let agent = mock_agent("done");
        let msg = dm_message("hello");
        let session = agent.trunk.get_or_create(
            &SessionKey::parse("qq:dm::123456").unwrap(),
            "tester".into(),
            None,
        );
        let hook_input = format_hook_input(&msg, 1, chrono::Utc::now().timestamp_millis());
        let snapshot = agent
            .record_incoming_and_snapshot(&session, &hook_input)
            .await;
        let (reply_tx, reply_rx) = tokio::sync::watch::channel(false);

        agent
            .process_recorded_message_with_progress(
                &session,
                &hook_input,
                snapshot,
                tokio_util::sync::CancellationToken::new(),
                Some(reply_tx),
                "test-branch",
            )
            .await
            .unwrap();

        assert!(*reply_rx.borrow());
    }

    #[tokio::test]
    async fn occupied_conversation_branches_and_recall_cancels_running_turns() {
        let entered = Arc::new(AtomicUsize::new(0));
        let sends = Arc::new(AtomicUsize::new(0));
        let seen_sequences = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(BlockingProvider {
            entered: Arc::clone(&entered),
            seen_sequences: Arc::clone(&seen_sequences),
        });
        let mut tools = crate::tool::ToolRegistry::new();
        for name in ["send_private_msg", "send_group_msg"] {
            tools.register(Arc::new(CountingSendTool {
                name,
                calls: Arc::clone(&sends),
            }));
        }
        let agent = Arc::new(Agent::new(
            provider,
            crate::config::AgentConfig::default(),
            crate::skill::SkillRegistry::new(),
            tools,
            Arc::new(echo_adapter::registry::AdapterRegistry::new()),
        ));
        let hook = AgentMessageHook::new(Arc::clone(&agent));

        hook.on_incoming_message(dm_message("第一句话"))
            .await
            .unwrap();
        wait_until(|| entered.load(Ordering::SeqCst) >= 1).await;
        hook.on_incoming_message(dm_message("第二句话"))
            .await
            .unwrap();
        wait_until(|| entered.load(Ordering::SeqCst) >= 2).await;
        assert_eq!(agent.active_inbound_turn_count(), 2);
        assert_eq!(agent.active_inbound_branch_count(), 2);
        let snapshots = seen_sequences
            .lock()
            .expect("seen sequences poisoned")
            .clone();
        assert!(snapshots.iter().any(|sequences| sequences == &[1]));
        assert!(snapshots.iter().any(|sequences| sequences == &[1, 2]));

        hook.on_incoming_message(dm_message("取消全部任务"))
            .await
            .unwrap();
        wait_until(|| agent.active_inbound_turn_count() == 0).await;
        wait_until(|| sends.load(Ordering::SeqCst) == 1).await;

        let session = agent.trunk.all().pop().unwrap();
        let history = session.history.lock().await;
        let user_sequences = history
            .iter()
            .filter_map(|message| crate::agent::structured_message_sequence(&message.content))
            .collect::<Vec<_>>();
        assert_eq!(user_sequences, vec![1, 2, 3]);
        assert!(history
            .iter()
            .any(|message| message.content == "已停止 2 个进行中的任务。"));
    }

    #[test]
    fn recall_detection_is_explicit_and_avoids_normal_questions() {
        assert_eq!(recall_command_scope("取消当前任务"), Some(false));
        assert_eq!(recall_command_scope(" /cancel-all "), None);
        assert_eq!(recall_command_scope("取消全部任务"), Some(true));
        assert_eq!(recall_command_scope("取消任务应该如何实现？"), None);
        assert_eq!(recall_command_scope("不要取消，继续执行"), None);
        // Bare words must NOT cancel — they are ordinary conversation.
        assert_eq!(recall_command_scope("停"), None);
        assert_eq!(recall_command_scope("取消"), None);
        assert_eq!(recall_command_scope("stop"), None);
        assert_eq!(recall_command_scope("cancel"), None);
        assert_eq!(recall_command_scope("算了"), None);
        assert_eq!(recall_command_scope("不用了"), None);
        // Slash commands still work.
        assert_eq!(recall_command_scope("/cancel"), Some(false));
        assert_eq!(recall_command_scope("/stop"), Some(false));
        assert_eq!(recall_command_scope("/stopall"), Some(true));
    }
}
