use echo_tool::execute::invalid_tool_arguments;
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
        request: &ChatRequest,
        _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError> {
        // 测试桩：流式与非流式同效（主对话默认经此路径；增量断言由专门用例覆盖）。
        self.chat(request).await
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
        request: &ChatRequest,
        _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError> {
        // 测试桩：流式与非流式同效（主对话默认经此路径；增量断言由专门用例覆盖）。
        self.chat(request).await
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
        request: &ChatRequest,
        _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError> {
        // 测试桩：流式与非流式同效（主对话默认经此路径；增量断言由专门用例覆盖）。
        self.chat(request).await
    }
}

fn test_agent(provider: Arc<dyn LlmProvider>) -> Agent {
    Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(InstanceRegistry::new()),
    )
}

/// 走**插件命令域**（全插件化 S5）：API / provider 命令的归属已从宿主
/// `apply_command` 移到 `echo-plugin-provider-llm`。测试要覆盖的是**宿主
/// 接缝**（`AgentProviderCommandHost`）——这里直接打接缝，而不是测宿主那条
/// "插件未启用"的兜底分支（兜底有它自己的测试）。
/// 状态广播的**绑定**（2026-11 S7）：工具声明 `state_key`，装它的插件声明
/// 线上形状，执行链只做一次查表。
///
/// 这条锁住三件事：① 绑定生效时面板**真的**收到 `ChecklistUpdated`；
/// ② 会话归属来自上下文（不是参数魔术键）；③ **没绑定就不发**——不许猜。
#[tokio::test]
async fn tool_state_event_is_emitted_only_when_a_binding_exists() {
    use echo_plugin_tools::checklist::ChecklistTool;

    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    // 工具必须在注册表里（执行链查的是注册表）。
    // `register_shared`：注册到注册表寿命——用 `register_reversible` 却不保存
    // 返回值会让工具当场注销（那正是本文件要防的坑）。
    agent.tools.register_shared(Arc::new(ChecklistTool::new()));

    let received = Arc::new(std::sync::Mutex::new(Vec::<BackendEvent>::new()));
    let sink = received.clone();
    agent.attach_event_sink(Arc::new(move |event: BackendEvent| {
        sink.lock().expect("events poisoned").push(event);
    }));

    let call = ToolCall {
        id: "c1".into(),
        name: "checklist".into(),
        arguments: r#"{"operation":"create","name":"smoke"}"#.into(),
    };
    agent.run_tool("local:tui::local_user", "branch-1", &call).await;

    // ① 没有绑定 → 不发（工具声明了状态，但没人认领线上形状）。
    assert!(
        !received
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, BackendEvent::ChecklistUpdated { .. })),
        "没有绑定时不该发事件"
    );

    // ② 注册绑定（由拥有该工具的插件给出声明）→ 发，且会话来自上下文。
    for (key, build) in echo_plugin_tools::checklist::all_state_event_bindings() {
        agent.register_tool_state_event(key, build);
    }
    received.lock().unwrap().clear();
    let call = ToolCall {
        id: "c2".into(),
        name: "checklist".into(),
        arguments: r#"{"operation":"add","name":"smoke","item":"hello"}"#.into(),
    };
    agent.run_tool("local:tui::local_user", "branch-1", &call).await;

    let events = received.lock().unwrap();
    let found = events
        .iter()
        .find_map(|event| match event {
            BackendEvent::ChecklistUpdated { session_id, state, .. } => {
                Some((session_id.clone(), state.clone()))
            }
            _ => None,
        })
        .expect("注册绑定后必须发 ChecklistUpdated");
    assert_eq!(found.0, "local:tui::local_user", "会话来自调用上下文");
    assert!(
        serde_json::to_string(&found.1).unwrap().contains("hello"),
        "状态快照应含刚添加的条目：{}",
        found.1
    );
}

/// 走**会话视图命令域**（全插件化 S5）：面板视图命令由
/// `echo-plugin-management::session_view` 认领。这里直接打宿主接缝
/// （插件侧的投影/瘦身/顺序在自己 crate 的测试里覆盖）。
async fn apply_via_session_view(agent: &Arc<Agent>, cmd: BackendCommand) {
    let host = Arc::new(crate::agent::session_view_host::AgentSessionViewHost::new(
        Arc::clone(agent),
    ));
    let handler = echo_plugin_management::session_view::SessionViewHandler::new(host);
    handler
        .handle(cmd.kind_name(), cmd.payload_value())
        .await
        .expect("命令应属于会话视图命令域")
        .expect("命令处理失败");
}

async fn apply_via_provider_plugin(agent: &Arc<Agent>, cmd: BackendCommand) {
    let host = Arc::new(crate::agent::provider_host::AgentProviderCommandHost::new(
        Arc::clone(agent),
    ));
    let handler = echo_plugin_provider_llm::commands::ProviderCommandHandler::new(host);
    handler
        .handle(cmd.kind_name(), cmd.payload_value())
        .await
        .expect("命令应属于 provider 命令域")
        .expect("命令处理失败");
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
    // PluginInstance flow: inbound message event, then branch execution, then output.
    agent.emit(BackendEvent::MessageReceived {
        session_id: session.id.clone(),
        instance_name: "local".into(),
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

    let timeline = agent.trunk.timeline_snapshot();
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
        Arc::new(InstanceRegistry::new()),
    );
    let call = ToolCall {
        id: "c1".into(),
        name: "mock_tool".into(),
        arguments: r#"{"query":"x","api_key":"secret-1"}"#.into(),
    };
    let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
    assert_eq!(result.text, "found");

    let timeline = agent.trunk.timeline_snapshot();
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
    let timeline = agent.trunk.timeline_snapshot();
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
    assert_eq!(session.history.lock().len(), 2);
    let _ = agent.process_message(&session, "again").await.unwrap();
    assert_eq!(session.history.lock().len(), 4);
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

    let history = session.history.lock();
    assert_eq!(history.len(), 4);
    assert_eq!(structured_message_sequence(&history[0].content), Some(1));
    assert_eq!(history[1].content, "reply-1");
    assert_eq!(structured_message_sequence(&history[2].content), Some(2));
    assert_eq!(history[3].content, "reply-2");
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
    let history = session.history.lock();
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

    // 流式增量变体（2026-10）：同样标注 team（面板流式消息按 team 过滤）。
    let content_delta = Agent::annotate_team_for(
        BackendEvent::AgentContentDelta {
            session_id: "local:tui::local_user".into(),
            team_id: None,
            branch_id: "b1".into(),
            delta: "片段".into(),
        },
        Some("EchoCode".into()),
    );
    match content_delta {
        BackendEvent::AgentContentDelta { team_id, delta, .. } => {
            assert_eq!(
                team_id.as_deref(),
                Some("EchoCode"),
                "AgentContentDelta 应被标注"
            );
            assert_eq!(delta, "片段");
        }
        other => panic!("变体保持: {other:?}"),
    }
    let reasoning_delta = Agent::annotate_team_for(
        BackendEvent::AgentReasoningDelta {
            session_id: "s".into(),
            team_id: None,
            branch_id: "b1".into(),
            delta: "想想".into(),
        },
        Some("EchoCode".into()),
    );
    match reasoning_delta {
        BackendEvent::AgentReasoningDelta { team_id, .. } => {
            assert_eq!(
                team_id.as_deref(),
                Some("EchoCode"),
                "AgentReasoningDelta 应被标注"
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
        instance_name: "local".into(),
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

    apply_via_session_view(
        &agent,
        BackendCommand::RequestTrunkTimeline {
            team_id: Some("t".into()),
            since_seq: 0,
            limit: None,
        },
    )
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

/// 2026-10 修复回归（面板「会话无消息」）：请求某会话的显示历史必须从
/// 事件日志按会话派生——共享显示时间线被 1024 条 FIFO 挤出该会话条目后，
/// 数据仍在（事件日志是事实来源）；`max_seq` 是合并水位线（该会话在当前
/// 显示窗口中的最大 seq）。
#[tokio::test]
async fn request_session_history_derives_from_the_event_log() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = Arc::new(test_agent(provider));
    // 去主智能体后会话命令必须带 team_id：本 agent 视为 "t"。
    agent.set_team_id(Some("t".into()));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    let target = "local:tui::local_user";
    // 显示时间线里有一条该会话的实时条目（合并水位线 = 1）。
    agent.emit(BackendEvent::MessageReceived {
        session_id: target.into(),
        instance_name: "local".into(),
        platform: "local".into(),
        user_id: "local_user".into(),
        user_name: "local user".into(),
        channel: "direct".into(),
        group_name: None,
        content: "实时条目".into(),
        images: vec![],
        timestamp: 1700000000,
        received_at_ms: 1700000000123,
        message_sequence: 1,
        team_id: None,
    });
    // 事件日志里有一条该会话的历史（显示窗口被挤出后的唯一数据源）。
    agent
        .trunk
        .append_event(echo_session::SessionEvent::UserMessage(
            echo_session::UserMessage {
                content: "事件日志里的历史".into(),
                timestamp: 1700000001,
                message_sequence: Some(2),
                source: None,
                images: Vec::new(),
                session: Some(target.into()),
            },
        ));

    apply_via_session_view(
        &agent,
        BackendCommand::RequestSessionHistory {
            team_id: Some("t".into()),
            session_id: target.into(),
            limit: None,
        },
    )
    .await;

    let mut events = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        events.push(event);
    }
    let (session_id, messages, total, truncated, max_seq) = events
        .into_iter()
        .find_map(|event| match event {
            BackendEvent::SessionHistory {
                session_id,
                messages,
                total,
                truncated,
                max_seq,
                ..
            } => Some((session_id, messages, total, truncated, max_seq)),
            _ => None,
        })
        .expect("SessionHistory event emitted");
    assert_eq!(session_id, target);
    assert_eq!(total, 1);
    assert!(!truncated);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].kind, "user");
    assert_eq!(messages[0].content, "事件日志里的历史");
    assert_eq!(max_seq, 1, "合并水位线 = 该会话显示时间线条目的最大 seq");
}

/// 2026-10 用户报告回归：清理会话历史后向面板重发的全量时间线必须是
/// **实际剩余条目**（其他会话仍在），而不是空快照——"空全量"会被面板
/// 当权威数据、清掉整个本地聊天缓存（含未被清理的会话）。
#[tokio::test]
async fn clear_history_resends_remaining_timeline_not_empty() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = Arc::new(test_agent(provider));
    agent.set_team_id(Some("t".into()));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    let entry = |session: &str, content: &str| crate::event::TimelineMessage {
        kind: "user".into(),
        content: content.into(),
        session_id: session.into(),
        time: 1700000000,
        source: None,
        reasoning: None,
        tool: None,
        images: None,
        seq: 0,
        ord: 0,
    };
    agent.trunk.push_timeline(entry("sA", "A 的消息"));
    agent.trunk.push_timeline(entry("sB", "B 的消息"));

    agent
        .apply_command(BackendCommand::ClearHistory {
            team_id: Some("t".into()),
            session_id: Some("sA".into()),
        })
        .await;

    let mut news = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        news.push(event);
    }
    let (messages, full) = news
        .into_iter()
        .find_map(|event| match event {
            BackendEvent::TrunkTimeline { messages, full, .. } => Some((messages, full)),
            _ => None,
        })
        .expect("TrunkTimeline emitted after clear");
    assert!(full, "resync snapshot must be full");
    assert_eq!(
        messages.len(),
        1,
        "only the sibling session's entry remains: {messages:?}"
    );
    assert_eq!(messages[0].session_id, "sB");
}

