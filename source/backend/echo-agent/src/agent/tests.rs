use super::tool_exec::invalid_tool_arguments;
use super::*;
use crate::llm::{ChatChunk, ChatResponse, LlmError, Usage};
use crate::session::SessionKey;
use crate::tool::{Tool, ToolError};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct MockProvider {
    pub calls: Arc<AtomicUsize>,
    pub reply: String,
}

struct ConcurrentProvider {
    entered: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }
    fn default_model(&self) -> &str {
        "mock-model"
    }
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.messages[0].role, crate::llm::ChatRole::System);
        Ok(ChatResponse {
            stop_reason: None,
            content: Some(self.reply.clone()),
            reasoning_content: None,
            tool_calls: vec![],
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

#[async_trait::async_trait]
impl LlmProvider for ConcurrentProvider {
    fn name(&self) -> &str {
        "concurrent"
    }

    fn default_model(&self) -> &str {
        "concurrent"
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.release
            .acquire()
            .await
            .expect("release semaphore should stay open")
            .forget();
        let sequence = request
            .messages
            .iter()
            .filter_map(|message| structured_message_sequence(&message.content))
            .next_back()
            .unwrap_or_default();
        Ok(ChatResponse {
            stop_reason: None,
            content: Some(format!("reply-{sequence}")),
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

/// 记录每次请求看到的历史，并在测试放行前一直挂起（用于单会话排队断言）。
#[derive(Clone)]
struct SnapshotProvider {
    calls: Arc<AtomicUsize>,
    seen: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    gate: Arc<tokio::sync::Semaphore>,
}

impl SnapshotProvider {
    /// 等第一次模型调用进入（此后队列闸门被第一个 turn 持有）。
    async fn wait_entered(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while self.calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first turn should reach the provider");
    }

    /// 放行所有挂起的模型调用。
    fn release(&self) {
        self.gate.add_permits(8);
    }
}

#[async_trait::async_trait]
impl LlmProvider for SnapshotProvider {
    fn name(&self) -> &str {
        "snapshot"
    }

    fn default_model(&self) -> &str {
        "snapshot"
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let seen: Vec<String> = request
            .messages
            .iter()
            .map(|message| message.content.clone())
            .collect();
        let sequence = seen
            .iter()
            .filter_map(|content| structured_message_sequence(content))
            .next_back()
            .unwrap_or_default();
        self.seen.lock().expect("seen poisoned").push(seen);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.gate
            .acquire()
            .await
            .expect("gate semaphore should stay open")
            .forget();
        Ok(ChatResponse {
            stop_reason: None,
            content: Some(format!("reply-{sequence}")),
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

fn test_agent(provider: Arc<dyn LlmProvider>) -> Agent {
    Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    )
}

// ── 进程级事件汇聚点（去主智能体 P0）──

/// 多人格共享同一汇聚点：任一人格 emit 的事件都到达同一处，
/// 「谁挂到 Panel」不再取决于默认人格。
#[tokio::test]
async fn shared_event_sink_receives_events_from_every_persona() {
    let received = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink: EventSink = {
        let received = received.clone();
        Arc::new(move |event: BackendEvent| {
            let id = match &event {
                BackendEvent::AgentOutput { session_id, .. } => session_id.clone(),
                _ => "other".into(),
            };
            received.lock().unwrap().push(id);
        })
    };
    let a = test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "a".into(),
    }));
    let b = test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "b".into(),
    }));
    a.attach_event_sink(sink.clone());
    b.attach_event_sink(sink);

    a.emit(BackendEvent::AgentOutput {
        session_id: "from-a".into(),
        team_id: None,
        content: "x".into(),
        branch_id: None,
    });
    b.emit(BackendEvent::AgentOutput {
        session_id: "from-b".into(),
        team_id: None,
        content: "y".into(),
        branch_id: None,
    });

    let got = received.lock().unwrap().clone();
    assert!(got.contains(&"from-a".to_string()), "got {got:?}");
    assert!(got.contains(&"from-b".to_string()), "got {got:?}");
}

/// 汇聚点优先于 handle：同时配置两者时只投递一次（不双投）。
#[tokio::test]
async fn event_sink_takes_precedence_over_handle_without_double_delivery() {
    let (bridge, handle) = crate::create_bridge();
    let agent = test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    }));
    agent.attach(Arc::new(handle));
    let sink_calls = Arc::new(AtomicUsize::new(0));
    let sink: EventSink = {
        let sink_calls = sink_calls.clone();
        Arc::new(move |_event: BackendEvent| {
            sink_calls.fetch_add(1, Ordering::SeqCst);
        })
    };
    agent.attach_event_sink(sink);

    agent.emit(BackendEvent::AgentOutput {
        session_id: "s".into(),
        team_id: None,
        content: "x".into(),
        branch_id: None,
    });

    assert_eq!(sink_calls.load(Ordering::SeqCst), 1);
    let mut rx = bridge.event_rx.lock().await;
    assert!(
        rx.try_recv().is_err(),
        "sink must suppress the legacy handle path (no double delivery)"
    );
}

#[tokio::test]
async fn emitted_events_are_recorded_into_the_display_timeline() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "Hello!".into(),
    });
    let agent = test_agent(provider);
    let key = SessionKey::local_tui();
    let session = agent.trunk.get_or_create(&key, "user".into(), None);
    // Adapter flow: inbound message event, then branch execution, then output.
    agent.emit(BackendEvent::MessageReceived {
        session_id: session.id.clone(),
        adapter_name: "local".into(),
        platform: "local".into(),
        user_id: "local_user".into(),
        user_name: "local user".into(),
        channel: "direct".into(),
        group_name: None,
        content: "hi".into(),
        images: vec![],
        timestamp: 1700000000,
        received_at_ms: 1700000000123,
        message_sequence: 1,
        team_id: None,
    });
    let reply = agent.process_message(&session, "hi").await.unwrap();
    agent.emit(BackendEvent::AgentOutput {
        session_id: session.id.clone(),
        content: reply.clone(),
        branch_id: None,
        team_id: None,
    });
    assert_eq!(reply, "Hello!");

    let timeline = agent.trunk.timeline_snapshot().unwrap_or_default();
    // user entry + backend reply entry.
    assert_eq!(timeline.len(), 2);
    assert_eq!(timeline[0].kind, "user");
    assert_eq!(timeline[0].content, "hi");
    assert!(timeline[0].source.is_some(), "source provenance recorded");
    assert_eq!(timeline[1].kind, "backend");
    assert_eq!(timeline[1].content, "Hello!");
}

#[tokio::test]
async fn timeline_records_tools_and_attaches_outcomes() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool {
        name: "mock_tool",
        result: "found".into(),
    }));
    let agent = Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    );
    let call = ToolCall {
        id: "c1".into(),
        name: "mock_tool".into(),
        arguments: r#"{"query":"x","api_key":"secret-1"}"#.into(),
    };
    let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
    assert_eq!(result.text, "found");

    let timeline = agent.trunk.timeline_snapshot().unwrap_or_default();
    assert_eq!(timeline.len(), 1);
    let tool = timeline[0].tool.as_ref().expect("tool entry");
    assert_eq!(tool.name, "mock_tool");
    assert!(tool.output.is_some(), "outcome attached");
    assert!(!tool.input.contains("secret-1"), "secrets redacted");
    assert!(tool.input.contains("[已隐藏]"), "redaction marker shown");
    assert!(!tool.failed);
}

#[tokio::test]
async fn timeline_associates_reasoning_with_the_final_output() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "done".into(),
    });
    let agent = test_agent(provider);
    let session_id = "local:tui::one";
    let branch_id = "branch-abc";
    agent.emit(BackendEvent::AgentReasoning {
        session_id: session_id.into(),
        team_id: None,
        branch_id: branch_id.into(),
        content: "先查资料".into(),
    });
    agent.emit(BackendEvent::ReplyBranchCompleted {
        session_id: session_id.into(),
        branch_id: branch_id.into(),
        message_sequence: 1,
        success: true,
        cancelled: false,
        completed_at_ms: 1000,
    });
    agent.emit(BackendEvent::AgentOutput {
        session_id: session_id.into(),
        content: "done".into(),
        branch_id: Some(branch_id.into()),
        team_id: None,
    });
    let timeline = agent.trunk.timeline_snapshot().unwrap_or_default();
    // 新语义：推理按到达顺序落为独立 reasoning 条目，backend 输出不再附加。
    assert_eq!(timeline.len(), 2);
    assert_eq!(timeline[0].kind, "reasoning");
    assert_eq!(timeline[0].content, "先查资料");
    assert_eq!(timeline[1].kind, "backend");
    assert_eq!(timeline[1].content, "done");
    assert!(
        timeline[1].reasoning.is_none(),
        "reasoning is a standalone entry"
    );
}

#[tokio::test]
async fn agent_replies_and_remembers() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "Hello!".into(),
    });
    let agent = test_agent(provider.clone());
    let key = SessionKey::local_tui();
    let session = agent.trunk.get_or_create(&key, "user".into(), None);
    let reply = agent.process_message(&session, "hi").await.unwrap();
    assert_eq!(reply, "Hello!");
    assert_eq!(session.history.lock().await.len(), 2);
    let _ = agent.process_message(&session, "again").await.unwrap();
    assert_eq!(session.history.lock().await.len(), 4);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}

