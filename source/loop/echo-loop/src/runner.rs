//! The default turn runner: a turn/step state machine driving the model
//! request and tool execution loop.

use std::sync::Arc;

use echo_context::{DispatchMode, EventBus};
use echo_defs::message::{ChatMessage, ChatRequest, ChatResponse, ToolCall};
use echo_defs::LlmProvider;
use thiserror::Error;

use crate::event::{
    AgentPreStep, AgentRequest, StepEnd, StepStart, ToolCallRequested, ToolResult, TurnEnd,
    TurnStart, TurnStopping,
};
use crate::pipeline::ToolPipeline;

/// Loop driver errors.
#[derive(Debug, Error)]
pub enum LoopError {
    #[error("model request failed: {0}")]
    Model(String),
    #[error("turn cancelled")]
    Cancelled,
    #[error("reached max tool iterations ({0}) without a final reply")]
    MaxIterations(usize),
    #[error("step rejected by pre-step listener")]
    StepRejected,
}

/// Turn runner configuration.
#[derive(Debug, Clone)]
pub struct LoopOptions {
    /// Upper bound on model-request iterations per turn (0 still allows one
    /// direct reply without tools).
    pub max_tool_iterations: usize,
    /// Per-tool execution timeout.
    pub tool_timeout: std::time::Duration,
}

impl Default for LoopOptions {
    fn default() -> Self {
        Self {
            max_tool_iterations: 1024,
            tool_timeout: std::time::Duration::from_secs(120),
        }
    }
}

/// How the runner executes one tool call. The harness owns the concrete
/// executor (registry lookup + orchestration tools); the runner only drives
/// the lifecycle around it. 引用式（非 Arc/`'static`）：executor 可与调用方
/// 的会话状态绑定（由 agent 提供 &self 闭包）。
pub type ToolExecutor<'a> = &'a (dyn Fn(&str, &str, &ToolCall) -> String + Send + Sync);

/// run 的扩展参数：工具 schema（发给模型）与推理回调（转发至 UI）。
#[derive(Default, Clone)]
pub struct RunExtras<'a> {
    /// 模型可见的工具定义（None = 无工具请求，纯对话）。
    pub tools: Option<Vec<echo_defs::tool::ToolDefinition>>,
    /// 推理回调：（session_id, reasoning_text）。None = 忽略。
    /// 引用式（与 executor 相同哲学）：回调可与调用方的会话状态绑定。
    pub on_reasoning: Option<&'a (dyn Fn(String, String) + Send + Sync)>,
}

/// The default agent-loop driver.
///
/// Drives one turn through steps:
///
/// ```text
/// turn/start
///   agent/pre-step (waterfall: rewrite or reject)
///   step/start
///   agent/request (waterfall) -> llm.chat
///   tool/call* -> pipeline (pre-execute -> execute -> post-execute)
///   step/end
///   (another request is owed? -> next step)
/// agent/turn-stopping
/// turn/end
/// ```
///
/// Every lifecycle event is dispatched on the bus; listeners can observe or
/// intercept (dsh extension points). The tool pipeline carries policy
/// middleware; the runner itself contains no timeout/approval/audit code.
pub struct TurnRunner {
    bus: Arc<EventBus>,
    llm: Arc<dyn LlmProvider>,
    pipeline: Arc<ToolPipeline>,
    options: LoopOptions,
}

impl TurnRunner {
    pub fn new(
        bus: Arc<EventBus>,
        llm: Arc<dyn LlmProvider>,
        pipeline: Arc<ToolPipeline>,
        options: LoopOptions,
    ) -> Self {
        Self {
            bus,
            llm,
            pipeline,
            options,
        }
    }