/// 2026-10 审计回归：无 session_id 的 ArchiveHistory 必须**拒绝**——
/// 旧语义是"整档归档 + 全量清空"，缺失 session 时降级执行等于误清整个
/// 人格；唯一客户端（面板）已升级为必带 session_id，此路径按过旧客户端
/// fail-closed 处理。
#[tokio::test]
async fn archive_history_without_session_id_is_rejected_fail_closed() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = Arc::new(test_agent(provider));
    agent.set_team_id(Some("t".into()));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    // 先放一条时间线：拒绝路径不得清空、也不得重发快照。
    agent.trunk.push_timeline(crate::event::TimelineMessage {
        kind: "user".into(),
        content: "必须保留".into(),
        session_id: "sA".into(),
        time: 1700000000,
        source: None,
        reasoning: None,
        tool: None,
        images: None,
        seq: 0,
        ord: 0,
    });

    agent
        .apply_command(BackendCommand::ArchiveHistory {
            team_id: Some("t".into()),
            session_id: None,
        })
        .await;

    let mut errors = Vec::new();
    let mut timeline_broadcasts = 0usize;
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        match event {
            BackendEvent::Error { message, .. } => errors.push(message),
            BackendEvent::TrunkTimeline { .. } => timeline_broadcasts += 1,
            _ => {}
        }
    }
    assert!(
        errors.iter().any(|m| m.contains("session_id")),
        "missing session_id must be rejected with a clear message: {errors:?}"
    );
    assert_eq!(
        timeline_broadcasts, 0,
        "rejection must not resync the timeline"
    );
    let kept = agent.trunk.timeline_snapshot();
    assert!(
        kept.iter().any(|m| m.content == "必须保留"),
        "rejection must not clear anything: {kept:?}"
    );
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
            limit: None,
        },
        BackendCommand::RequestContext {
            team_id: None,
            session_id: None,
        },
        BackendCommand::ClearHistory {
            team_id: None,
            session_id: None,
        },
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
    let agent = Arc::new(test_agent(provider));
    agent.set_config_path(tmp.clone());

    apply_via_provider_plugin(
        &agent,
        BackendCommand::UpdateApiConfig {
            name: String::new(),
            provider: "anthropic".into(),
            model: "claude-x".into(),
            base_url: "http://example.test".into(),
            api_key: "sk-test-123".into(),
            thinking: None,
            reasoning_effort: None,
        },
    )
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

// ── 会话级 API 覆盖（2026-10）──

/// 会话级覆盖：设置后 resolve_session_provider 返回专用 provider；
/// 清除后回到 persona 级。
#[tokio::test]
async fn session_provider_override_resolves_and_clears() {
    let persona_provider: Arc<dyn LlmProvider> = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "persona".into(),
    });
    let agent = test_agent(persona_provider.clone());
    // 预置一个 profile（池）——set_session_provider 按名解析。
    {
        let mut cfg = agent.model.config.write().await;
        cfg.api_profiles
            .push(crate::config::ApiProfile::new("kimi", "openai", "kimi-k3"));
        cfg.api_profiles[0].api_key = "sk-kimi-test".into();
        cfg.api_profiles[0].base_url = "http://kimi.test".into();
    }

    // 无覆盖时 → persona 级 provider。
    let p = agent.resolve_session_provider("sess-1").await;
    assert_eq!(p.name(), "mock", "no override → persona provider");

    // 设置覆盖 → 专用 provider（实例不同——用 Arc 指针区分）。
    let ok = agent.set_session_provider("sess-1", "kimi").await;
    assert!(ok, "set_session_provider(kimi) should succeed");
    let p = agent.resolve_session_provider("sess-1").await;
    // profile "kimi" 声明 provider="openai" → create_provider 构建 OpenAI
    // provider（name="openai"）；与 persona 级 MockProvider（name="mock"）
    // 区分。
    assert_eq!(p.name(), "openai", "session override → profile provider");
    assert!(
        !Arc::ptr_eq(&p, &persona_provider),
        "session override must be a different provider instance"
    );

    // 同 profile 重复设置 → 幂等（不重建）。
    let ok = agent.set_session_provider("sess-1", "kimi").await;
    assert!(ok);

    // 清除覆盖 → 回到 persona 级。
    let ok = agent.set_session_provider("sess-1", "").await;
    assert!(ok);
    let p = agent.resolve_session_provider("sess-1").await;
    assert!(
        Arc::ptr_eq(&p, &persona_provider),
        "cleared override → persona provider"
    );
}

/// 会话级覆盖不影响其他会话：sess-1 有覆盖，sess-2 无覆盖。
#[tokio::test]
async fn session_provider_override_is_scoped() {
    let persona_provider: Arc<dyn LlmProvider> = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "persona".into(),
    });
    let agent = test_agent(persona_provider.clone());
    {
        let mut cfg = agent.model.config.write().await;
        cfg.api_profiles.push(crate::config::ApiProfile::new(
            "deepseek-flash",
            "deepseek",
            "deepseek-chat",
        ));
        cfg.api_profiles[0].api_key = "sk-ds-test".into();
    }

    let ok = agent.set_session_provider("sess-1", "deepseek-flash").await;
    assert!(ok);

    // sess-1 有覆盖，sess-2 无。
    let p1 = agent.resolve_session_provider("sess-1").await;
    let p2 = agent.resolve_session_provider("sess-2").await;
    assert!(
        !Arc::ptr_eq(&p1, &p2),
        "sess-1 overridden, sess-2 follows persona"
    );
    assert!(Arc::ptr_eq(&p2, &persona_provider));
}

/// 不存在的 profile → set_session_provider 失败（回退 persona 级）。
#[tokio::test]
async fn session_provider_override_rejects_unknown_profile() {
    let agent = test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "x".into(),
    }));
    let ok = agent.set_session_provider("sess-1", "nonexistent").await;
    assert!(!ok, "unknown profile must fail");
    let p = agent.resolve_session_provider("sess-1").await;
    assert_eq!(p.name(), "mock", "fallback to persona provider");
}

/// 会话级覆盖的 profile 解析也以共享 ConfigStore（core.toml）为首选事实
/// 源：管理面新增（persona 内存快照尚未包含）的 profile 必须能解析成功
/// ——2026-10 用户报障回归（session profile resolve failed: profile not
/// found: uuapi-gemini：persona 快照落后于管理面）。
#[tokio::test]
async fn session_provider_prefers_shared_config_store_pool() {
    let path = std::env::temp_dir().join(format!(
        "echo-session-pool-{}-{}.toml",
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
    let agent = test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "x".into(),
    }));
    // 内存快照为空池——文件才是事实源。
    agent.set_config_store(echo_config::ConfigStore::new(path.clone()));
    let ok = agent.set_session_provider("sess-store", "kimi").await;
    assert!(ok, "store-only profile must resolve for session override");
    let p = agent.resolve_session_provider("sess-store").await;
    // provider="kimi" 走 Anthropic 兼容实现（name = "anthropic"）。
    assert_eq!(
        p.name(),
        "anthropic",
        "session override built from store profile"
    );
    std::fs::remove_file(&path).ok();
}

/// 会话模型与覆盖 provider **成对**解析：覆盖生效时 `session_model` 返回
/// 覆盖 profile 的模型（2026-10 用户报障回归——请求模型若仍取 persona
/// active_model，覆盖端点会按该模型报 404 model_not_found）。
#[tokio::test]
async fn session_model_pairs_with_override_provider() {
    let agent = test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "x".into(),
    }));
    {
        let mut cfg = agent.model.config.write().await;
        cfg.api_profiles.push(crate::config::ApiProfile::new(
            "gemini",
            "anthropic",
            "gemini-4-argon",
        ));
        cfg.api_profiles[0].api_key = "sk-x".into();
        cfg.api_profiles[0].base_url = "https://uuapi.io".into();
    }
    agent.set_model("deepseek-v4-flash".into()).await;
    // 无覆盖 = persona 级 active_model。
    assert_eq!(agent.session_model("sess-1").await, "deepseek-v4-flash");
    // 有覆盖 = 覆盖 profile 的模型（与 resolve_session_provider 配对）。
    assert!(agent.set_session_provider("sess-1", "gemini").await);
    assert_eq!(agent.session_model("sess-1").await, "gemini-4-argon");
    // 覆盖只作用于自身会话。
    assert_eq!(agent.session_model("sess-2").await, "deepseek-v4-flash");
    // 清除后回到 persona 级。
    assert!(agent.set_session_provider("sess-1", "").await);
    assert_eq!(agent.session_model("sess-1").await, "deepseek-v4-flash");
}