/// 并行多会话模式：同一会话的多个 turn 并发进入模型，按请求序号合并。
#[tokio::test]
async fn parallel_mode_runs_turns_concurrently_and_merges_by_request_sequence() {
    let entered = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = Arc::new(ConcurrentProvider {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let agent = Arc::new(test_agent(provider));
    agent
        .set_loop_mode_for_test(echo_defs::LoopMode::Parallel)
        .await;
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    let first =
        r#"<backend_message_hook>{"message_sequence":1,"content":"first"}</backend_message_hook>"#;
    let second =
        r#"<backend_message_hook>{"message_sequence":2,"content":"second"}</backend_message_hook>"#;

    let first_task = {
        let agent = Arc::clone(&agent);
        let session = session.clone();
        tokio::spawn(async move { agent.process_message(&session, first).await })
    };
    let second_task = {
        let agent = Arc::clone(&agent);
        let session = session.clone();
        tokio::spawn(async move { agent.process_message(&session, second).await })
    };

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while entered.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both branches should enter the provider concurrently");
    release.add_permits(2);
    first_task.await.unwrap().unwrap();
    second_task.await.unwrap().unwrap();

    let history = session.history.lock().await;
    assert_eq!(history.len(), 4);
    assert_eq!(structured_message_sequence(&history[0].content), Some(1));
    assert_eq!(history[1].content, "reply-1");
    assert_eq!(structured_message_sequence(&history[2].content), Some(2));
    assert_eq!(history[3].content, "reply-2");
}

/// 单会话模式隐藏并行分支工具；并行模式恢复（工具 schema 与提示词同源）。
#[tokio::test]
async fn spawn_parallel_task_is_hidden_in_single_mode() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = Arc::new(test_agent(provider));
    assert!(!agent.allows_dynamic_tool("spawn_parallel_task"));
    agent
        .set_loop_mode_for_test(echo_defs::LoopMode::Parallel)
        .await;
    assert!(agent.allows_dynamic_tool("spawn_parallel_task"));
}

/// 单会话模式（默认）：同一会话的 turn 串行排队——第二个 turn 拿到的是
/// 第一轮结束后的上下文（能看到 reply-1），且不会并发进入模型。
#[tokio::test]
async fn single_mode_serialises_turns_and_second_turn_sees_the_first_reply() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(SnapshotProvider {
        calls: Arc::clone(&calls),
        seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        gate: Arc::new(tokio::sync::Semaphore::new(0)),
    });
    let agent = Arc::new(test_agent(provider.clone()));
    // 默认即单会话：不做任何配置。
    assert_eq!(agent.loop_mode(), echo_defs::LoopMode::Single);
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    let first =
        r#"<backend_message_hook>{"message_sequence":1,"content":"first"}</backend_message_hook>"#;
    let second =
        r#"<backend_message_hook>{"message_sequence":2,"content":"second"}</backend_message_hook>"#;

    let first_task = {
        let agent = Arc::clone(&agent);
        let session = session.clone();
        tokio::spawn(async move { agent.process_message(&session, first).await })
    };
    // 等第一轮真的进入模型（持有队列闸门），再投第二个输入。
    provider.wait_entered().await;
    let second_task = {
        let agent = Arc::clone(&agent);
        let session = session.clone();
        tokio::spawn(async move { agent.process_message(&session, second).await })
    };
    // 第二个 turn 必须排队：闸门仍被第一轮持有，模型调用数不增加。
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "second turn must wait");
    assert_eq!(
        agent.active_inbound_turn_count(),
        2,
        "queued turn is cancellable"
    );

    provider.release();
    first_task.await.unwrap().unwrap();
    second_task.await.unwrap().unwrap();

    let seen = provider.seen.lock().expect("seen poisoned").clone();
    assert_eq!(seen.len(), 2, "both turns ran");
    assert!(
        seen[1].iter().any(|content| content == "reply-1"),
        "the queued turn must see the first reply: {:?}",
        seen[1]
    );
    let history = session.history.lock().await;
    assert_eq!(history.len(), 4);
    assert_eq!(structured_message_sequence(&history[0].content), Some(1));
    assert_eq!(history[1].content, "reply-1");
    assert_eq!(structured_message_sequence(&history[2].content), Some(2));
    assert_eq!(history[3].content, "reply-2");
}

/// annotate_team 覆盖（2026-10 巡检）：带 team_id 字段的变体被标注；
/// 无 team_id 字段的变体原样透传（不 panic、不变形）。
#[test]
fn annotate_team_stamps_covered_variants() {
    use crate::event::BackendEvent;
    let stamped = Agent::annotate_team_for(
        BackendEvent::AgentThinking {
            session_id: "local:tui::local_user".into(),
            team_id: None,
        },
        Some("EchoCode".into()),
    );
    match stamped {
        BackendEvent::AgentThinking { team_id, .. } => {
            assert_eq!(
                team_id.as_deref(),
                Some("EchoCode"),
                "AgentThinking 应被标注"
            )
        }
        other => panic!("变体保持: {other:?}"),
    }

    // 无 team_id 字段的变体：原样透传。
    let passthrough = Agent::annotate_team_for(
        BackendEvent::FederationInvite {
            invite: "echofed://x#t".into(),
        },
        Some("EchoCode".into()),
    );
    match passthrough {
        BackendEvent::FederationInvite { invite } => {
            assert_eq!(invite, "echofed://x#t")
        }
        other => panic!("变体保持: {other:?}"),
    }

    // team 为 None（__core 服务代理）时：显式置 None（与 emit 同口径）。
    let none_team = Agent::annotate_team_for(
        BackendEvent::ToolResult {
            session_id: "s".into(),
            team_id: Some("stale".into()),
            tool_name: "t".into(),
            result: "r".into(),
            tool_call_id: String::new(),
            timed_out: false,
            branch_id: String::new(),
            elapsed_ms: None,
        },
        None,
    );
    match none_team {
        BackendEvent::ToolResult { team_id, .. } => {
            assert_eq!(team_id, None, "None team 覆盖旧值")
        }
        other => panic!("变体保持: {other:?}"),
    }
}

#[tokio::test]
async fn single_mode_runs_cross_session_turns_in_parallel() {
    // 2026-10 修复回归：Single 模式的 turn 闸门必须**按会话**——此前是
    // agent 全局共享，跨会话也串行（local:tui 的长任务阻塞 QQ 回复）。
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(SnapshotProvider {
        calls: Arc::clone(&calls),
        seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        gate: Arc::new(tokio::sync::Semaphore::new(0)),
    });
    let agent = Arc::new(test_agent(provider.clone()));
    assert_eq!(agent.loop_mode(), echo_defs::LoopMode::Single);
    let session_a = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    let session_b = agent.trunk.get_or_create(
        &SessionKey::local_workspace("other-ws"),
        "user".into(),
        None,
    );
    let first =
        r#"<backend_message_hook>{"message_sequence":1,"content":"first"}</backend_message_hook>"#;
    let other =
        r#"<backend_message_hook>{"message_sequence":1,"content":"other"}</backend_message_hook>"#;

    let first_task = {
        let agent = Arc::clone(&agent);
        let session = session_a.clone();
        tokio::spawn(async move { agent.process_message(&session, first).await })
    };
    // 第一轮（A 会话）进入模型并挂住闸门。
    provider.wait_entered().await;
    // B 会话投递：**不同会话不排队**——第二个模型调用应直接进入。
    let other_task = {
        let agent = Arc::clone(&agent);
        let session = session_b.clone();
        tokio::spawn(async move { agent.process_message(&session, other).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cross-session turn must not wait for the first session's gate");

    provider.release();
    first_task.await.unwrap().unwrap();
    other_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn request_trunk_timeline_returns_the_persisted_history() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = Arc::new(test_agent(provider));
    // 去主智能体后会话命令必须带 team_id：本 agent 视为 "t"。
    agent.set_team_id(Some("t".into()));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));
    agent.emit(BackendEvent::MessageReceived {
        session_id: "local:tui::local_user".into(),
        adapter_name: "local".into(),
        platform: "local".into(),
        user_id: "local_user".into(),
        user_name: "local user".into(),
        channel: "direct".into(),
        group_name: None,
        content: "你好".into(),
        images: vec![],
        timestamp: 1700000000,
        received_at_ms: 1700000000123,
        message_sequence: 1,
        team_id: None,
    });

    agent
        .apply_command(BackendCommand::RequestTrunkTimeline {
            team_id: Some("t".into()),
            since_seq: 0,
        })
        .await;
    let mut events = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        events.push(event);
    }
    let timeline = events
        .into_iter()
        .find_map(|event| match event {
            BackendEvent::TrunkTimeline { messages, .. } => Some(messages),
            _ => None,
        })
        .expect("TrunkTimeline event emitted");
    assert_eq!(timeline.len(), 1);
    assert_eq!(timeline[0].kind, "user");
    assert_eq!(timeline[0].content, "你好");
}

/// 去主智能体：会话类命令缺少 team_id 时必须明确报错（无默认人格兜底）。
#[tokio::test]
async fn session_commands_require_explicit_team_id() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = Arc::new(test_agent(provider));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    for cmd in [
        BackendCommand::RequestTrunkTimeline {
            team_id: None,
            since_seq: 0,
        },
        BackendCommand::RequestContext {
            team_id: None,
            session_id: None,
        },
        BackendCommand::ClearHistory { team_id: None },
    ] {
        agent.apply_command(cmd).await;
    }

    let mut errors = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        if let BackendEvent::Error { message, .. } = event {
            errors.push(message);
        }
    }
    assert_eq!(errors.len(), 3, "each command must error: {errors:?}");
    assert!(
        errors.iter().all(|m| m.contains("team_id")),
        "errors must explain the missing team_id: {errors:?}"
    );
}

