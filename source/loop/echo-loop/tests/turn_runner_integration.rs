//! TurnRunner end-to-end: a mock LLM drives the turn/step lifecycle.

use std::sync::Arc;

use async_trait::async_trait;
use echo_context::{EventBus, WaterfallDecision};
use echo_defs::llm::LlmError;
use echo_defs::llm::LlmProvider;
use echo_defs::message::{ChatChunk, ChatRequest, ChatResponse, ToolCall, Usage};
use echo_loop::{
    AgentPreStep, AgentRequest, StepStart, ToolCallRequested, ToolResult, TurnEnd, TurnRunner,
    TurnStart,
};

/// A scripted provider: first call requests a tool, second returns the reply.
struct ScriptedProvider {
    script: tokio::sync::Mutex<std::collections::VecDeque<ChatResponse>>,
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }
    fn default_model(&self) -> &str {
        "scripted-model"
    }
    async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, LlmError> {
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

fn scripted(script: Vec<ChatResponse>) -> Arc<ScriptedProvider> {
    Arc::new(ScriptedProvider {
        script: tokio::sync::Mutex::new(script.into()),
    })
}

#[tokio::test]
async fn turn_emits_lifecycle_events_in_order() {
    let bus: Arc<EventBus> = Arc::new(EventBus::default());
    let llm = scripted(vec![
        ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "calc".into(),
                arguments: r#"{"expr":"1+1"}"#.into(),
            }],
            usage: Usage::default(),
        },
        ChatResponse {
            content: Some("结果是 2".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        },
    ]);
    let pipeline: Arc<echo_loop::ToolPipeline> = Arc::new(echo_loop::ToolPipeline::new());
    let runner = TurnRunner::new(
        bus.clone(),
        llm,
        pipeline,
        echo_loop::LoopOptions::default(),
    );

    let lifecycle = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = lifecycle.clone();
    let _d1 = bus.observe::<TurnStart, _>(move |_| seen.lock().unwrap().push("turn/start"));
    let seen = lifecycle.clone();
    let _d2 = bus.observe::<AgentPreStep, _>(move |_| seen.lock().unwrap().push("agent/pre-step"));
    let seen = lifecycle.clone();
    let _d3 = bus.observe::<StepStart, _>(move |_| seen.lock().unwrap().push("step/start"));
    let seen = lifecycle.clone();
    let _d4 = bus.observe::<AgentRequest, _>(move |_| seen.lock().unwrap().push("agent/request"));
    let seen = lifecycle.clone();
    let _d5 = bus.observe::<ToolCallRequested, _>(move |_| seen.lock().unwrap().push("tool/call"));
    let seen = lifecycle.clone();
    let _d6 = bus.observe::<ToolResult, _>(move |_| seen.lock().unwrap().push("tool/result"));
    let seen = lifecycle.clone();
    let _d7 = bus.observe::<TurnEnd, _>(move |_| seen.lock().unwrap().push("turn/end"));

    let reply = runner
        .run(
            "qq:dm::1",
            "计算 1+1".into(),
            "系统提示".into(),
            vec![],
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_s, _b, call| format!("执行了 {}", call.name)),
        )
        .await
        .unwrap();
    assert_eq!(reply, "结果是 2");

    let order = lifecycle.lock().unwrap().clone();
    assert!(
        order
            .windows(3)
            .any(|w| w == ["turn/start", "agent/pre-step", "step/start"]),
        "turn opens before steps: {order:?}"
    );
    assert!(
        order
            .windows(3)
            .any(|w| w == ["tool/call", "tool/result", "step/start"]),
        "second step follows the tool result: {order:?}"
    );
    assert_eq!(
        order.last().map(|s| &s[..]),
        Some("turn/end"),
        "turn closes: {order:?}"
    );
}

#[tokio::test]
async fn pre_step_rejection_closes_turn_without_request() {
    let bus: Arc<EventBus> = Arc::new(EventBus::default());
    let llm = scripted(vec![]); // any request would fail
    let runner = TurnRunner::new(
        bus.clone(),
        llm,
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    );
    // A waterfall listener rejects the step.
    let _reject = bus.subscribe::<AgentPreStep, _>(|pre_step, _| {
        pre_step.accept = false;
        WaterfallDecision::ShortCircuit
    });
    let result = runner
        .run(
            "s1",
            "hi".into(),
            "sys".into(),
            vec![],
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_, _, _| String::new()),
        )
        .await;
    assert!(matches!(result, Err(echo_loop::LoopError::StepRejected)));
}