// ── API profile 改名（2026-10）──

/// 改名：池条目名与 active_api 一起改写；单个 ConfigStore patch 落盘
/// （旧名消失、值保留、无关段不丢）；回推 ApiConfigUpdated + 通知。
#[tokio::test]
async fn rename_api_rewrites_pool_and_active_api() {
    let tmp = std::env::temp_dir().join(format!(
        "echo-rename-{}-{}.toml",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::write(
        &tmp,
        "[server]\nx = 1\n\n[agent]\nprovider = \"openai\"\nmodel = \"top\"\napi_key = \"sk-top\"\n",
    )
    .unwrap();
    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));
    agent.set_config_path(tmp.clone());
    {
        let mut cfg = agent.model.config.write().await;
        cfg.api_profiles
            .push(crate::config::ApiProfile::new("alpha", "deepseek", "m1"));
        cfg.api_profiles[0].api_key = "sk-alpha".into();
        cfg.api_profiles
            .push(crate::config::ApiProfile::new("beta", "kimi", "m2"));
        cfg.active_api = "alpha".into();
    }

    agent
        .rename_api("alpha", " gamma ", None) // 去空白验证
        .await;

    let cfg = agent.api_config().await;
    assert!(
        cfg.api_profiles.iter().all(|p| p.name != "alpha"),
        "old name must be gone: {:?}",
        cfg.api_profiles.iter().map(|p| &p.name).collect::<Vec<_>>()
    );
    let renamed = cfg
        .api_profiles
        .iter()
        .find(|p| p.name == "gamma")
        .expect("renamed profile exists");
    assert_eq!(renamed.api_key, "sk-alpha", "values survive rename");
    assert_eq!(renamed.provider, "deepseek");
    assert_eq!(cfg.active_api, "gamma", "active_api follows rename");

    let content = std::fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("name = \"gamma\""),
        "pool persisted under new name: {content}"
    );
    assert!(
        !content.contains("name = \"alpha\""),
        "old name must not remain: {content}"
    );
    assert!(
        content.contains("active_api = \"gamma\""),
        "active_api persisted: {content}"
    );
    assert!(
        content.contains("[server]\nx = 1"),
        "unrelated sections preserved: {content}"
    );

    let mut updated = 0;
    let mut notify = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        match event {
            BackendEvent::ApiConfigUpdated { active_api, .. } => {
                updated += 1;
                assert_eq!(active_api, "gamma");
            }
            BackendEvent::Error { message, .. } => notify.push(message),
            _ => {}
        }
    }
    assert!(updated >= 1, "ApiConfigUpdated must be re-emitted");
    assert!(
        notify.iter().any(|m| m.contains("renamed API config")),
        "notify message: {notify:?}"
    );
    std::fs::remove_file(&tmp).ok();
}

/// from == to（trim 后）→ 幂等 no-op：零写入，只回推配置事件
///（Panel 依赖回推收起表单）。
#[tokio::test]
async fn rename_api_same_name_is_noop() {
    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));
    {
        let mut cfg = agent.model.config.write().await;
        cfg.api_profiles
            .push(crate::config::ApiProfile::new("alpha", "deepseek", "m1"));
        cfg.active_api = "alpha".into();
    }

    agent.rename_api("alpha", " alpha ", None).await;

    let cfg = agent.api_config().await;
    assert_eq!(cfg.api_profiles.len(), 1);
    assert_eq!(cfg.api_profiles[0].name, "alpha");

    let mut updated = 0;
    let mut errors = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        match event {
            BackendEvent::ApiConfigUpdated { .. } => updated += 1,
            BackendEvent::Error { message, .. } => errors.push(message),
            _ => {}
        }
    }
    assert!(updated >= 1, "no-op rename must still re-emit config");
    assert!(
        !errors.iter().any(|m| m.contains("rename failed")),
        "no-op must not error: {errors:?}"
    );
}

/// to 已被占用 → 拒绝，零写入（不合并、不覆盖）。
#[tokio::test]
async fn rename_api_rejects_existing_target() {
    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));
    {
        let mut cfg = agent.model.config.write().await;
        cfg.api_profiles
            .push(crate::config::ApiProfile::new("alpha", "deepseek", "m1"));
        cfg.api_profiles
            .push(crate::config::ApiProfile::new("beta", "kimi", "m2"));
        cfg.active_api = "alpha".into();
    }

    agent.rename_api("alpha", "beta", None).await;

    let cfg = agent.api_config().await;
    assert!(cfg.api_profiles.iter().any(|p| p.name == "alpha"));
    assert!(cfg.api_profiles.iter().any(|p| p.name == "beta"));
    assert_eq!(
        cfg.active_api, "alpha",
        "failed rename must not touch state"
    );

    let mut errors = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        if let BackendEvent::Error { message, .. } = event {
            errors.push(message);
        }
    }
    assert!(
        errors.iter().any(|m| m.contains("already exists")),
        "errors: {errors:?}"
    );
}

/// from 不存在 / 空 → 拒绝（精确匹配语义）。
#[tokio::test]
async fn rename_api_rejects_missing_source() {
    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    for from in ["ghost", ""] {
        agent.rename_api(from, "x", None).await;
    }

    let mut errors = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        if let BackendEvent::Error { message, .. } = event {
            errors.push(message);
        }
    }
    assert_eq!(errors.len(), 2, "both must error: {errors:?}");
    assert!(
        errors.iter().all(|m| m.contains("API config not found")),
        "errors: {errors:?}"
    );
}

/// to 空白 / 带控制字符 / 超长 → 拒绝。
#[tokio::test]
async fn rename_api_rejects_invalid_target() {
    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));
    {
        let mut cfg = agent.model.config.write().await;
        cfg.api_profiles
            .push(crate::config::ApiProfile::new("alpha", "deepseek", "m1"));
    }

    for to in ["   ", "bad\tname", &"x".repeat(200)] {
        agent.rename_api("alpha", to, None).await;
    }

    let cfg = agent.api_config().await;
    assert_eq!(cfg.api_profiles.len(), 1);
    assert_eq!(cfg.api_profiles[0].name, "alpha");

    let mut errors = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        if let BackendEvent::Error { message, .. } = event {
            errors.push(message);
        }
    }
    assert_eq!(errors.len(), 3, "all must error: {errors:?}");
    assert!(
        errors.iter().all(|m| m.contains("API rename failed")),
        "errors: {errors:?}"
    );
}

/// 会话级覆盖重绑：引用被改名 profile 的会话切到新名（重建 provider），
/// 引用其他 profile 的会话不受影响。
#[tokio::test]
async fn rename_api_rebinds_session_provider_under_new_name() {
    let agent = test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    }));
    {
        let mut cfg = agent.model.config.write().await;
        cfg.api_profiles
            .push(crate::config::ApiProfile::new("kimi", "openai", "kimi-k3"));
        cfg.api_profiles[0].api_key = "sk-kimi".into();
        cfg.api_profiles[0].base_url = "http://kimi.test".into();
        cfg.api_profiles
            .push(crate::config::ApiProfile::new("other", "openai", "m-other"));
        cfg.api_profiles[1].api_key = "sk-other".into();
        cfg.active_api = "kimi".into();
    }
    assert!(agent.set_session_provider("sess-1", "kimi").await);
    assert!(agent.set_session_provider("sess-2", "other").await);
    let before = agent
        .model
        .session_providers
        .get("sess-1")
        .map(|e| e.value().1.clone())
        .expect("override set");

    agent.rename_api("kimi", "k3", None).await;

    let entry = agent
        .model
        .session_providers
        .get("sess-1")
        .map(|e| e.value().clone())
        .expect("override kept after rename");
    assert_eq!(entry.0, "k3", "override rebound to the new name");
    assert!(
        !Arc::ptr_eq(&entry.1, &before),
        "rebind must rebuild the provider (metering name is baked at build)"
    );
    assert_eq!(entry.1.name(), "openai");
    // 其他 profile 的覆盖不受影响。
    assert_eq!(
        agent
            .model
            .session_providers
            .get("sess-2")
            .unwrap()
            .value()
            .0,
        "other"
    );
}

/// persona 引用重引用：引用被改名 profile 的 agent 切到新名。
#[tokio::test]
async fn rename_api_reapplies_persona_reference() {
    let agent = test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    }));
    {
        let mut cfg = agent.model.config.write().await;
        cfg.api_profiles
            .push(crate::config::ApiProfile::new("kimi", "openai", "kimi-k3"));
        cfg.api_profiles[0].api_key = "sk-kimi".into();
        cfg.api_profiles[0].base_url = "http://kimi.test".into();
    }
    agent.set_persona_api(Some("kimi".into())).await;

    agent.rename_api("kimi", "k3", None).await;

    assert_eq!(
        agent.model.persona_api.read().await.as_deref(),
        Some("k3"),
        "persona reference must follow the rename"
    );
}