#[tokio::test]
async fn update_api_config_persists() {
    let tmp = std::env::temp_dir().join(format!("echo-agent-cfg-{}.toml", uuid::Uuid::new_v4()));
    std::fs::write(
        &tmp,
        "[server]\nx = 1\n\n[agent]\nprovider = \"openai\"\nmodel = \"old-model\"\n",
    )
    .unwrap();
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = test_agent(provider);
    agent.set_config_path(tmp.clone());

    agent
        .apply_command(BackendCommand::UpdateApiConfig {
            name: String::new(),
            provider: "anthropic".into(),
            model: "claude-x".into(),
            base_url: "http://example.test".into(),
            api_key: "sk-test-123".into(),
            thinking: None,
            reasoning_effort: None,
        })
        .await;

    let cfg = agent.api_config().await;
    assert_eq!(cfg.provider, "anthropic");
    assert_eq!(cfg.model, "claude-x");
    assert_eq!(cfg.base_url, "http://example.test");
    assert_eq!(cfg.effective_api_key(), "sk-test-123");
    assert!(cfg.active_api.is_empty());

    let content = std::fs::read_to_string(&tmp).unwrap();
    assert!(content.contains("[server]\nx = 1"));
    assert!(content.contains("provider = \"anthropic\""));
    assert!(content.contains("model = \"claude-x\""));
    assert!(content.contains("base_url = \"http://example.test\""));
    assert!(content.contains("api_key = \"sk-test-123\""));
    assert!(!content.contains("old-model"));
    std::fs::remove_file(&tmp).ok();
}

#[tokio::test]
async fn persist_config_keeps_on_disk_api_key() {
    let tmp = std::env::temp_dir().join(format!("echo-agent-cfg-{}.toml", uuid::Uuid::new_v4()));
    std::fs::write(
        &tmp,
        "[agent]\nprovider = \"openai\"\napi_key = \"sk-on-disk\"\n",
    )
    .unwrap();
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = test_agent(provider);
    agent.set_config_path(tmp.clone());

    agent
        .apply_command(BackendCommand::UpdateApiConfig {
            name: String::new(),
            provider: "anthropic".into(),
            model: "claude-x".into(),
            base_url: "http://example.test".into(),
            api_key: String::new(),
            thinking: None,
            reasoning_effort: None,
        })
        .await;

    let content = std::fs::read_to_string(&tmp).unwrap();
    assert!(content.contains("provider = \"anthropic\""));
    assert!(
        content.contains("api_key = \"sk-on-disk\""),
        "unexpected: {content}"
    );
    std::fs::remove_file(&tmp).ok();
}

/// 人格名单由 AgentManager 独立落盘；人格快照里的副本可能过期，
/// `persist_config` 必须以磁盘为准（否则无关保存会写回旧名单）。
#[tokio::test]
async fn persist_config_preserves_on_disk_teams() {
    let tmp = std::env::temp_dir().join(format!("echo-agent-cfg-{}.toml", uuid::Uuid::new_v4()));
    std::fs::write(
        &tmp,
        "[agent]\nprovider = \"openai\"\n\n[agent.teams.fresh]\nname = \"Fresh\"\n",
    )
    .unwrap();
    let agent = test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    }));
    // 模拟"过期快照"：agent 内存里的 teams 是空的，磁盘上已有 fresh。
    agent.set_config_path(tmp.clone());

    agent
        .apply_command(BackendCommand::UpdateApiConfig {
            name: String::new(),
            provider: "anthropic".into(),
            model: "claude-x".into(),
            base_url: "http://example.test".into(),
            api_key: String::new(),
            thinking: None,
            reasoning_effort: None,
        })
        .await;

    let content = std::fs::read_to_string(&tmp).unwrap();
    assert!(content.contains("provider = \"anthropic\""));
    assert!(
        content.contains("[agent.teams.fresh]"),
        "on-disk teams must survive an unrelated persist: {content}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[tokio::test]
async fn update_api_config_new_profile_inherits_empty_fields() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = test_agent(provider);

    // 先建立顶层默认配置。
    agent
        .apply_command(BackendCommand::UpdateApiConfig {
            name: String::new(),
            provider: "deepseek".into(),
            model: "deepseek-x".into(),
            base_url: "http://ds.test".into(),
            api_key: "sk-top".into(),
            thinking: None,
            reasoning_effort: None,
        })
        .await;

    // 用全空字段创建命名 profile：应从当前生效配置继承，而不是写出
    // 残缺 profile（真实事故：TUI 只改 model 保存后 profile 缺字段）。
    agent
        .apply_command(BackendCommand::UpdateApiConfig {
            name: "backup".into(),
            provider: String::new(),
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            thinking: None,
            reasoning_effort: None,
        })
        .await;

    let cfg = agent.api_config().await;
    assert_eq!(cfg.active_api, "backup");
    let profile = cfg
        .api_profiles
        .iter()
        .find(|p| p.name == "backup")
        .expect("profile created");
    assert_eq!(profile.provider, "deepseek", "provider inherited");
    assert_eq!(profile.model, "deepseek-x", "model inherited");
    assert_eq!(profile.base_url, "http://ds.test", "base_url inherited");

    // 更新已有 profile 时保持"空 = 保留原值"语义。
    agent
        .apply_command(BackendCommand::UpdateApiConfig {
            name: "backup".into(),
            provider: String::new(),
            model: "deepseek-y".into(),
            base_url: String::new(),
            api_key: String::new(),
            thinking: None,
            reasoning_effort: None,
        })
        .await;
    let cfg = agent.api_config().await;
    let profile = cfg
        .api_profiles
        .iter()
        .find(|p| p.name == "backup")
        .expect("profile exists");
    assert_eq!(profile.provider, "deepseek", "provider kept, not inherited");
    assert_eq!(profile.model, "deepseek-y");
}

#[tokio::test]
async fn test_api_config_profile_empty_fields_fall_back() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = Arc::new(test_agent(provider));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    // 顶层配置指向一个必然连不上的地址（连接即刻被拒绝，不打真实网络）。
    agent
        .apply_command(BackendCommand::UpdateApiConfig {
            name: String::new(),
            provider: "deepseek".into(),
            model: "deepseek-x".into(),
            base_url: "http://127.0.0.1:1".into(),
            api_key: "sk-top".into(),
            thinking: None,
            reasoning_effort: None,
        })
        .await;

    // 手工注入一个字段残缺的 profile（provider/base_url 为空）。
    agent.config.write().await.api_profiles.push(ApiProfile {
        name: "broken".into(),
        provider: String::new(),
        model: "deepseek-x".into(),
        base_url: String::new(),
        api_key: String::new(),
        thinking: crate::config::ThinkingMode::Enabled,
        reasoning_effort: crate::config::ReasoningEffort::Max,
    });

    agent
        .apply_command(BackendCommand::TestApi {
            name: "broken".into(),
        })
        .await;

    let mut result = None;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
            if let BackendEvent::ApiTestResult { name, message, .. } = event {
                result = Some((name, message));
                break;
            }
        }
        if result.is_some() {
            break;
        }
    }
    let (name, message) = result.expect("ApiTestResult event emitted");
    assert_eq!(name, "broken");
    assert!(
        !message.contains("provider build failed"),
        "空字段应回退到顶层配置而不是构建失败: {message}"
    );
    // 回退后 provider 构建成功，失败只可能发生在请求阶段（连接拒绝）。
    assert!(
        message.contains("请求失败") || message.contains("请求超时"),
        "unexpected message: {message}"
    );
}

#[test]
fn data_plugin_stale_detects_manifest_diff() {
    use echo_plugin::{PluginDescriptor, PluginKind, PluginManifest};
    let manifest = PluginManifest::builtin(
        "acme.tools.weather",
        "Weather",
        "1.0.0",
        PluginKind::Tool,
        "weather",
        "old desc",
    );
    let desc = PluginDescriptor::from(&manifest);

    // 完全一致：不 stale
    assert!(!super::data_plugin_stale(&desc, &manifest));

    // version 变化 → stale
    let mut v2 = manifest.clone();
    v2.version = "1.1.0".into();
    assert!(super::data_plugin_stale(&desc, &v2));

    // description 变化 → stale
    let mut d2 = manifest.clone();
    d2.description = "new desc".into();
    assert!(super::data_plugin_stale(&desc, &d2));

    // 包归属变化（声明 package）→ stale
    let p2 = manifest.clone().with_package("acme.weather-suite");
    assert!(super::data_plugin_stale(&desc, &p2));

    // 未声明 package 时 package_id = 插件 id，不算 stale
    let same = manifest.clone();
    assert!(!super::data_plugin_stale(&desc, &same));
}

#[test]
fn invalid_tool_arguments_flags_missing_required() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"command": {"type": "string"}},
        "required": ["command"],
    });
    // 空对象：缺 command
    let message = invalid_tool_arguments("bash", "{}", &serde_json::json!({}), &schema)
        .expect("missing required flagged");
    assert!(message.contains("缺少必需参数 command"), "{message}");
    assert!(message.contains("你发送的参数: {}"), "{message}");
    assert!(message.contains("schema"), "{message}");
    // null 值同样算缺失
    assert!(
        invalid_tool_arguments("bash", "{}", &serde_json::json!({"command": null}), &schema)
            .is_some()
    );
    // 参数不是对象：所有必需字段都缺失
    assert!(invalid_tool_arguments("bash", "[]", &serde_json::Value::Null, &schema).is_some());
    // 字段齐全（含空字符串——合法值，不算缺失）：通过
    assert!(
        invalid_tool_arguments("bash", "{}", &serde_json::json!({"command": ""}), &schema,)
            .is_none()
    );
    // schema 无 required：不预检
    let free = serde_json::json!({"type": "object", "properties": {}});
    assert!(invalid_tool_arguments("stub", "{}", &serde_json::json!({}), &free).is_none());
}