    /// Drive one turn: admitted input → final reply (or error).
    ///
    /// `execute_tool` is the harness's tool executor (called through the
    /// pipeline). `history` is the point-in-time model context; the runner
    /// appends assistant/tool messages to it as steps progress.
    pub async fn run(
        &self,
        session_id: &str,
        input: String,
        system_prompt: String,
        history: Vec<ChatMessage>,
        cancel: tokio_util::sync::CancellationToken,
        execute_tool: ToolExecutor<'_>,
        extras: RunExtras<'_>,
    ) -> Result<String, LoopError> {
        self.bus.emit_sync(
            TurnStart {
                session_id: session_id.into(),
                input: input.clone(),
            },
            DispatchMode::Observe,
        );

        // agent/pre-step: listeners may rewrite the message list or reject.
        let pre_step = AgentPreStep {
            session_id: session_id.into(),
            messages: {
                let mut messages = vec![ChatMessage::system(system_prompt)];
                messages.extend(history);
                messages
            },
            accept: true,
        };
        let pre_step = self.bus.emit_sync(pre_step, DispatchMode::Waterfall);
        if !pre_step.accept {
            self.bus.emit_sync(
                TurnEnd {
                    session_id: session_id.into(),
                },
                DispatchMode::Observe,
            );
            return Err(LoopError::StepRejected);
        }
        let mut messages = pre_step.messages;

        let model = self.llm.default_model().to_string();
        let max_iterations = self.options.max_tool_iterations.max(1);

        for step_index in 0..max_iterations {
            if cancel.is_cancelled() {
                return Err(LoopError::Cancelled);
            }
            self.bus.emit_sync(
                StepStart {
                    session_id: session_id.into(),
                    step_index,
                },
                DispatchMode::Observe,
            );

            // agent/request: listeners may rewrite the request.
            let request = AgentRequest {
                session_id: session_id.into(),
                request: ChatRequest {
                    model: model.clone(),
                    messages: messages.clone(),
                    tools: extras.tools.clone(),
                    temperature: None,
                    max_tokens: None,
                },
            };
            let request = self.bus.emit_sync(request, DispatchMode::Waterfall);

            let response = self
                .llm
                .chat(&request.request)
                .await
                .map_err(|e| LoopError::Model(e.to_string()))?;
            if let Some(ref cb) = extras.on_reasoning {
                if let Some(ref text) = response.reasoning_content {
                    if !text.trim().is_empty() {
                        cb(session_id.to_string(), text.clone());
                    }
                }
            }

            if response.tool_calls.is_empty() {
                let reply = response.content.unwrap_or_default();
                self.bus.emit_sync(
                    StepEnd {
                        session_id: session_id.into(),
                        step_index,
                    },
                    DispatchMode::Observe,
                );
                self.bus.emit_sync(
                    TurnStopping {
                        session_id: session_id.into(),
                    },
                    DispatchMode::Observe,
                );
                self.bus.emit_sync(
                    TurnEnd {
                        session_id: session_id.into(),
                    },
                    DispatchMode::Observe,
                );
                return Ok(reply);
            }

            // The model wants tools: record the assistant turn, execute each
            // call through the pipeline, feed results back.
            messages.push(assistant_with_tool_calls(&response));
            for call in &response.tool_calls {
                if cancel.is_cancelled() {
                    return Err(LoopError::Cancelled);
                }
                self.bus.emit_sync(
                    ToolCallRequested {
                        session_id: session_id.into(),
                        call: call.clone(),
                    },
                    DispatchMode::Observe,
                );
                let session_id_owned = session_id.to_string();
                let call_for_executor = call.clone();
                let execute_tool = execute_tool.clone();
                let result = self
                    .pipeline
                    .run(call, move || {
                        let session_id = session_id_owned.clone();
                        let call = call_for_executor.clone();
                        let execute_tool = execute_tool.clone();
                        Box::pin(async move { execute_tool(&session_id, "", &call) })
                    })
                    .await;
                let result_text = match result {
                    crate::pipeline::ToolPipelineResult::ShortCircuit(text) => text,
                    crate::pipeline::ToolPipelineResult::Continue => String::new(),
                };
                self.bus.emit_sync(
                    ToolResult {
                        session_id: session_id.into(),
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        result: result_text.clone(),
                    },
                    DispatchMode::Observe,
                );
                messages.push(ChatMessage::tool(result_text, &call.id));
            }
            self.bus.emit_sync(
                StepEnd {
                    session_id: session_id.into(),
                    step_index,
                },
                DispatchMode::Observe,
            );
        }

        self.bus.emit_sync(
            TurnStopping {
                session_id: session_id.into(),
            },
            DispatchMode::Observe,
        );
        self.bus.emit_sync(
            TurnEnd {
                session_id: session_id.into(),
            },
            DispatchMode::Observe,
        );
        Err(LoopError::MaxIterations(max_iterations))
    }
}

/// Build the assistant message carrying tool calls (content + reasoning).
fn assistant_with_tool_calls(response: &ChatResponse) -> ChatMessage {
    let mut message = ChatMessage::assistant_with_reasoning(
        response.content.clone().unwrap_or_default(),
        response.reasoning_content.clone(),
    );
    if !response.tool_calls.is_empty() {
        message.tool_calls = Some(response.tool_calls.clone());
    }
    message
}