/// 落盘失败 → 内存零改动（落盘先行、提交在后 = 天然回滚）。
#[tokio::test]
async fn rename_api_persist_failure_leaves_memory_unchanged() {
    let blocker =
        std::env::temp_dir().join(format!("echo-rename-blocker-{}", uuid::Uuid::new_v4()));
    std::fs::write(&blocker, "not a directory").unwrap();
    // 父路径是文件 → patch 写临时文件必然 ENOTDIR。
    let bad = blocker.join("nested").join("cfg.toml");

    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));
    agent.set_config_store(echo_config::ConfigStore::new(bad));
    {
        let mut cfg = agent.model.config.write().await;
        cfg.api_profiles
            .push(crate::config::ApiProfile::new("alpha", "deepseek", "m1"));
        cfg.active_api = "alpha".into();
    }

    agent.rename_api("alpha", "gamma", None).await;

    let cfg = agent.api_config().await;
    assert!(
        cfg.api_profiles.iter().any(|p| p.name == "alpha"),
        "memory must stay untouched on persist failure"
    );
    assert!(cfg.api_profiles.iter().all(|p| p.name != "gamma"));
    assert_eq!(cfg.active_api, "alpha");

    let mut errors = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        if let BackendEvent::Error { message, .. } = event {
            errors.push(message);
        }
    }
    assert!(
        errors.iter().any(|m| m.contains("persist error")),
        "errors: {errors:?}"
    );
    std::fs::remove_file(&blocker).ok();
}

/// echo-loop turn 的请求模型与 provider 成对（会话覆盖 = profile 模型）：
/// 2026-10 用户报障回归——此前请求模型取 persona active_model，打到覆盖
/// 端点 404 model_not_found。LlmRequest 事件在模型调用前发出，断言不依赖
/// 调用成功（覆盖端点指向空端口）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_loop_turn_uses_session_override_model() {
    let persona = Arc::new(ScriptedProvider::new(vec![]));
    let agent = Arc::new(Agent::new(
        persona.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(InstanceRegistry::new()),
    ));
    {
        let mut cfg = agent.model.config.write().await;
        cfg.api_profiles.push(crate::config::ApiProfile::new(
            "gemini",
            "anthropic",
            "gemini-4-argon",
        ));
        cfg.api_profiles[0].api_key = "sk-x".into();
        // 空端口：连接必然失败（不依赖真实网络）；只断言请求模型。
        cfg.api_profiles[0].base_url = "http://127.0.0.1:1".into();
    }
    agent.set_model("deepseek-v4-flash".into()).await;

    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    {
        let seen = seen.clone();
        let sink: EventSink = Arc::new(move |event: BackendEvent| {
            if let BackendEvent::LlmRequest { model, .. } = &event {
                seen.lock().unwrap().push(model.clone());
            }
        });
        agent.attach_event_sink(sink);
    }

    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    assert!(
        agent.set_session_provider(&session.id, "gemini").await,
        "覆盖 provider 应构建成功"
    );

    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        persona.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);

    // turn 因覆盖端点连接失败而报错——LlmRequest 已在模型调用前发出。
    let _ = agent.process_message(&session, "hi").await;

    let models = seen.lock().unwrap().clone();
    assert_eq!(
        models.first().map(String::as_str),
        Some("gemini-4-argon"),
        "turn 请求模型必须与覆盖 provider 成对：{models:?}"
    );
}