#[tokio::test]
async fn run_tool_empty_arguments_gets_corrective_error() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let mut tools = ToolRegistry::new();
    crate::tool::builtin::coding::register_coding_tools(&mut tools, std::env::temp_dir());
    let agent = Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    );
    let call = ToolCall {
        id: "bad-1".into(),
        name: "bash".into(),
        arguments: "{}".into(),
    };
    let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
    assert!(
        result.text.contains("缺少必需参数 command"),
        "corrective message: {}",
        result.text
    );
    assert!(
        result.text.contains("你发送的参数: {}"),
        "echoes received args: {}",
        result.text
    );
    assert!(
        result.text.contains("schema"),
        "includes tool schema: {}",
        result.text
    );
}

#[tokio::test]
async fn run_tool_malformed_json_gets_clear_error() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let mut tools = ToolRegistry::new();
    crate::tool::builtin::coding::register_coding_tools(&mut tools, std::env::temp_dir());
    let agent = Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    );
    let call = ToolCall {
        id: "bad-2".into(),
        name: "bash".into(),
        arguments: "{bad json".into(),
    };
    let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
    assert!(
        result.text.contains("工具参数不是合法 JSON"),
        "clear JSON error: {}",
        result.text
    );
    assert!(
        result.text.contains("{bad json"),
        "echoes raw arguments: {}",
        result.text
    );
}

#[tokio::test]
async fn run_tool_valid_arguments_pass_preflight() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let mut tools = ToolRegistry::new();
    crate::tool::builtin::coding::register_coding_tools(&mut tools, std::env::temp_dir());
    let agent = Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    );
    let call = ToolCall {
        id: "ok-1".into(),
        name: "bash".into(),
        arguments: r#"{"command":"echo preflight-ok"}"#.into(),
    };
    let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
    assert!(
        result.text.contains("preflight-ok"),
        "valid call executes: {}",
        result.text
    );
    assert!(
        !result.text.contains("工具参数无效"),
        "no preflight error: {}",
        result.text
    );
}

#[tokio::test]
async fn skill_keyword_loads_instructions() {
    let mut reg = SkillRegistry::new();
    reg.register(crate::skill::Skill {
        metadata: crate::skill::SkillMetadata {
            name: "calc".into(),
            description: "d".into(),
            keywords: vec!["calc".into()],
            always: false,
            enabled: true,
            category: String::new(),
            package: None,
            system: false,
        },
        instructions: "use calculator tool".into(),
    });
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = Agent::new(
        provider,
        AgentConfig::default(),
        reg,
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    );
    let prompt = agent.build_system_prompt("help me calc 1+1").await;
    assert!(prompt.contains("Available skills"));
    assert!(prompt.contains("Triggered skill: calc"));
    assert!(prompt.contains("calculator"));
}

#[tokio::test]
async fn skill_directory_hot_reloads_updates_additions_and_deletions() {
    let dir = std::env::temp_dir().join(format!("echo-skill-reload-{}", uuid::Uuid::new_v4()));
    let style_dir = dir.join("style");
    std::fs::create_dir_all(&style_dir).unwrap();
    let style_path = style_dir.join("SKILL.md");
    std::fs::write(
        &style_path,
        "---\nname: style\ndescription: style\nmetadata:\n  always: true\n---\nold instructions",
    )
    .unwrap();

    let config = AgentConfig {
        skills_dir: dir.display().to_string(),
        ..Default::default()
    };
    let initial = SkillRegistry::discover(&config.skills_dir).unwrap();
    let skills_dir = config.skills_dir.clone();
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = Arc::new(Agent::new(
        provider,
        config,
        initial,
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    ));

    let first_prompt = agent.build_system_prompt("plain input").await;
    assert!(first_prompt.contains("old instructions"));

    // 手动触发重载（替代旧的定时扫描）：修改 + 新增技能后显式 reload。
    std::fs::write(
        &style_path,
        "---\nname: style\ndescription: updated\nmetadata:\n  always: true\n---\nnew instructions",
    )
    .unwrap();
    let extra_dir = dir.join("extra");
    std::fs::create_dir_all(&extra_dir).unwrap();
    std::fs::write(
        extra_dir.join("SKILL.md"),
        "---\nname: extra\ndescription: extra\nkeywords: [extra]\n---\nextra instructions",
    )
    .unwrap();
    assert!(
        agent.reload_skills(&skills_dir).await.unwrap(),
        "modified + added skills should report a reload"
    );

    let skills = agent.skills.lock().await;
    let updated = skills
        .get("style")
        .is_some_and(|skill| skill.instructions == "new instructions");
    let added = skills.get("extra").is_some();
    drop(skills);
    assert!(updated, "modified skill content should reload");
    assert!(added, "added skill should be discovered");

    let second_prompt = agent.build_system_prompt("plain input").await;
    assert!(second_prompt.contains("new instructions"));
    assert!(!second_prompt.contains("old instructions"));

    // 删除技能后再次手动重载。
    std::fs::remove_file(&style_path).unwrap();
    assert!(
        agent.reload_skills(&skills_dir).await.unwrap(),
        "deleted skill should report a reload"
    );
    assert!(agent.skills.lock().await.get("style").is_none());

    agent.shutdown().await;
    std::fs::remove_dir_all(&dir).ok();
}
// ── Tool calling loop ──────────────────────────────────────────────────

/// LLM provider that replays a fixed script of responses.
struct ScriptedProvider {
    script: tokio::sync::Mutex<std::collections::VecDeque<ChatResponse>>,
    calls: Arc<AtomicUsize>,
    requests: tokio::sync::Mutex<Vec<ChatRequest>>,
}

impl ScriptedProvider {
    fn new(script: Vec<ChatResponse>) -> Self {
        Self {
            script: tokio::sync::Mutex::new(script.into()),
            calls: Arc::new(AtomicUsize::new(0)),
            requests: tokio::sync::Mutex::new(Vec::new()),
        }
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }
    fn default_model(&self) -> &str {
        "mock-model"
    }
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.messages[0].role, crate::llm::ChatRole::System);
        self.requests.lock().await.push(request.clone());
        self.script
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| LlmError::Config("script exhausted".into()))
    }
    async fn chat_stream(
        &self,
        _request: &ChatRequest,
        _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<(), LlmError> {
        Ok(())
    }
}

/// A tool that returns a canned result.
struct MockTool {
    name: &'static str,
    result: String,
}

#[async_trait::async_trait]
impl Tool for MockTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "mock tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(&self, _arguments: serde_json::Value) -> Result<String, ToolError> {
        Ok(self.result.clone())
    }
}

fn tool_call(id: &str, name: &str) -> ChatResponse {
    ChatResponse {
        stop_reason: None,
        content: Some(format!("calling {name}")),
        reasoning_content: None,
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        }],
        usage: Usage::default(),
    }
}

#[tokio::test]
async fn tool_calling_loop_executes_and_finishes() {
    let script = vec![
        tool_call("call_1", "mock_tool"),
        ChatResponse {
            stop_reason: None,
            content: Some("final answer".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        },
    ];
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool {
        name: "mock_tool",
        result: "tool output".into(),
    }));

    let agent = Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    );
    let key = SessionKey::local_tui();
    let session = agent.trunk.get_or_create(&key, "user".into(), None);
    let reply = agent
        .process_message(&session, "use the tool")
        .await
        .unwrap();
    assert_eq!(reply, "final answer");
    assert_eq!(provider.call_count(), 2);
}

#[tokio::test]
async fn tool_loop_replays_reasoning_content_on_the_next_request() {
    let mut first = tool_call("call_1", "mock_tool");
    first.reasoning_content = Some("先调用工具，再根据结果回答".into());
    let provider = Arc::new(ScriptedProvider::new(vec![
        first,
        ChatResponse {
            stop_reason: None,
            content: Some("done".into()),
            reasoning_content: Some("工具返回成功".into()),
            tool_calls: Vec::new(),
            usage: Usage::default(),
        },
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool {
        name: "mock_tool",
        result: "ok".into(),
    }));
    let agent = Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    );
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);

    assert_eq!(
        agent.process_message(&session, "run").await.unwrap(),
        "done"
    );
    let requests = provider.requests.lock().await;
    let replayed = requests[1]
        .messages
        .iter()
        .find(|message| message.tool_calls.is_some())
        .expect("assistant tool-call message");
    assert_eq!(
        replayed.reasoning_content.as_deref(),
        Some("先调用工具，再根据结果回答")
    );
}

/// 回归：生产装配（组合根）把 spawn_subagent 注册进注册表并接线运行态后，
/// echo-loop 路径发送的工具名必须唯一——重复会让 API 拒绝整个请求
///（"Tool names must be unique"）。
#[tokio::test]
async fn echo_loop_tool_names_stay_unique_when_spawn_subagent_is_registered() {
    let provider = Arc::new(ScriptedProvider::new(vec![ChatResponse {
        stop_reason: None,
        content: Some("done".into()),
        reasoning_content: None,
        tool_calls: vec![],
        usage: Usage::default(),
    }]));
    let store = crate::subagent::SubagentStore::new();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(crate::subagent::SpawnSubagentTool::new(
        store.clone(),
        Arc::new(|_| {}),
    )));
    tools.set_package(
        crate::subagent::SPAWN_SUBAGENT_TOOL,
        crate::plugins::SUBAGENT_PLUGIN_ID,
    );
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    ));
    agent.attach_subagent_runtime(store);
    assert!(
        agent.allows_dynamic_tool(crate::subagent::SPAWN_SUBAGENT_TOOL),
        "插件允许 + 运行态已接线"
    );
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
    agent.set_use_echo_loop(true);

    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    assert_eq!(agent.process_message(&session, "hi").await.unwrap(), "done");

    let requests = provider.requests.lock().await;
    let names: Vec<String> = requests[0]
        .tools
        .as_ref()
        .expect("tools sent")
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(
        names.contains(&crate::subagent::SPAWN_SUBAGENT_TOOL.to_string()),
        "schema 应来自注册表: {names:?}"
    );
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(names.len(), unique.len(), "duplicate tool names: {names:?}");
}