/// 内置循环路径同口径：请求模型与 provider 成对（会话覆盖 = profile 模型）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builtin_loop_turn_uses_session_override_model() {
    let persona = Arc::new(ScriptedProvider::new(vec![]));
    let agent = Arc::new(Agent::new(
        persona.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(InstanceRegistry::new()),
    ));
    {
        let mut cfg = agent.model.config.write().await;
        cfg.api_profiles.push(crate::config::ApiProfile::new(
            "gemini",
            "anthropic",
            "gemini-4-argon",
        ));
        cfg.api_profiles[0].api_key = "sk-x".into();
        cfg.api_profiles[0].base_url = "http://127.0.0.1:1".into();
    }
    agent.set_model("deepseek-v4-flash".into()).await;

    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    {
        let seen = seen.clone();
        let sink: EventSink = Arc::new(move |event: BackendEvent| {
            if let BackendEvent::LlmRequest { model, .. } = &event {
                seen.lock().unwrap().push(model.clone());
            }
        });
        agent.attach_event_sink(sink);
    }

    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    assert!(agent.set_session_provider(&session.id, "gemini").await);

    // 未注入 runner：首个 turn 惰性构建默认 runner；连接失败不影响事件断言。
    let _ = agent.process_message(&session, "hi").await;

    let models = seen.lock().unwrap().clone();
    assert_eq!(
        models.first().map(String::as_str),
        Some("gemini-4-argon"),
        "内置循环请求模型必须与覆盖 provider 成对：{models:?}"
    );
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
    let agent = Arc::new(test_agent(provider));
    agent.set_config_path(tmp.clone());

    apply_via_provider_plugin(
        &agent,
        BackendCommand::UpdateApiConfig {
            name: String::new(),
            provider: "anthropic".into(),
            model: "claude-x".into(),
            base_url: "http://example.test".into(),
            api_key: String::new(),
            thinking: None,
            reasoning_effort: None,
        },
    )
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
    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    // 模拟"过期快照"：agent 内存里的 teams 是空的，磁盘上已有 fresh。
    agent.set_config_path(tmp.clone());

    apply_via_provider_plugin(
        &agent,
        BackendCommand::UpdateApiConfig {
            name: String::new(),
            provider: "anthropic".into(),
            model: "claude-x".into(),
            base_url: "http://example.test".into(),
            api_key: String::new(),
            thinking: None,
            reasoning_effort: None,
        },
    )
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
    let agent = Arc::new(test_agent(provider));

    // 先建立顶层默认配置。
    apply_via_provider_plugin(
        &agent,
        BackendCommand::UpdateApiConfig {
            name: String::new(),
            provider: "deepseek".into(),
            model: "deepseek-x".into(),
            base_url: "http://ds.test".into(),
            api_key: "sk-top".into(),
            thinking: None,
            reasoning_effort: None,
        },
    )
    .await;

    // 用全空字段创建命名 profile：应从当前生效配置继承，而不是写出
    // 残缺 profile（真实事故：TUI 只改 model 保存后 profile 缺字段）。
    apply_via_provider_plugin(
        &agent,
        BackendCommand::UpdateApiConfig {
            name: "backup".into(),
            provider: String::new(),
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            thinking: None,
            reasoning_effort: None,
        },
    )
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
    apply_via_provider_plugin(
        &agent,
        BackendCommand::UpdateApiConfig {
            name: "backup".into(),
            provider: String::new(),
            model: "deepseek-y".into(),
            base_url: String::new(),
            api_key: String::new(),
            thinking: None,
            reasoning_effort: None,
        },
    )
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
    apply_via_provider_plugin(
        &agent,
        BackendCommand::UpdateApiConfig {
            name: String::new(),
            provider: "deepseek".into(),
            model: "deepseek-x".into(),
            base_url: "http://127.0.0.1:1".into(),
            api_key: "sk-top".into(),
            thinking: None,
            reasoning_effort: None,
        },
    )
    .await;

    // 手工注入一个字段残缺的 profile（provider/base_url 为空）。
    agent
        .model
        .config
        .write()
        .await
        .api_profiles
        .push(ApiProfile {
            name: "broken".into(),
            provider: String::new(),
            model: "deepseek-x".into(),
            base_url: String::new(),
            api_key: String::new(),
            thinking: crate::config::ThinkingMode::Enabled,
            reasoning_effort: crate::config::ReasoningEffort::Max,
        });

    apply_via_provider_plugin(
        &agent,
        BackendCommand::TestApi {
            name: "broken".into(),
        },
    )
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
    use echo_plugin::{PluginDescriptor, PluginManifest};
    let manifest = PluginManifest::builtin(
        "acme.tools.weather",
        "Weather",
        "1.0.0",
        "tool",
        "weather",
        "old desc",
    );
    let desc = PluginDescriptor::from(&manifest);

    // 完全一致：不 stale
    assert!(!super::plugin_reload::data_plugin_stale(&desc, &manifest));

    // version 变化 → stale
    let mut v2 = manifest.clone();
    v2.version = "1.1.0".into();
    assert!(super::plugin_reload::data_plugin_stale(&desc, &v2));

    // description 变化 → stale
    let mut d2 = manifest.clone();
    d2.description = "new desc".into();
    assert!(super::plugin_reload::data_plugin_stale(&desc, &d2));

    // 包归属变化（声明 package）→ stale
    let p2 = manifest.clone().with_package("acme.weather-suite");
    assert!(super::plugin_reload::data_plugin_stale(&desc, &p2));

    // 未声明 package 时 package_id = 插件 id，不算 stale
    let same = manifest.clone();
    assert!(!super::plugin_reload::data_plugin_stale(&desc, &same));
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
    crate::tool::builtin::coding::register_coding_tools(
        &mut tools,
        std::env::temp_dir(),
        None,
        None,
    );
    let agent = Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(InstanceRegistry::new()),
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
    crate::tool::builtin::coding::register_coding_tools(
        &mut tools,
        std::env::temp_dir(),
        None,
        None,
    );
    let agent = Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(InstanceRegistry::new()),
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
    crate::tool::builtin::coding::register_coding_tools(
        &mut tools,
        std::env::temp_dir(),
        None,
        None,
    );
    let agent = Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(InstanceRegistry::new()),
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
        Arc::new(InstanceRegistry::new()),
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
        Arc::new(InstanceRegistry::new()),
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

    let skills = agent.prompt.skills.lock().await;
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
    assert!(agent.prompt.skills.lock().await.get("style").is_none());

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
        request: &ChatRequest,
        _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError> {
        // 测试桩：流式与非流式同效（主对话默认经此路径；增量断言由专门用例覆盖）。
        self.chat(request).await
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
        Arc::new(InstanceRegistry::new()),
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
        Arc::new(InstanceRegistry::new()),
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
        Arc::new(InstanceRegistry::new()),
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

/// 2026-10 观测缺口修复回归：spawn_subagent 经异步通道直入管线（不走
/// `run_tool`），其调用与回执必须照样落事件日志——此前委派没有 spawn
/// 时间戳、也读不到 profile/node/timeout 实参（审计只能靠完成钩子倒推）。
#[tokio::test]
async fn spawn_subagent_call_and_receipt_are_recorded_in_event_log() {
    let task_text =
        "记录验证用子任务：本用例不真正执行委派（运行态未接线），只断言 spawn 调用与回执已落库。";
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatResponse {
            stop_reason: Some("tool_calls".into()),
            content: None,
            reasoning_content: None,
            tool_calls: vec![crate::llm::ToolCall {
                id: "call_spawn_rec_1".into(),
                name: crate::subagent::SPAWN_SUBAGENT_TOOL.into(),
                arguments: serde_json::json!({
                    "task": task_text,
                    "timeout_secs": 321,
                })
                .to_string(),
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
        Arc::new(InstanceRegistry::new()),
    ));
    // 不接运行态：本用例验证"记录先行"——即便受理被拒（运行态未接线），
    // 调用与回执也必须成对落库（与 run_tool 的权限前置记录同口径）。
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    assert_eq!(
        agent
            .process_message(&session, "委派一个任务")
            .await
            .unwrap(),
        "done"
    );

    // 事件日志：ToolCall(spawn_subagent) 与配对 ToolResult 必须成对落库，
    // 实参（task 全文 / timeout_secs / spawn 时间戳）完整保留。
    let log = agent.trunk.event_log();
    let call = log
        .iter()
        .find_map(|event| match event {
            echo_session::SessionEvent::ToolCall(call)
                if call.name == crate::subagent::SPAWN_SUBAGENT_TOOL =>
            {
                Some(call.clone())
            }
            _ => None,
        })
        .expect("spawn 调用必须落 ToolCall 事件（观测缺口修复）");
    assert_eq!(call.id, "call_spawn_rec_1");
    assert!(
        call.arguments.contains("记录验证用子任务"),
        "arguments 必须含 task 全文: {}",
        call.arguments
    );
    assert!(
        call.arguments.contains("321"),
        "timeout_secs 实参必须保留: {}",
        call.arguments
    );
    assert!(call.started_at_ms.is_some(), "spawn 时间戳（时延计算口径）");
    let receipt = log
        .iter()
        .find_map(|event| match event {
            echo_session::SessionEvent::ToolResult(result)
                if result.tool_call_id == "call_spawn_rec_1" =>
            {
                Some(result.clone())
            }
            _ => None,
        })
        .expect("spawn 回执必须落 ToolResult 事件（成对，不留悬空调用）");
    assert!(!receipt.result.is_empty(), "回执文本非空");

    // 面板事件同口径（委派行可见）：ToolCall + ToolResult。
    let mut events = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        events.push(event);
    }
    assert!(
        events.iter().any(|e| matches!(
            e,
            BackendEvent::ToolCall { tool_name, .. }
                if tool_name == crate::subagent::SPAWN_SUBAGENT_TOOL
        )),
        "面板 ToolCall 缺失（委派行不可见）"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            BackendEvent::ToolResult { tool_call_id, .. }
                if tool_call_id == "call_spawn_rec_1"
        )),
        "面板 ToolResult 缺失"
    );
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
        Arc::new(InstanceRegistry::new()),
    ));
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);

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
        Arc::new(InstanceRegistry::new()),
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

            conversations: vec![],
            last_conversation: None,
        })
        .unwrap();
    store.set_active(Some("proj".into()), None).unwrap();
    // 工作区块由 **workspace 插件**贡献（`Contribution::Text`），不是 Agent
    // 自己拼的——所以这里也得把插件装上（与组合根同款接法）。
    {
        let host = Arc::new(crate::plugins::PluginHost::new());
        let prompt_store = store.clone();
        host.register_and_mount(Arc::new(
            echo_plugin::BuiltinPlugin::new(
                echo_plugin::PluginManifest::builtin(
                    crate::plugins::WORKSPACE_PLUGIN_ID,
                    "工作区会话",
                    "1",
                    "tool",
                    "workspace",
                    "",
                ),
                |_| Ok(vec![]),
            )
            .with_text_blocks([echo_plugin::PluginTextBlock::new(
                "workspace",
                "工作区会话",
                50,
            )])
            .with_text_block_content(move |key, ctx| {
                if key != "workspace" {
                    return None;
                }
                match ctx.get("session_id").and_then(|v| v.as_str()) {
                    Some(session) if !session.is_empty() => prompt_store
                        .prompt_text_for(session)
                        .or_else(|| prompt_store.prompt_text()),
                    _ => prompt_store.prompt_text(),
                }
            }),
        ))
        .expect("mount workspace plugin");
        agent.set_plugin_host(host);
    }
    agent.set_workspace_store(store.clone());

    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);

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
/// 回归（2026-10 生产事故）：DeepSeek 402/余额不足时，echo-loop 路径
/// 必须在**收尾时**发 `AgentCompleted` + `Error` 事件——此前 `Err(other)`
/// 分支静默吞掉，用户在面板看到的是"转了圈后什么都没发生"、busy 浮条
/// 永久挂着。本测试模拟"provider 返回错误"（如 402 余额不足）。
#[tokio::test]
async fn echo_loop_model_error_surfaces_completed_and_error_events() {
    use crate::llm::LlmError;
    // 模拟 provider 返回错误（402/余额不足）。
    struct FailingProvider;
    #[async_trait::async_trait]
    impl LlmProvider for FailingProvider {
        fn name(&self) -> &str {
            "fail"
        }
        fn default_model(&self) -> &str {
            "fail-model"
        }
        async fn chat(&self, _: &ChatRequest) -> Result<ChatResponse, LlmError> {
            Err(LlmError::Api("HTTP 402: Insufficient Balance".into()))
        }
        async fn chat_stream(
            &self,
            _: &ChatRequest,
            _: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<ChatResponse, LlmError> {
            Err(LlmError::Api("HTTP 402: Insufficient Balance".into()))
        }
    }
    let agent = Arc::new(Agent::new(
        Arc::new(FailingProvider),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(InstanceRegistry::new()),
    ));
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        Arc::new(FailingProvider),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);

    let err = agent.process_message(&session, "你好").await;
    assert!(err.is_err(), "402 必然失败: {err:?}");

    let events: Vec<_> = {
        let mut out = Vec::new();
        while let Ok(ev) = bridge.event_rx.lock().await.try_recv() {
            out.push(ev);
        }
        out
    };
    // 关键断言：AgentCompleted（busy 收尾）+ Error（错误可见）必须都发
    let has_completed = events
        .iter()
        .any(|e| matches!(e, BackendEvent::AgentCompleted { .. }));
    let has_error = events
        .iter()
        .any(|e| matches!(e, BackendEvent::Error { .. }));
    assert!(
        has_completed,
        "缺 AgentCompleted（busy 浮条会永远挂着）: {events:?}"
    );
    assert!(has_error, "缺 Error（错误对用户不可见）: {events:?}");
    // 错误消息必须包含 provider 的 402 提示（而非静默的 LoopError::Model）
    let error_msg = events
        .iter()
        .find_map(|e| {
            if let BackendEvent::Error { message, .. } = e {
                Some(message.clone())
            } else {
                None
            }
        })
        .expect("Error event missing");
    assert!(
        error_msg.contains("402") || error_msg.contains("Insufficient"),
        "错误消息未携带 provider 错误: {error_msg}"
    );
}

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
            prompt_cache_hit_tokens: None,
            prompt_cache_miss_tokens: None,
            prompt_cache_creation_tokens: None,
        },
    }]));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(InstanceRegistry::new()),
    ));
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
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
        agent.prompt.last_blocks(&session.id).is_some(),
        "last_prompt_blocks 必须按会话写入（/context 视图）"
    );
}

/// 2026-10 上下文审计 🟡 回归：/context 的提示词块按会话隔离——会话 A
/// 的 turn 不得覆盖会话 B 的 `last_prompt_blocks`（此前 agent 级单槽，
/// 多会话时面板切到 B 看到的却是 A 的提示词块）。
#[tokio::test]
async fn last_prompt_blocks_are_scoped_per_session() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatResponse {
            stop_reason: None,
            content: Some("reply-a".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        },
        ChatResponse {
            stop_reason: None,
            content: Some("reply-b".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        },
    ]));
    let agent = test_agent(provider);
    // 注册一个触发式技能（关键词 alpha）：命中会话 A 的输入、不命中 B，
    // 使两会话的提示词块内容可区分。
    agent
        .prompt
        .skills
        .lock()
        .await
        .register(echo_defs::skill::Skill::direct(
            "scoped-probe",
            "probe",
            vec!["alpha".into()],
            false,
            "probe",
            "ALPHA 技能正文",
        ));
    let session_a = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    let session_b = agent.trunk.get_or_create(
        &SessionKey::parse("qq:dm::42").unwrap(),
        "qq-user".into(),
        None,
    );

    agent.process_message(&session_a, "alpha").await.unwrap();
    agent.process_message(&session_b, "beta").await.unwrap();

    let blocks_a = agent.prompt.last_blocks(&session_a.id);
    let blocks_b = agent.prompt.last_blocks(&session_b.id);
    let text_of = |blocks: Option<Vec<super::boundary::PromptBlock>>| {
        blocks
            .expect("提示词块")
            .iter()
            .map(|b| b.content.clone())
            .collect::<String>()
    };
    let text_a = text_of(blocks_a);
    let text_b = text_of(blocks_b);
    // A 的输入命中触发词 alpha → 含技能正文；B 未命中 → 不含。若仍是
    // agent 级单槽，B 的 turn 会覆盖 A（text_a 不再含技能正文），本断言失败。
    assert!(
        text_a.contains("ALPHA 技能正文"),
        "A 块含触发技能: {text_a}"
    );
    assert!(
        !text_b.contains("ALPHA 技能正文"),
        "B 块不含 A 的触发技能（串会话）"
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
        Arc::new(InstanceRegistry::new()),
    ));
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
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
        Arc::new(InstanceRegistry::new()),
    ));
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
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
        Arc::new(InstanceRegistry::new()),
    ));
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
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
        Arc::new(InstanceRegistry::new()),
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
        Arc::new(InstanceRegistry::new()),
    );
    let key = SessionKey::local_tui();
    let session = agent.trunk.get_or_create(&key, "user".into(), None);
    let reply = agent.process_message(&session, "hi").await.unwrap();
    assert_eq!(reply, "done");
}

// ── 工作区会话（workspace 插件）──

/// 给一个 agent 装上**完整的** workspace 插件（命令域 + 提示词块 + 门控）。
///
/// 为什么测试也要走真装配：工作区命令域已迁进插件（`Contribution::Command`），
/// `apply_command` 先问插件再落内置分支。只 `set_workspace_store` 而不装插件，
/// 测的是"宿主兜底分支"——那不是线上路径。
///
/// `AgentWorkspaceHost` 用"单元素 agent 快照"构造（生产传 manager 快照），
/// 同一份实现两种装配。
fn install_workspace_plugin(agent: &Arc<Agent>) {
    let host = Arc::new(
        crate::agent::workspace_host::AgentWorkspaceHost::from_agents(
            {
                let agent = Arc::clone(agent);
                move || vec![Arc::clone(&agent)]
            },
            Arc::new(|_event| {}),
            None,
        ),
    );
    let plugin_host = Arc::new(crate::plugins::PluginHost::new());
    // 宿主接缝经服务表（全插件化 S6：插件构造不再收参数）。
    let ctx = std::sync::Arc::new(echo_context::Ctx::default());
    let _keep = ctx
        .provide(&echo_plugin_workspace::host_keys::WORKSPACE_HOST, host)
        .expect("workspace host 注册");
    plugin_host.set_mount_ctx(echo_plugin::MountContext::new().with_ctx(ctx));
    plugin_host
        .register_and_mount(echo_plugin_workspace::plugin("test"))
        .expect("mount workspace plugin");
    agent.set_plugin_host(plugin_host);
}

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

    install_workspace_plugin(&agent);
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

                conversations: vec![],
                last_conversation: None,
            },
        })
        .await;
    // 激活。
    agent
        .apply_command(BackendCommand::ActivateWorkspaceSession {
            team_id: Some("t".into()),
            id: Some("core".into()),
            conversation: None,
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

    install_workspace_plugin(&agent);
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

                conversations: vec![],
                last_conversation: None,
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

                conversations: vec![],
                last_conversation: None,
            })
            .unwrap();
    }
    // 模拟持久化恢复：激活发生在进程装配之前。
    store.set_active(Some("core".into()), None).unwrap();

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
    // workspace_after_change 是异步 spawn（2026-10 会话级 API 恢复）——
    // 等一拍让钩子完成。
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

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

/// 多对话（2026-10）：激活指定对话 → 通道按 `<ws>/<conv>` 派生注册并广播；
/// 删除对话 → 通道历史经 store 钩子清空。
#[tokio::test]
async fn workspace_conversation_channels_and_delete_cleanup() {
    let store = Arc::new(crate::workspace::WorkspaceStore::load(None));
    store
        .upsert(echo_protocol::WorkspaceSessionInfo {
            id: "proj".into(),
            name: "Proj".into(),
            directories: vec!["/srv/proj".into()],
            ..Default::default()
        })
        .unwrap();
    let conv = store
        .upsert_conversation(
            "proj",
            echo_protocol::WorkspaceConversationInfo {
                id: String::new(),
                name: "支线".into(),
                legacy: false,
                api_profile: None,
            },
        )
        .unwrap();
    assert_eq!(conv.id, "conv", "CJK 名 slug 回退 conv");

    let agent = Arc::new(test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    })));
    agent.set_workspace_store(store.clone());

    // 激活指定对话 → 通道 id 带对话段，昵称 = 工作区 · 对话。
    store
        .set_active(Some("proj".into()), Some(conv.id.clone()))
        .unwrap();
    // workspace_after_change 是异步 spawn（2026-10 会话级 API 恢复）——
    // 等一拍让钩子完成。
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let channel = agent
        .trunk
        .all()
        .into_iter()
        .find(|s| s.id == "local:workspace:proj/conv:local_user")
        .expect("conversation channel registered");
    assert_eq!(channel.nickname, "Proj · 支线");

    // 该通道写一条事件（模拟对话历史），删除对话后必须清空。
    agent
        .trunk
        .append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some(channel.id.clone()),
                content: "支线记忆".into(),
                timestamp: 1700000001,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
    assert!(agent
        .trunk
        .event_log()
        .iter()
        .any(|e| e.session() == Some(channel.id.as_str())));

    store.delete_conversation("proj", &conv.id).unwrap();
    // 清空经 tokio::spawn 异步执行——等待其落地。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let cleared = agent
            .trunk
            .event_log()
            .iter()
            .all(|e| e.session() != Some(channel.id.as_str()));
        if cleared || std::time::Instant::now() > deadline {
            assert!(cleared, "删除对话后通道历史已清空");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // 激活工作区（不带对话）→ 惰性存量主对话走裸通道 id。
    store.set_active(Some("proj".into()), None).unwrap();
    assert!(
        agent
            .trunk
            .all()
            .iter()
            .any(|s| s.id == "local:workspace:proj:local_user"),
        "legacy main channel keeps the bare id"
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

            conversations: vec![],
            last_conversation: None,
        })
        .unwrap();
    store.set_active(Some("proj".into()), None).unwrap();
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
        Arc::new(InstanceRegistry::new()),
    ));
    // **文本块的来源是这个插件本身**（`Contribution::Text`）：把插件装上，
    // 内容按 `{"team_id", "session_id"}` 上下文算——与组合根里的接法一致。
    let host = Arc::new(crate::plugins::PluginHost::new());
    {
        let prompt_store = store.clone();
        let plugin = echo_plugin::BuiltinPlugin::new(
            echo_plugin::PluginManifest::builtin(
                crate::plugins::WORKSPACE_PLUGIN_ID,
                "工作区会话",
                "1",
                "tool",
                "workspace",
                "",
            ),
            |_| Ok(vec![]),
        )
        .with_text_blocks([echo_plugin::PluginTextBlock::new(
            "workspace",
            "工作区会话",
            50,
        )])
        .with_text_block_content(move |key, ctx| {
            if key != "workspace" {
                return None;
            }
            match ctx.get("session_id").and_then(|v| v.as_str()) {
                Some(session) if !session.is_empty() => prompt_store
                    .prompt_text_for(session)
                    .or_else(|| prompt_store.prompt_text()),
                _ => prompt_store.prompt_text(),
            }
        });
        host.register_and_mount(Arc::new(plugin)).expect("mount");
    }
    agent.set_plugin_host(host);
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
    crate::tool::builtin::coding::register_coding_tools(
        &mut tools,
        std::env::temp_dir(),
        None,
        None,
    );
    let agent = Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(InstanceRegistry::new()),
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
        Arc::new(InstanceRegistry::new()),
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
        Arc::new(InstanceRegistry::new()),
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
        Arc::new(InstanceRegistry::new()),
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
        Arc::new(InstanceRegistry::new()),
    );
    agent.set_config_store(echo_config::ConfigStore::new(path.clone()));
    // 文件里的 kimi 可见（否则会报"不存在（当前未配置任何…）"）
    assert!(agent.subagent_provider_for("kimi").await.is_ok());
    std::fs::remove_file(&path).ok();
}

// ── 流式输出（2026-10）──────────────────────────────────────────────

/// 流式桩：`chat_stream` 逐段发送增量并返回组装后的全量响应；
/// `chat` 返回同一份全量（用于 stream_output=false 对照）。
struct StreamingProvider {
    full: ChatResponse,
    chunks: Vec<ChatChunk>,
    stream_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmProvider for StreamingProvider {
    fn name(&self) -> &str {
        "streaming"
    }
    fn default_model(&self) -> &str {
        "streaming-model"
    }
    async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        Ok(self.full.clone())
    }
    async fn chat_stream(
        &self,
        _request: &ChatRequest,
        tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        for chunk in &self.chunks {
            let _ = tx.send(chunk.clone());
        }
        Ok(self.full.clone())
    }
}

fn streaming_agent(provider: Arc<StreamingProvider>) -> Arc<Agent> {
    Arc::new(Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(InstanceRegistry::new()),
    ))
}

/// 主对话流式（内置循环路径）：增量经 `AgentContentDelta` /
/// `AgentReasoningDelta` 到达，且**先于**终值 `AgentReasoning`/`AgentOutput`；
/// 终值为全量（与增量拼接一致，供前端对账）。
#[tokio::test]
async fn builtin_loop_forwards_streaming_deltas_before_final_events() {
    let provider = Arc::new(StreamingProvider {
        full: ChatResponse {
            content: Some("你好世界".into()),
            reasoning_content: Some("先想一想".into()),
            tool_calls: vec![],
            usage: Usage {
                prompt_tokens: 3,
                completion_tokens: 4,
                prompt_cache_hit_tokens: None,
                prompt_cache_miss_tokens: None,
                prompt_cache_creation_tokens: None,
            },
            stop_reason: None,
        },
        chunks: vec![
            ChatChunk {
                content_delta: None,
                reasoning_delta: Some("先想".into()),
                tool_call_delta: None,
            },
            ChatChunk {
                content_delta: None,
                reasoning_delta: Some("一想".into()),
                tool_call_delta: None,
            },
            ChatChunk {
                content_delta: Some("你好".into()),
                reasoning_delta: None,
                tool_call_delta: None,
            },
            ChatChunk {
                content_delta: Some("世界".into()),
                reasoning_delta: None,
                tool_call_delta: None,
            },
        ],
        stream_calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = streaming_agent(provider.clone());
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);

    assert_eq!(
        agent.process_message(&session, "hi").await.unwrap(),
        "你好世界"
    );
    assert_eq!(
        provider.stream_calls.load(Ordering::SeqCst),
        1,
        "主对话应走 chat_stream"
    );

    let mut events = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        events.push(event);
    }
    let content_deltas: String = events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::AgentContentDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    let reasoning_deltas: String = events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::AgentReasoningDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        content_deltas, "你好世界",
        "正文增量拼接应与全量一致: {events:?}"
    );
    assert_eq!(reasoning_deltas, "先想一想", "推理增量拼接应与全量一致");

    let pos = |pred: &dyn Fn(&BackendEvent) -> bool| events.iter().position(pred);
    let last_content_delta = events
        .iter()
        .rposition(|e| matches!(e, BackendEvent::AgentContentDelta { .. }))
        .expect("AgentContentDelta 缺失");
    let reasoning_full =
        pos(&|e| matches!(e, BackendEvent::AgentReasoning { .. })).expect("AgentReasoning 缺失");
    // process_message 内部以 AgentCompleted 收尾（AgentOutput 由命令层在
    // 其后发射；本测试验证 turn 内顺序：增量先于终值事件）。
    let completed =
        pos(&|e| matches!(e, BackendEvent::AgentCompleted { .. })).expect("AgentCompleted 缺失");
    let first_delta = pos(&|e| {
        matches!(
            e,
            BackendEvent::AgentContentDelta { .. } | BackendEvent::AgentReasoningDelta { .. }
        )
    })
    .expect("增量事件缺失");
    assert!(
        first_delta < reasoning_full && last_content_delta < completed,
        "增量必须先于终值（对账语义）: {events:?}"
    );
}