/// echo-loop 路径的工具超时守卫（2026-09-26 补：此前该路径没有守卫，
/// 挂死的工具会永久拖住 turn——内置循环的外圈守卫在 echo-loop 上不生效）。
/// 超时后 turn 继续（notice 喂回模型），事件日志补记中断结果、不留悬空调用。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_loop_path_guards_hung_tools_with_timeout() {
    struct HungTool;
    #[async_trait::async_trait]
    impl crate::tool::Tool for HungTool {
        fn name(&self) -> &str {
            "hung_tool"
        }
        fn description(&self) -> &str {
            "sleeps far beyond the guard (test)"
        }
        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<String, crate::tool::ToolError> {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok("never".into())
        }
    }

    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatResponse {
            stop_reason: Some("tool_calls".into()),
            content: None,
            reasoning_content: None,
            tool_calls: vec![crate::llm::ToolCall {
                id: "call_hung_1".into(),
                name: "hung_tool".into(),
                arguments: "{}".into(),
            }],
            usage: Usage::default(),
        },
        ChatResponse {
            stop_reason: None,
            content: Some("done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        },
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HungTool));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig {
            // 1s 守卫：挂死工具（60s）必须被中止。
            tool_timeout_secs: Some(1),
            ..Default::default()
        },
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    ));
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
    agent.set_use_echo_loop(true);

    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    let started = std::time::Instant::now();
    let reply = agent
        .process_message(&session, "run the slow tool")
        .await
        .unwrap();
    assert_eq!(reply, "done", "turn must continue after a tool timeout");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "guard must abort the 60s tool long before it finishes: {:?}",
        started.elapsed()
    );

    // 模型可见的下一次请求里带超时 notice（turn 未中断）。
    let requests = provider.requests.lock().await;
    assert!(requests.len() >= 2, "expected a second model request");
    let saw_notice = requests[1].messages.iter().any(|message| {
        message.role == crate::llm::ChatRole::Tool && message.content.contains("timed out after")
    });
    assert!(saw_notice, "tool message must carry the timeout notice");

    // 事件日志成对：ToolCall 有对应的（中断）ToolResult，不留悬空调用。
    let paired = agent.trunk.event_log().iter().any(|event| {
        matches!(
            event,
            echo_session::SessionEvent::ToolResult(result)
                if result.tool_call_id == "call_hung_1"
                    && result.result.contains("timed out after")
        )
    });
    assert!(
        paired,
        "interrupted result must be recorded for log pairing"
    );
}

/// echo-loop 路径的提示词分层与内置循环一致（2026-09-30 修复回归）：
/// persona 系统技能与工作区会话注入在 echo-loop 路径同样生效——此前
/// `build_prompt_blocks` 传 `&[]`/None，两层在普通输入上静默丢失。
#[tokio::test]
async fn echo_loop_path_injects_persona_skills_and_workspace() {
    let mut skills = SkillRegistry::new();
    skills.register(crate::skill::Skill::direct(
        "persona-style",
        "人格语气规则",
        vec![],
        false,
        "",
        "PERSONA_SKILL_MARKER",
    ));
    let provider = Arc::new(ScriptedProvider::new(vec![ChatResponse {
        stop_reason: None,
        content: Some("done".into()),
        reasoning_content: None,
        tool_calls: vec![],
        usage: Usage::default(),
    }]));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        skills,
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    ));
    // persona 系统技能 + 工作区插件允许（空白名单 = 全部启用）。
    agent
        .apply_capabilities(&crate::config::TeamMember {
            system_skills: vec!["persona-style".into()],
            ..Default::default()
        })
        .await;
    let store = Arc::new(crate::workspace::WorkspaceStore::load(None));
    store
        .upsert(echo_protocol::WorkspaceSessionInfo {
            id: "proj".into(),
            name: "Proj".into(),
            description: String::new(),
            directories: vec!["/srv/proj".into()],
        })
        .unwrap();
    store.set_active(Some("proj".into())).unwrap();
    agent.set_workspace_store(store);

    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
    agent.set_use_echo_loop(true);

    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    assert_eq!(agent.process_message(&session, "hi").await.unwrap(), "done");

    let requests = provider.requests.lock().await;
    let system = requests[0].messages[0].content.clone();
    assert!(
        system.contains("PERSONA_SKILL_MARKER"),
        "persona 系统技能必须注入（echo-loop 路径）: {system}"
    );
    assert!(
        system.contains("# 当前工作区会话"),
        "工作区会话必须注入（echo-loop 路径）: {system}"
    );
}

/// echo-loop 路径 UI 事件对齐（2026-10 巡检）：与内置循环同屏同字段——
/// AgentThinking 起、LlmRequest/LlmResponse（经 runner 总线翻译）、
/// AgentReasoning（回调）、AgentCompleted 收；并写 last_prompt_blocks
/// （/context 视图）。
#[tokio::test]
async fn echo_loop_path_emits_panel_lifecycle_events() {
    let provider = Arc::new(ScriptedProvider::new(vec![ChatResponse {
        stop_reason: None,
        content: Some("done".into()),
        reasoning_content: Some("先想一想".into()),
        tool_calls: vec![],
        usage: Usage {
            prompt_tokens: 11,
            completion_tokens: 22,
        },
    }]));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    ));
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
    agent.set_use_echo_loop(true);
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);

    assert_eq!(agent.process_message(&session, "hi").await.unwrap(), "done");

    let mut events = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        events.push(event);
    }
    let has = |pred: &dyn Fn(&BackendEvent) -> bool| events.iter().any(pred);
    assert!(
        has(&|e| matches!(e, BackendEvent::AgentThinking { .. })),
        "AgentThinking 缺失（busy 不会启动）: {events:?}"
    );
    assert!(
        has(&|e| matches!(
            e,
            BackendEvent::LlmRequest { model, .. } if model == "mock-model"
        )),
        "LlmRequest 缺失（模型指示不显示）: {events:?}"
    );
    assert!(
        has(&|e| matches!(
            e,
            BackendEvent::LlmResponse {
                prompt_tokens: 11,
                completion_tokens: 22,
                ..
            }
        )),
        "LlmResponse 缺失（用量不显示）: {events:?}"
    );
    assert!(
        has(&|e| matches!(
            e,
            BackendEvent::AgentReasoning { content, .. } if content == "先想一想"
        )),
        "AgentReasoning 缺失: {events:?}"
    );
    assert!(
        has(&|e| matches!(e, BackendEvent::AgentCompleted { .. })),
        "AgentCompleted 缺失（busy 永不结束）: {events:?}"
    );
    // 顺序：Thinking 在 Completed 之前。
    let pos = |pred: &dyn Fn(&BackendEvent) -> bool| events.iter().position(pred);
    let thinking = pos(&|e| matches!(e, BackendEvent::AgentThinking { .. })).unwrap();
    let completed = pos(&|e| matches!(e, BackendEvent::AgentCompleted { .. })).unwrap();
    assert!(thinking < completed, "Thinking 应先于 Completed");
    // /context 视图数据（与内置循环同口径）。
    assert!(
        agent.last_prompt_blocks.lock().await.is_some(),
        "last_prompt_blocks 必须写入（/context 视图）"
    );
}

/// echo-loop 路径动态模型（2026-10 巡检）：runner 构造期固化改为每 turn
/// 解析——运行期 set_model 在下一 turn 的请求与 LlmRequest 事件中生效。
#[tokio::test]
async fn echo_loop_path_uses_active_model_per_turn() {
    let provider = Arc::new(ScriptedProvider::new(vec![ChatResponse {
        stop_reason: None,
        content: Some("done".into()),
        reasoning_content: None,
        tool_calls: vec![],
        usage: Usage::default(),
    }]));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    ));
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
    agent.set_use_echo_loop(true);
    agent.set_model("runtime-model".into()).await;
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);

    assert_eq!(agent.process_message(&session, "hi").await.unwrap(), "done");
    let requests = provider.requests.lock().await;
    assert_eq!(
        requests[0].model, "runtime-model",
        "echo 路径必须用运行期模型（此前构造期固化）"
    );
}

/// echo-loop 路径工具图片透传（2026-10 巡检）：execute_rich 产出的图片
/// 必须进入下一轮请求的工具消息（此前只取 text——多模态链路上丢失）。
#[tokio::test]
async fn echo_loop_path_carries_tool_images_into_next_request() {
    struct ImageTool;
    #[async_trait::async_trait]
    impl Tool for ImageTool {
        fn name(&self) -> &str {
            "image_tool"
        }
        fn description(&self) -> &str {
            "returns an image (test)"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _arguments: serde_json::Value) -> Result<String, ToolError> {
            Ok("should not be used".into())
        }
        async fn execute_rich(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<crate::tool::ToolResult, ToolError> {
            Ok(crate::tool::ToolResult::with_images(
                "see image",
                vec!["data:image/png;base64,AAAA".into()],
            ))
        }
    }

    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatResponse {
            stop_reason: Some("tool_calls".into()),
            content: None,
            reasoning_content: None,
            tool_calls: vec![crate::llm::ToolCall {
                id: "call_img_1".into(),
                name: "image_tool".into(),
                arguments: "{}".into(),
            }],
            usage: Usage::default(),
        },
        ChatResponse {
            stop_reason: None,
            content: Some("done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        },
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ImageTool));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    ));
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
    agent.set_use_echo_loop(true);
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);

    assert_eq!(
        agent.process_message(&session, "run").await.unwrap(),
        "done"
    );
    let requests = provider.requests.lock().await;
    let tool_message = requests[1]
        .messages
        .iter()
        .find(|message| message.role == crate::llm::ChatRole::Tool)
        .expect("second request must carry the tool message");
    assert_eq!(
        tool_message.images,
        vec!["data:image/png;base64,AAAA".to_string()],
        "工具产出的图片必须透传（此前只取 text）"
    );
}

/// echo-loop 路径在途工具取消（2026-10 巡检）：turn 取消后挂起的工具执行
/// 必须立即中止（此前取消延迟至工具自然结束），事件日志补记中断结果
/// 保持成对。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_loop_path_cancels_in_flight_tool() {
    struct GatedHungTool {
        started: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl Tool for GatedHungTool {
        fn name(&self) -> &str {
            "hung_tool"
        }
        fn description(&self) -> &str {
            "sleeps far beyond the guard (test)"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _arguments: serde_json::Value) -> Result<String, ToolError> {
            self.started.notify_one();
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok("never".into())
        }
    }

    let started = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(ScriptedProvider::new(vec![ChatResponse {
        stop_reason: Some("tool_calls".into()),
        content: None,
        reasoning_content: None,
        tool_calls: vec![crate::llm::ToolCall {
            id: "call_hung_cancel".into(),
            name: "hung_tool".into(),
            arguments: "{}".into(),
        }],
        usage: Usage::default(),
    }]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GatedHungTool {
        started: started.clone(),
    }));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    ));
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
    agent.set_use_echo_loop(true);
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    let session_id = session.id.clone();

    let turn = {
        let agent = Arc::clone(&agent);
        let session = session.clone();
        tokio::spawn(async move { agent.process_message(&session, "run").await })
    };
    // 等工具真的开始执行，再取消。
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .expect("tool should start");
    let started_at = std::time::Instant::now();
    let cancelled = agent.cancel_inbound_turns(&session_id, true);
    assert_eq!(cancelled, 1, "one active turn to cancel");
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("turn must end promptly after cancel (in-flight tool aborted)")
        .unwrap();
    assert!(result.is_err(), "cancelled turn returns error");
    assert!(
        result.unwrap_err().to_string().contains("cancel"),
        "error must be TURN_CANCELLED"
    );
    assert!(
        started_at.elapsed() < std::time::Duration::from_secs(10),
        "cancel must not wait for the 60s tool"
    );
    // 事件日志成对：ToolCall 对应的（中断）ToolResult 已补记。
    let log = agent.trunk.event_log();
    let interrupted = log.iter().any(|event| {
        matches!(
            event,
            echo_session::SessionEvent::ToolResult(result)
                if result.result.contains("cancelled")
        )
    });
    assert!(interrupted, "interrupted tool result must be recorded");
}

#[test]
fn sequence_parsing_only_accepts_structured_markers() {
    let hook = r#"<qq_message_hook>
{"channel":{"type":"private"},"message_sequence":7,"content":"hi"}
</qq_message_hook>"#;
    assert_eq!(structured_message_sequence(hook), Some(7));
    let backend = r#"<backend_message_hook>{"message_sequence":3}</backend_message_hook>"#;
    assert_eq!(structured_message_sequence(backend), Some(3));
    let timer = r#"<timer_event>{"message_sequence":9}</timer_event>"#;
    assert_eq!(structured_message_sequence(timer), Some(9));

    // Ordinary conversation containing braces must NOT be parsed.
    assert_eq!(
        structured_message_sequence("请对比 {\"a\":1} 和 {\"b\":2}"),
        None
    );
    assert_eq!(
        structured_message_sequence("message_sequence: 42 in plain text"),
        None
    );
    assert_eq!(structured_message_sequence("const x = {n: 1};"), None);
    // The marker must be at the very start.
    assert_eq!(
        structured_message_sequence(
            "note: <qq_message_hook>{\"message_sequence\":1}</qq_message_hook>"
        ),
        None
    );
}

#[test]
fn extract_input_images_reads_hook_payload() {
    let hook = r#"<qq_message_hook>
    {"channel":{"type":"private"},"message_sequence":7,"content":"hi","images":["https://example.com/a.png","data:image/png;base64,AAA"]}
    </qq_message_hook>"#;
    assert_eq!(
        extract_input_images(hook),
        vec![
            "https://example.com/a.png".to_string(),
            "data:image/png;base64,AAA".to_string()
        ]
    );
    // 无图 hook 返回空
    let plain =
        r#"<qq_message_hook>{"message_sequence":1,"content":"hi","images":[]}</qq_message_hook>"#;
    assert!(extract_input_images(plain).is_empty());
    // 普通文本不解析
    assert!(extract_input_images("看一下这张图片 https://example.com/a.png").is_empty());
}

#[test]
fn inbound_image_hook_yields_a_compact_model_request() {
    // 端到端护栏：大图入站后，模型请求里不允许再出现 base64 —— 图片只走
    // image 块，文本里是占位符（1.3MB 截图 ≈ 125 万文本 token 的根因）。
    let payload = "A".repeat(6000);
    let uri = format!("data:image/png;base64,{payload}");
    let hook = format!(
        "<backend_message_hook>{{\"message_sequence\":15,\"content\":\"看截图\",\"images\":[\"{uri}\"]}}</backend_message_hook>"
    );
    let images = extract_input_images(&hook);
    assert_eq!(images, vec![uri.clone()]);

    let log = vec![echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: None,
            content: hook.clone(),
            timestamp: 0,
            message_sequence: Some(15),
            source: None,
            images,
        },
    )];
    let messages = echo_session::derive::derive_messages(&log, 800_000);
    assert_eq!(messages.len(), 1);
    assert!(
        !messages[0].content.contains("AAAA"),
        "base64 must not reach the model text"
    );
    assert!(messages[0].content.contains("[图片#1]"));
    assert_eq!(
        messages[0].images.len(),
        1,
        "image block still carries the payload"
    );
    // 单条消息的估算成本从 6000+ 降到占位符级别。
    assert!(echo_defs::token::estimate_message_tokens(&messages[0]) < 500);
}

#[test]
fn user_message_event_roundtrips_images() {
    let hook = r#"<qq_message_hook>{"message_sequence":3,"content":"看图","images":["https://example.com/a.png"]}</qq_message_hook>"#;
    let event = echo_session::event::UserMessage {
        session: None,
        content: hook.into(),
        timestamp: 0,
        message_sequence: Some(3),
        source: None,
        images: extract_input_images(hook),
    };
    let json =
        serde_json::to_string(&echo_session::SessionEvent::UserMessage(event.clone())).unwrap();
    let back: echo_session::SessionEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(back, echo_session::SessionEvent::UserMessage(event));
}

#[test]
fn probe_config_resolves_active_profile_and_named_profiles() {
    use crate::config::ApiProfile;
    let mut cfg = crate::config::AgentConfig {
        provider: "deepseek".into(),
        model: "deepseek-v4-flash".into(),
        base_url: "https://api.deepseek.com/anthropic".into(),
        api_key: "sk-xxx".into(),
        active_api: "deepseek".into(),
        ..Default::default()
    };
    cfg.api_profiles.push(ApiProfile {
        name: "deepseek".into(),
        provider: "deepseek".into(),
        model: "deepseek-v4-flash".into(),
        base_url: "https://api.deepseek.com/anthropic".into(),
        api_key: "sk-xxx".into(),
        thinking: crate::config::ThinkingMode::Enabled,
        reasoning_effort: crate::config::ReasoningEffort::Max,
    });
    cfg.api_profiles.push(ApiProfile {
        name: "openai".into(),
        provider: "openai".into(),
        model: "gpt-4o".into(),
        base_url: "https://api.openai.com/v1".into(),
        api_key: String::new(),
        thinking: crate::config::ThinkingMode::Disabled,
        reasoning_effort: crate::config::ReasoningEffort::Low,
    });

    // 默认配置（name=''）：合并激活 profile → provider 非空。
    let probe = super::api_admin::resolve_probe_config(&cfg, "").expect("default probe resolves");
    assert_eq!(probe.provider, "deepseek");
    assert!(!probe.api_key.is_empty(), "api key resolved");

    // 指定 profile：使用该 profile 的值。
    let probe =
        super::api_admin::resolve_probe_config(&cfg, "openai").expect("openai probe resolves");
    assert_eq!(probe.provider, "openai");
    assert_eq!(probe.model, "gpt-4o");

    // 不存在的 profile → 明确报错。
    let err = super::api_admin::resolve_probe_config(&cfg, "nope").unwrap_err();
    assert!(err.contains("profile not found"));
}

#[test]
fn deepseek_balance_endpoint_maps_official_variants() {
    // Anthropic / OpenAI / beta / 裸域 → 统一 /user/balance
    assert_eq!(
        super::api_admin::deepseek_balance_endpoint("https://api.deepseek.com/anthropic")
            .as_deref(),
        Some("https://api.deepseek.com/user/balance")
    );
    assert_eq!(
        super::api_admin::deepseek_balance_endpoint("https://api.deepseek.com/v1").as_deref(),
        Some("https://api.deepseek.com/user/balance")
    );
    assert_eq!(
        super::api_admin::deepseek_balance_endpoint("https://api.deepseek.com").as_deref(),
        Some("https://api.deepseek.com/user/balance")
    );
    assert_eq!(
        super::api_admin::deepseek_balance_endpoint("https://api.deepseek.com/").as_deref(),
        Some("https://api.deepseek.com/user/balance")
    );
    // 非 DeepSeek 域 → None（前端不展示余额入口）
    assert!(super::api_admin::deepseek_balance_endpoint("https://uuapi.io/v1").is_none());
    assert!(super::api_admin::deepseek_balance_endpoint("https://api.openai.com/v1").is_none());
}