/// `stream_output=false`：主对话退回整段 `chat`——无增量事件、终值照常。
#[tokio::test]
async fn stream_output_disabled_skips_chat_stream_and_deltas() {
    let provider = Arc::new(StreamingProvider {
        full: ChatResponse {
            content: Some("整段回复".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
            stop_reason: None,
        },
        chunks: vec![ChatChunk {
            content_delta: Some("整段".into()),
            reasoning_delta: None,
            tool_call_delta: None,
        }],
        stream_calls: Arc::new(AtomicUsize::new(0)),
    });
    let config = AgentConfig {
        stream_output: false,
        ..AgentConfig::default()
    };
    let agent = Arc::new(Agent::new(
        provider.clone(),
        config,
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(InstanceRegistry::new()),
    ));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);

    assert_eq!(
        agent.process_message(&session, "hi").await.unwrap(),
        "整段回复"
    );
    assert_eq!(
        provider.stream_calls.load(Ordering::SeqCst),
        0,
        "stream_output=false 不得调用 chat_stream"
    );
    let mut has_delta = false;
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        if matches!(
            event,
            BackendEvent::AgentContentDelta { .. } | BackendEvent::AgentReasoningDelta { .. }
        ) {
            has_delta = true;
        }
    }
    assert!(!has_delta, "关闭流式后不应有增量事件");
}

/// 主对话流式（echo-loop 路径，生产路径）：ChatExecutor 的增量转发与内置
/// 循环同口径（增量先于终值；拼接与全量一致）。
#[tokio::test]
async fn echo_loop_path_forwards_streaming_deltas() {
    let provider = Arc::new(StreamingProvider {
        full: ChatResponse {
            content: Some("流式回复".into()),
            reasoning_content: Some("推理".into()),
            tool_calls: vec![],
            usage: Usage::default(),
            stop_reason: None,
        },
        chunks: vec![
            ChatChunk {
                content_delta: None,
                reasoning_delta: Some("推".into()),
                tool_call_delta: None,
            },
            ChatChunk {
                content_delta: Some("流式".into()),
                reasoning_delta: None,
                tool_call_delta: None,
            },
            ChatChunk {
                content_delta: Some("回复".into()),
                reasoning_delta: None,
                tool_call_delta: None,
            },
        ],
        stream_calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(InstanceRegistry::new()),
    ));
    let runner = Arc::new(echo_loop::runner::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    agent.set_loop_runner(runner);
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);

    assert_eq!(
        agent.process_message(&session, "hi").await.unwrap(),
        "流式回复"
    );
    assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);

    let mut events = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        events.push(event);
    }
    let content_deltas: String = events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::AgentContentDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    let reasoning_deltas: String = events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::AgentReasoningDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        content_deltas, "流式回复",
        "echo-loop 路径正文增量: {events:?}"
    );
    assert_eq!(reasoning_deltas, "推", "echo-loop 路径推理增量");
    let last_delta = events
        .iter()
        .rposition(|e| {
            matches!(
                e,
                BackendEvent::AgentContentDelta { .. } | BackendEvent::AgentReasoningDelta { .. }
            )
        })
        .unwrap();
    let reasoning_full = events
        .iter()
        .position(|e| matches!(e, BackendEvent::AgentReasoning { .. }))
        .expect("AgentReasoning 缺失");
    assert!(
        last_delta < reasoning_full,
        "增量应先于全量推理: {events:?}"
    );
}

/// 转发任务合并语义：达阈值立即分片、余量在关闭时 flush；拼接无损。
#[tokio::test]
async fn delta_forwarder_batches_and_flushes_on_close() {
    let received: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = received.clone();
    let emit: Arc<dyn Fn(BackendEvent) + Send + Sync> = Arc::new(move |event: BackendEvent| {
        if let BackendEvent::AgentContentDelta { delta, .. } = event {
            sink.lock().unwrap().push(delta);
        }
    });
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ChatChunk>();
    let task = super::streaming::spawn_delta_forwarder(rx, emit, "s".into(), "b".into());
    tx.send(ChatChunk {
        content_delta: Some("abc".into()),
        reasoning_delta: None,
        tool_call_delta: None,
    })
    .unwrap();
    tx.send(ChatChunk {
        content_delta: Some("x".repeat(300)),
        reasoning_delta: None,
        tool_call_delta: None,
    })
    .unwrap();
    drop(tx);
    task.await.unwrap();
    let events = received.lock().unwrap().clone();
    let total: String = events.concat();
    assert_eq!(total, format!("abc{}", "x".repeat(300)));
    assert!(events.len() <= 2, "窗口合并应有界: {}", events.len());
}

/// 2026-10 单轨化回归：QQ hook / 定时器 / QQ 会话后台输入原走内置循环，
/// 现与普通输入同经 echo-loop——提示词边界块必须照常注入，普通输入不注入。
#[tokio::test]
async fn single_track_loop_injects_input_boundary_blocks() {
    let final_reply = || ChatResponse {
        stop_reason: None,
        content: Some("ok".into()),
        reasoning_content: None,
        tool_calls: vec![],
        usage: Usage::default(),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        final_reply(),
        final_reply(),
        final_reply(),
    ]));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(InstanceRegistry::new()),
    ));
    let local = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    let qq = agent.trunk.get_or_create(
        &SessionKey::parse("qq:dm::123456").unwrap(),
        "tester".into(),
        None,
    );

    agent.process_message(&local, "普通输入").await.unwrap();
    agent
        .process_message(
            &local,
            r#"<qq_message_hook>{"message_sequence":7,"content":"hi","images":[]}</qq_message_hook>"#,
        )
        .await
        .unwrap();
    agent
        .process_message(&qq, "后台对 QQ 会话说话")
        .await
        .unwrap();

    let requests = provider.requests.lock().await.clone();
    assert_eq!(requests.len(), 3, "三条输入都经 echo-loop 各请求一次");
    let system = |i: usize| requests[i].messages[0].content.clone();
    assert!(
        !system(0).contains("# Platform message boundary"),
        "普通本地输入不注入平台边界"
    );
    assert!(
        system(1).contains("# Platform message boundary (qq)"),
        "平台 hook 输入必须注入平台边界块（平台名来自标记）: {}",
        system(1)
    );
    let backend_block = super::boundary::BoundaryKind::BackendInput.block().content;
    assert!(
        system(2).contains(backend_block.lines().next().unwrap_or_default()),
        "QQ 会话的后台输入必须注入后台边界块"
    );
}

/// 泛化 Core-1（01a S3）：`emit_instance_list` 填充 `platform` / `ui` /
/// `session_labels`（后两者经 PluginInstance trait 访问器取贡献原样）。
#[tokio::test]
async fn instance_list_fills_platform_ui_and_session_labels() {
    struct UiFakeInstance;

    #[async_trait::async_trait]
    impl echo_defs::plugin_instance::PluginInstance for UiFakeInstance {
        fn name(&self) -> &str {
            "fake-default"
        }
        fn display_name(&self) -> &str {
            "Fake / UI"
        }
        fn platform(&self) -> &str {
            "fake"
        }
        fn is_configured(&self) -> bool {
            true
        }
        fn status_info(&self) -> echo_defs::plugin_instance::InstanceInfo {
            echo_defs::plugin_instance::InstanceInfo {
                name: "fake-default".into(),
                display_name: "Fake / UI".into(),
                status: echo_defs::plugin_instance::InstanceConnectionState::Connected,
                self_id: Some("1".into()),
                started_at: None,
                platform: "fake".into(),
                configured: true,
            }
        }
        async fn start(&self) -> Result<(), echo_defs::plugin_instance::InstanceError> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), echo_defs::plugin_instance::InstanceError> {
            Ok(())
        }
        fn ui_descriptor(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({"version": 1, "panels": []}))
        }
        fn session_labels(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({"dm": "私聊", "group": "群聊"}))
        }
    }

    let registry = InstanceRegistry::new();
    registry.register(Arc::new(UiFakeInstance));
    let agent = Agent::new(
        Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        }),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(registry),
    );
    let received = Arc::new(std::sync::Mutex::new(Vec::<BackendEvent>::new()));
    let sink_events = received.clone();
    agent.attach_event_sink(Arc::new(move |event: BackendEvent| {
        sink_events.lock().expect("events poisoned").push(event);
    }));

    let instance_list = crate::agent::instance_list_event(&agent);
    agent.emit(instance_list);

    let events = received.lock().expect("events poisoned").clone();
    let instances = events
        .iter()
        .find_map(|event| match event {
            BackendEvent::InstanceList { instances } => Some(instances),
            _ => None,
        })
        .expect("InstanceList emitted");
    assert_eq!(instances.len(), 1);
    let value = serde_json::to_value(&instances[0]).unwrap();
    assert_eq!(value["platform"], serde_json::json!("fake"));
    assert_eq!(value["ui"]["version"], serde_json::json!(1));
    assert_eq!(value["session_labels"]["dm"], serde_json::json!("私聊"));
}