#[test]
fn probe_config_reports_missing_provider() {
    let cfg = crate::config::AgentConfig::default();
    let err = super::api_admin::resolve_probe_config(&cfg, "").unwrap_err();
    assert!(
        err.contains("provider"),
        "error must mention provider: {err}"
    );
}

#[tokio::test]
async fn max_tool_iterations_returns_error() {
    let cfg = AgentConfig {
        max_tool_iterations: 2,
        ..Default::default()
    };
    let script = vec![
        tool_call("c1", "mock_tool"),
        tool_call("c2", "mock_tool"),
        tool_call("c3", "mock_tool"),
    ];
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool {
        name: "mock_tool",
        result: "x".into(),
    }));

    let agent = Agent::new(
        provider,
        cfg,
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    );
    let key = SessionKey::local_tui();
    let session = agent.trunk.get_or_create(&key, "user".into(), None);
    let err = agent.process_message(&session, "loop").await.unwrap_err();
    assert!(err.to_string().contains("max tool iterations"));
}

#[tokio::test]
async fn tool_execution_failure_is_surfaced_to_llm() {
    let script = vec![
        tool_call("call_1", "missing_tool"),
        ChatResponse {
            stop_reason: None,
            content: Some("done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        },
    ];
    let provider = Arc::new(ScriptedProvider::new(script));
    let agent = Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    );
    let key = SessionKey::local_tui();
    let session = agent.trunk.get_or_create(&key, "user".into(), None);
    let reply = agent.process_message(&session, "hi").await.unwrap();
    assert_eq!(reply, "done");
}

// ── 工作区会话（workspace 插件）──

/// 工作区命令走 team 路由（去主智能体后不落"默认人格"兜底），
/// 且 save → list → active → git 的完整链路可用。
#[tokio::test]
async fn workspace_commands_lifecycle() {
    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    agent.set_team_id(Some("t".into()));
    let path = std::env::temp_dir().join(format!(
        "echo-workspace-agent-test-{}.json",
        std::process::id()
    ));
    std::fs::remove_file(&path).ok();
    agent.set_workspace_store(Arc::new(crate::workspace::WorkspaceStore::load(Some(
        path.clone(),
    ))));

    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    // 保存（空 id → 服务端生成）。
    agent
        .apply_command(BackendCommand::SaveWorkspaceSession {
            team_id: Some("t".into()),
            session: echo_protocol::WorkspaceSessionInfo {
                id: String::new(),
                name: "core".into(),
                description: String::new(),
                directories: vec![env!("CARGO_MANIFEST_DIR").into()],
            },
        })
        .await;
    // 激活。
    agent
        .apply_command(BackendCommand::ActivateWorkspaceSession {
            team_id: Some("t".into()),
            id: Some("core".into()),
        })
        .await;
    // 列表。
    agent
        .apply_command(BackendCommand::RequestWorkspaceSessions {
            team_id: Some("t".into()),
        })
        .await;
    // git 状态（真实仓库 checkout）。
    agent
        .apply_command(BackendCommand::RequestWorkspaceGitStatus {
            team_id: Some("t".into()),
            session_id: "core".into(),
        })
        .await;

    let mut events = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        events.push(event);
    }
    // 取最后一次列表快照（save → activate 都会推送列表）。
    let list = events
        .iter()
        .rev()
        .find_map(|e| match e {
            BackendEvent::WorkspaceSessions {
                sessions, active, ..
            } => Some((sessions.clone(), active.clone())),
            _ => None,
        })
        .expect("WorkspaceSessions emitted");
    assert_eq!(list.0.len(), 1);
    assert_eq!(list.0[0].id, "core");
    assert_eq!(list.1.as_deref(), Some("core"), "activation survives");

    let git = events
        .iter()
        .find_map(|e| match e {
            BackendEvent::WorkspaceGitStatus { directories, .. } => Some(directories.clone()),
            _ => None,
        })
        .expect("WorkspaceGitStatus emitted");
    assert_eq!(git.len(), 1);
    assert!(git[0].is_repo, "repo checkout detected: {git:?}");

    // 提示注入：激活后 build_prompt_blocks 含 workspace 区块。
    let store = agent.workspace_store().expect("store attached");
    let text = store.prompt_text().expect("active prompt text");
    assert!(text.contains("core"));
    assert!(text.contains(env!("CARGO_MANIFEST_DIR")));

    // 激活 = 进入项目对话：通道会话注册并广播 SessionUpdated。
    let channel = events
        .iter()
        .find_map(|e| match e {
            BackendEvent::SessionUpdated { session }
                if session.id == "local:workspace:core:local_user" =>
            {
                Some(session)
            }
            _ => None,
        })
        .expect("channel SessionUpdated emitted");
    assert_eq!(channel.nickname, "core");
    assert_eq!(channel.team_id.as_deref(), Some("t"));
    // 通道会话常驻 trunk registry（RequestState 会带上它，供切换器展示）。
    assert!(
        agent
            .trunk
            .all()
            .iter()
            .any(|s| s.id == "local:workspace:core:local_user"),
        "channel session registered"
    );

    std::fs::remove_file(&path).ok();
}

/// 文件浏览器：列出会话目录（拒绝越界路径，返回条目含目录与文件）。
#[tokio::test]
async fn workspace_files_command_lists_within_scope() {
    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    agent.set_team_id(Some("t".into()));
    let path = std::env::temp_dir().join(format!(
        "echo-workspace-files-test-{}.json",
        std::process::id()
    ));
    std::fs::remove_file(&path).ok();
    agent.set_workspace_store(Arc::new(crate::workspace::WorkspaceStore::load(Some(
        path.clone(),
    ))));

    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    let root = env!("CARGO_MANIFEST_DIR").to_string();
    agent
        .apply_command(BackendCommand::SaveWorkspaceSession {
            team_id: Some("t".into()),
            session: echo_protocol::WorkspaceSessionInfo {
                id: "core".into(),
                name: "core".into(),
                description: String::new(),
                directories: vec![root.clone().into()],
            },
        })
        .await;
    // 列目录（会话目录本身）。
    agent
        .apply_command(BackendCommand::RequestWorkspaceFiles {
            team_id: Some("t".into()),
            session_id: "core".into(),
            path: root.clone(),
        })
        .await;
    // 越界路径（/etc 不在会话目录内）→ 错误事件。
    agent
        .apply_command(BackendCommand::RequestWorkspaceFiles {
            team_id: Some("t".into()),
            session_id: "core".into(),
            path: "/etc".into(),
        })
        .await;

    let mut events = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        events.push(event);
    }
    let listings: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::WorkspaceFiles {
                path: p,
                entries,
                error,
                ..
            } => Some((p.clone(), entries.clone(), error.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(listings.len(), 2, "two listings expected");
    // 第一次：合法目录，条目非空且含 src（本仓库 checkout）。
    let (_, entries, error) = &listings[0];
    assert!(error.is_none(), "in-scope listing error: {error:?}");
    assert!(
        entries.iter().any(|e| e.name == "src" && e.is_dir),
        "src/ expected: {entries:?}"
    );
    // 第二次：越界拒绝。
    let (_, entries, error) = &listings[1];
    assert!(entries.is_empty());
    assert!(
        error
            .as_deref()
            .unwrap_or("")
            .contains("不在该会话的工作区目录内"),
        "unexpected: {error:?}"
    );

    std::fs::remove_file(&path).ok();
}

/// 通道广播唯一入口（store 变更钩子）：
/// - 重启恢复：装配前 store 已带 active → `set_workspace_store` 补注册通道；
/// - 模型侧 `workspace` 工具 `use`：与面板命令同一条广播路径
///   （`WorkspaceSessions` + 通道 `SessionUpdated`）。
#[tokio::test]
async fn workspace_channel_tool_use_broadcasts() {
    let store = Arc::new(crate::workspace::WorkspaceStore::load(None));
    for (id, name) in [("core", "Core"), ("docs", "Docs")] {
        store
            .upsert(echo_protocol::WorkspaceSessionInfo {
                id: id.into(),
                name: name.into(),
                description: String::new(),
                directories: vec!["/srv/x".into()],
            })
            .unwrap();
    }
    // 模拟持久化恢复：激活发生在进程装配之前。
    store.set_active(Some("core".into())).unwrap();

    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    agent.set_team_id(Some("t".into()));
    agent.set_workspace_store(store.clone());
    assert!(
        agent
            .trunk
            .all()
            .iter()
            .any(|s| s.id == "local:workspace:core:local_user"),
        "startup ensure registers the persisted active channel"
    );

    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    // 模型侧工具直接切换激活（不经命令路径）。
    let tool = crate::workspace::WorkspaceTool::new(store.clone());
    crate::tool::Tool::execute(&tool, serde_json::json!({"operation": "use", "id": "docs"}))
        .await
        .expect("tool use");

    let mut events = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        events.push(event);
    }
    let active = events
        .iter()
        .rev()
        .find_map(|e| match e {
            BackendEvent::WorkspaceSessions { active, .. } => Some(active.clone()),
            _ => None,
        })
        .expect("WorkspaceSessions broadcast on tool use");
    assert_eq!(active.as_deref(), Some("docs"));
    assert!(
        events.iter().any(|e| matches!(
            e,
            BackendEvent::SessionUpdated { session }
                if session.id == "local:workspace:docs:local_user"
        )),
        "tool use broadcasts the channel SessionUpdated"
    );
    assert!(
        agent
            .trunk
            .all()
            .iter()
            .any(|s| s.id == "local:workspace:docs:local_user"),
        "tool use registers the channel"
    );
}

/// workspace 插件门控：白名单外人格的工具包被禁用、提示词区块不注入；
/// 白名单内人格工具可见且激活会话时注入「工作区会话」区块。
#[tokio::test]
async fn workspace_plugin_gates_tool_and_prompt_block() {
    let mut tools = ToolRegistry::new();
    let store = Arc::new(crate::workspace::WorkspaceStore::load(None));
    store
        .upsert(echo_protocol::WorkspaceSessionInfo {
            id: "proj".into(),
            name: "Proj".into(),
            description: String::new(),
            directories: vec!["/srv/proj".into()],
        })
        .unwrap();
    store.set_active(Some("proj".into())).unwrap();
    tools.register(Arc::new(crate::workspace::WorkspaceTool::new(
        store.clone(),
    )));
    tools.set_package("workspace", crate::plugins::WORKSPACE_PLUGIN_ID);
    let agent = Arc::new(Agent::new(
        Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        }),
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    ));
    agent.set_workspace_store(store);

    // 白名单含 workspace → 工具可见 + 提示区块注入。
    agent
        .apply_capabilities(&crate::config::TeamMember {
            enabled_plugins: vec![
                crate::plugins::TOOLS_BUILTIN_PLUGIN_ID.into(),
                crate::plugins::WORKSPACE_PLUGIN_ID.into(),
            ],
            ..Default::default()
        })
        .await;
    let names: Vec<String> = agent
        .tools
        .definitions()
        .await
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(names.contains(&"workspace".to_string()), "tools: {names:?}");
    let blocks = agent.build_prompt_blocks().await;
    assert!(
        blocks.iter().any(|b| b.key == "workspace"),
        "workspace block injected"
    );
    assert!(blocks
        .iter()
        .find(|b| b.key == "workspace")
        .unwrap()
        .content
        .contains("/srv/proj"));

    // 白名单不含 workspace → 工具包禁用 + 区块消失。
    agent
        .apply_capabilities(&crate::config::TeamMember {
            enabled_plugins: vec![crate::plugins::TOOLS_BUILTIN_PLUGIN_ID.into()],
            ..Default::default()
        })
        .await;
    let names: Vec<String> = agent
        .tools
        .definitions()
        .await
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(
        !names.contains(&"workspace".to_string()),
        "tools: {names:?}"
    );
    let blocks = agent.build_prompt_blocks().await;
    assert!(
        !blocks.iter().any(|b| b.key == "workspace"),
        "workspace block hidden without the plugin"
    );
}

/// 缺 team_id 的工作区命令被拒绝（与其它会话类命令同一约定）。
#[tokio::test]
async fn workspace_commands_require_team_id() {
    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    agent.set_team_id(Some("t".into()));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));
    agent
        .apply_command(BackendCommand::RequestWorkspaceSessions { team_id: None })
        .await;
    let mut events = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        events.push(event);
    }
    let message = events
        .iter()
        .find_map(|e| match e {
            BackendEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("error emitted");
    assert!(message.contains("team_id"), "unexpected: {message}");
}

/// 端到端（演练口径）：bash 读取含密钥的文件 → 工具结果与会话日志零明文；
/// 参数本身含密钥时日志记占位符、执行仍按原始参数（功能不受影响）。
#[tokio::test]
async fn run_tool_redacts_secrets_from_real_bash_output_and_log() {
    use echo_defs::sanitize::Redactor;
    const KEY: &str = "sk-runtoole2eruntoole2e0001";
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let mut tools = ToolRegistry::new();
    crate::tool::builtin::coding::register_coding_tools(&mut tools, std::env::temp_dir());
    let agent = Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    );
    let redactor = Arc::new(echo_sanitize::RegistryRedactor::new());
    redactor.register_secret(KEY, "api_key:test");
    agent.set_redactor(redactor);

    // 真实文件 + 真实 bash：模拟"读 core.toml"的主泄漏通道。
    let file = std::env::temp_dir().join(format!("echo-redact-e2e-{}.txt", std::process::id()));
    std::fs::write(&file, format!("api_key = \"{KEY}\"\n")).expect("write fixture");

    let call = ToolCall {
        id: "redact-1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command": format!("cat {}", file.display())}).to_string(),
    };
    let result = agent
        .run_tool("local:tui::redact-e2e", "branch-1", &call)
        .await;
    assert!(
        !result.text.contains(KEY),
        "tool result must be redacted: {}",
        result.text
    );
    assert!(result.text.contains("【已隐藏:api_key:test】"));

    // 会话事件日志（ToolCall 参数 + ToolResult）零明文。
    let rendered = format!("{:?}", agent.trunk.event_log());
    assert!(!rendered.contains(KEY), "session event log must be clean");

    // 参数本身含密钥：执行按原始参数（输出即密钥），日志只记占位符。
    let call2 = ToolCall {
        id: "redact-2".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command": format!("printf '%s' {KEY}")}).to_string(),
    };
    let result2 = agent
        .run_tool("local:tui::redact-e2e", "branch-1", &call2)
        .await;
    assert!(!result2.text.contains(KEY));
    assert!(result2.text.contains("【已隐藏:api_key:test】"));
    let rendered = format!("{:?}", agent.trunk.event_log());
    assert!(!rendered.contains(KEY), "logged arguments must be redacted");

    std::fs::remove_file(&file).ok();
}

// ── 子代理模型覆盖（spawn_subagent profile 参数；2026-10）──

fn profile_cfg() -> crate::config::AgentConfig {
    use crate::config::ApiProfile;
    crate::config::AgentConfig {
        provider: "deepseek".into(),
        model: "deepseek-flash".into(),
        base_url: "https://api.deepseek.com/anthropic".into(),
        api_key: "sk-top-secret".into(),
        api_profiles: vec![ApiProfile {
            name: "kimi".into(),
            provider: "kimi".into(),
            model: "k3".into(),
            base_url: "https://api.kimi.com/coding".into(),
            api_key: "sk-kimi-secret".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn mock() -> Arc<dyn LlmProvider> {
    Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })
}

/// 按名解析成功（构建新 provider，不依赖网络）；未知名与缺 key 均
/// fail-closed（错误信息附可用清单），**不回退主模型**。
#[tokio::test]
async fn subagent_provider_resolves_named_profile_and_fails_closed() {
    use crate::config::ApiProfile;
    let agent = Agent::new(
        mock(),
        profile_cfg(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    );
    // 命中池中 profile → 构建成功
    assert!(agent.subagent_provider_for("kimi").await.is_ok());
    // 未知名 → 报错并列出可用清单
    let err = agent
        .subagent_provider_for("nope")
        .await
        .err()
        .expect("unknown profile must fail-closed");
    assert!(err.contains("不存在"), "{err}");
    assert!(err.contains("kimi"), "must list available profiles: {err}");
    // 宽松回退语义（与 apply_named_profile / probe 同口径）：profile 未配
    // key 时沿用顶层生效 key，不视为错误。
    let mut cfg = profile_cfg();
    cfg.api_profiles.push(ApiProfile {
        name: "keyless".into(),
        provider: "ollama".into(),
        model: "llama3".into(),
        base_url: "http://localhost:11434/v1".into(),
        api_key: String::new(),
        ..Default::default()
    });
    let agent = Agent::new(
        mock(),
        cfg,
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    );
    assert!(agent.subagent_provider_for("keyless").await.is_ok());

    // 全链路无 key（顶层与 profile 均空、provider=ollama 无 env 回退）
    // → fail-closed 明确报错。
    let mut cfg = profile_cfg();
    cfg.api_key = String::new();
    cfg.api_profiles.push(ApiProfile {
        name: "keyless".into(),
        provider: "ollama".into(),
        model: "llama3".into(),
        base_url: "http://localhost:11434/v1".into(),
        api_key: String::new(),
        ..Default::default()
    });
    let agent = Agent::new(
        mock(),
        cfg,
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    );
    let err = agent
        .subagent_provider_for("keyless")
        .await
        .err()
        .expect("utterly keyless profile must fail-closed");
    assert!(err.contains("API Key"), "{err}");
}

/// 池以共享 ConfigStore（core.toml）为首选事实源：文件里新加的 profile
/// 对 persona 侧解析立即可见（不受 persona 启动快照过期影响）。
#[tokio::test]
async fn subagent_provider_prefers_shared_config_store_pool() {
    let path = std::env::temp_dir().join(format!(
        "echo-subagent-pool-{}-{}.toml",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::write(
        &path,
        "[agent]\nprovider = \"deepseek\"\nmodel = \"deepseek-flash\"\napi_key = \"sk-top\"\n\n\
         [[agent.api_profiles]]\nname = \"kimi\"\nprovider = \"kimi\"\nmodel = \"k3\"\n\
         base_url = \"https://api.kimi.com/coding\"\napi_key = \"sk-kimi\"\n",
    )
    .unwrap();
    let agent = Agent::new(
        mock(),
        // 内存快照为空池——文件才是事实源
        crate::config::AgentConfig {
            provider: "deepseek".into(),
            model: "deepseek-flash".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: "sk-top".into(),
            ..Default::default()
        },
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    );
    agent.set_config_store(echo_adapter::ConfigStore::new(path.clone()));
    // 文件里的 kimi 可见（否则会报"不存在（当前未配置任何…）"）
    assert!(agent.subagent_provider_for("kimi").await.is_ok());
    std::fs::remove_file(&path).ok();
}