// ── 宿主 → 插件的事件投递（plugin-framework v0.7 接线）──

/// `Agent::emit` 是**唯一**的领域事件汇聚点：订阅了某事件名的插件应当收到它，
/// 且热路径闸门（无人订阅时一次原子 load 返回）在挂上订阅者后打开。
///
/// 这条测试走真实 `Agent::emit`，不是直接调 `PluginHost`——接线的价值就在
/// "emit 的输出真的流到插件"，只测 PluginHost 盖不住那个接缝。
#[tokio::test]
async fn agent_emit_delivers_to_subscribed_plugins() {
    let agent = test_agent(Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "x".into(),
    }));
    let host = Arc::new(crate::plugins::PluginHost::new());
    host.set_mount_ctx(echo_plugin::MountContext::new());
    agent.set_plugin_host(host.clone());

    // 无人订阅：闸门关闭（这是现网全部部署的状态）。
    assert!(!host.has_event_subscribers());

    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    host.register_and_mount(Arc::new(
        echo_plugin::BuiltinPlugin::new(
            echo_plugin::PluginManifest::builtin("obs.tool", "o", "1", "tool", "", ""),
            |_| Ok(vec![]),
        )
        .with_subscriptions(["ToolResult"])
        .with_event_handler(move |event: &echo_plugin::PluginEvent| {
            sink.lock().unwrap().push(event.name.clone());
            Ok(())
        }),
    ))
    .expect("mount ok");
    assert!(host.has_event_subscribers(), "挂上订阅者后闸门应打开");

    // 订阅的事件 → 到达。
    agent.emit(echo_protocol::BackendEvent::ToolResult {
        session_id: "s1".into(),
        team_id: None,
        tool_name: "bash".into(),
        result: "ok".into(),
        tool_call_id: String::new(),
        timed_out: false,
        branch_id: String::new(),
        elapsed_ms: Some(12),
    });
    assert_eq!(*seen.lock().unwrap(), vec!["ToolResult".to_string()]);

    // 没订阅的事件 → 被名字闸门挡掉（连序列化都不做）。
    agent.emit(echo_protocol::BackendEvent::Error {
        session_id: None,
        message: "boom".into(),
    });
    assert_eq!(seen.lock().unwrap().len(), 1, "未订阅的事件不该到达插件");
    assert_eq!(host.event_counters().0, 1, "只有那一条进成功计数");
}


/// 2026-10 scope audit: the context snapshot after clear/archive must go to
/// the operated session.
///
/// It used to go through `emit_context_snapshot_for()` (=
/// `emit_context_snapshot(None)`), which resolved to `local:tui` or "any
/// existing session". So clearing a non-local session left the panel showing
/// another session's snapshot — the current window never refreshed and looked
/// like "the clear did nothing".
#[tokio::test]
async fn clear_history_snapshot_targets_the_operated_session() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = Arc::new(test_agent(provider));
    agent.set_team_id(Some("t".into()));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    // Two sessions: the one being cleared (sA) and an unrelated sB — the
    // "any existing session" fallback would pick sB.
    for (session, text) in [("sA", "A"), ("sB", "B")] {
        agent.trunk.import_session_events(
            session,
            vec![echo_session::SessionEvent::UserMessage(
                echo_session::event::UserMessage {
                    session: Some(session.into()),
                    content: text.into(),
                    timestamp: 1,
                    message_sequence: None,
                    source: None,
                    images: vec![],
                },
            )],
        );
    }

    agent
        .apply_command(BackendCommand::ClearHistory {
            team_id: Some("t".into()),
            session_id: Some("sA".into()),
        })
        .await;

    let mut snapshots = Vec::new();
    let mut notices = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        match event {
            BackendEvent::ContextSnapshot { session_id, .. } => snapshots.push(session_id),
            BackendEvent::Notice {
                session_id,
                ok,
                message,
                ..
            } => notices.push((session_id, ok, message)),
            _ => {}
        }
    }
    assert_eq!(
        snapshots,
        vec![Some("sA".to_string())],
        "snapshot must target the cleared session, not another one"
    );
    assert!(
        notices
            .iter()
            .any(|(session, ok, message)| session.as_deref() == Some("sA")
                && *ok
                && message.contains("已清理")),
        "clear result must be a session-scoped Notice: {notices:?}"
    );
}

/// 2026-10 scope audit: a clear without `session_id` is refused.
///
/// The old semantics were "wipe every session of this persona" (event log +
/// projections + timeline) — irreversible, and no UI entry needs it (the panel
/// sends the session from both "clear history" and "delete conversation").
#[tokio::test]
async fn clear_history_without_session_id_is_rejected_fail_closed() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = Arc::new(test_agent(provider));
    agent.set_team_id(Some("t".into()));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    agent.trunk.import_session_events(
        "sA",
        vec![echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some("sA".into()),
                content: "keep me".into(),
                timestamp: 1,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        )],
    );

    agent
        .apply_command(BackendCommand::ClearHistory {
            team_id: Some("t".into()),
            session_id: None,
        })
        .await;

    let mut messages = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        if let BackendEvent::Error { message, .. } = event {
            messages.push(message);
        }
    }
    assert!(
        messages.iter().any(|m| m.contains("需要指定会话")),
        "missing session must be refused: {messages:?}"
    );
    assert_eq!(
        agent.trunk.history_for("sA").map(|h| h.lock().len()),
        Some(1),
        "refused command must not wipe any session"
    );
}

/// 2026-10 scope audit: session-scoped operation receipts use `Notice`
/// (session + ok), not `Error` — `Error{session_id}` freezes that session's
/// streaming rows in the panel, and compaction is a background maintenance
/// operation.
#[tokio::test]
async fn compact_result_is_a_session_scoped_notice() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "【当前目标】x".into(),
    });
    let agent = Arc::new(test_agent(provider));
    agent.set_team_id(Some("t".into()));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    // `keep_recent` is clamped to [10, 500], so the log must exceed 10 events.
    let session = "sA";
    agent.trunk.import_session_events(
        session,
        (0..20)
            .map(|i| {
                echo_session::SessionEvent::UserMessage(echo_session::event::UserMessage {
                    session: Some(session.into()),
                    content: format!("m{i}"),
                    timestamp: i,
                    message_sequence: None,
                    source: None,
                    images: vec![],
                })
            })
            .collect(),
    );

    agent
        .apply_command(BackendCommand::CompactHistory {
            team_id: Some("t".into()),
            session_id: Some(session.into()),
            keep_recent: Some(2),
        })
        .await;

    let mut notices = Vec::new();
    let mut errors = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        match event {
            BackendEvent::Notice {
                session_id,
                ok,
                message,
                ..
            } => notices.push((session_id, ok, message)),
            BackendEvent::Error { message, .. } => errors.push(message),
            _ => {}
        }
    }
    assert!(
        errors.is_empty(),
        "the result must not go through Error (the panel treats it as a session failure): {errors:?}"
    );
    let (session_id, ok, _message) = notices
        .iter()
        .find(|(_, _, message)| message.contains("已压缩"))
        .cloned()
        .unwrap_or_else(|| panic!("compaction result must be a Notice: {notices:?}"));
    assert_eq!(session_id.as_deref(), Some(session), "receipt must carry the session");
    assert!(ok, "successful receipt has ok=true");
}

/// 2026-10 scope audit: a compaction without `session_id` is refused instead of
/// degrading to "compact every session of this persona".
#[tokio::test]
async fn compact_without_session_id_is_rejected_fail_closed() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let agent = Arc::new(test_agent(provider));
    agent.set_team_id(Some("t".into()));
    let (bridge, handle) = crate::create_bridge();
    agent.attach(Arc::new(handle));

    agent.trunk.import_session_events(
        "sA",
        (0..20)
            .map(|i| {
                echo_session::SessionEvent::UserMessage(echo_session::event::UserMessage {
                    session: Some("sA".into()),
                    content: format!("m{i}"),
                    timestamp: i,
                    message_sequence: None,
                    source: None,
                    images: vec![],
                })
            })
            .collect(),
    );

    agent
        .apply_command(BackendCommand::CompactHistory {
            team_id: Some("t".into()),
            session_id: None,
            keep_recent: Some(2),
        })
        .await;

    let mut messages = Vec::new();
    while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
        if let BackendEvent::Error { message, .. } = event {
            messages.push(message);
        }
    }
    assert!(
        messages.iter().any(|m| m.contains("需要指定会话")),
        "missing session must be refused: {messages:?}"
    );
    let compacted = agent
        .trunk
        .event_log()
        .iter()
        .any(|e| matches!(e, echo_session::SessionEvent::Compaction(_)));
    assert!(!compacted, "a refused command must not land any compaction");
}

/// 2026-10 scope audit: deleting an API profile also drops the session-level
/// overrides that point at it (otherwise those sessions keep resolving a
/// profile that no longer exists).
#[tokio::test]
async fn deleting_api_profile_drops_session_overrides() {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    let seed: Arc<dyn LlmProvider> = provider.clone();
    let agent = Arc::new(test_agent(provider));
    agent.set_team_id(Some("t".into()));
    // Seed an override without going through provider construction.
    agent
        .model
        .session_providers
        .insert("sA".into(), ("gone".to_string(), seed));
    let dropped = agent.drop_session_providers_of("gone").await;
    assert_eq!(dropped, 1, "the override pointing at the profile is dropped");
    assert!(
        agent.model.session_providers.get("sA").is_none(),
        "no stale override may survive"
    );
}
