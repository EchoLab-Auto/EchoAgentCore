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
    #[error("output truncated by token limit {0} times in one turn; giving up")]
    Truncated(usize),
}

/// Turn runner configuration.
#[derive(Debug, Clone)]
pub struct LoopOptions {
    /// Upper bound on model-request iterations per turn (0 still allows one
    /// direct reply without tools).
    pub max_tool_iterations: usize,
    /// Per-tool execution timeout.
    pub tool_timeout: std::time::Duration,
    /// 单次模型请求的 completion 预算（max_tokens）。`None` = 无上限
    /// （后端回退到 echo_defs::message::DEFAULT_MAX_TOKENS，128K）。
    pub max_tokens: Option<u32>,
}

impl Default for LoopOptions {
    fn default() -> Self {
        Self {
            max_tool_iterations: 1024,
            tool_timeout: std::time::Duration::from_secs(120),
            max_tokens: None,
        }
    }
}

/// 一轮内允许的自动续跑次数上限：输出被 max_tokens 截断时把残片入栈并
/// 让模型接着写。超过上限说明模型陷入"每轮都写满预算"的循环，按错误上报，
/// 不再静默重试。
const MAX_TRUNCATION_CONTINUES: usize = 4;

/// 截断续跑时喂给模型的提示（user 角色，区别于真实用户输入）。
const TRUNCATION_CONTINUE_PROMPT: &str = "[system notice] Your previous output was cut off by the max token limit before the turn was complete. Continue exactly from where you stopped. If you were composing a tool call, discard the partial call and re-issue it in full.";

/// How the runner executes one tool call. The harness owns the concrete
/// executor (registry lookup + orchestration tools); the runner only drives
/// the lifecycle around it. 引用式（非 Arc/`'static`）：executor 可与调用方
/// 的会话状态绑定（由 agent 提供 &self 闭包）。
pub type ToolExecutor<'a> = &'a (dyn Fn(&str, &str, &ToolCall) -> String + Send + Sync);

/// 异步版工具执行器：异步编排工具（如 `spawn_subagent`）经此通道进入管线。
///
/// 签名是「同步返回 future 句柄」而非 async 闭包：harness 侧的编排通常需要
/// tokio::spawn 一个后台任务，其结果经 oneshot 通道传回——这避免了
/// `async Fn` 闭包捕获引用时未来生命周期无法表达为 Send 的经典困境。
pub type AsyncToolExecutor<'a> = &'a (dyn Fn(
    &str,
    &str,
    &ToolCall,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
         + Send
         + Sync);

/// 异步编排工具的内联处理钩子（subagent 等）。
///
/// loop 拥有"先管线拦截、后回退注册表"的分派点——不在 agent 层的
/// `run_tool` 里硬编码特判；其他异步编排工具将来复用同一组钩子。
/// 默认实现全部 no-op（无异步编排工具时零成本）。
pub struct SubagentToolHooks<'a> {
    /// 判断某工具是否应由异步通道处理（如 `spawn_subagent`）。
    pub is_async_tool: Option<&'a (dyn Fn(&str) -> bool + Send + Sync)>,
    /// 异步执行一个被 [`Self::is_async_tool`] 认领的工具。
    pub execute_async: Option<AsyncToolExecutor<'a>>,
    /// 模型可见的异步编排工具 schema（追加在注册表定义之后）。
    pub extra_tool_definitions:
        Option<&'a (dyn Fn() -> Vec<echo_defs::tool::ToolDefinition> + Send + Sync)>,
}

impl std::fmt::Debug for SubagentToolHooks<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentToolHooks")
            .field("is_async_tool", &self.is_async_tool.is_some())
            .field("execute_async", &self.execute_async.is_some())
            .field(
                "extra_tool_definitions",
                &self.extra_tool_definitions.is_some(),
            )
            .finish()
    }
}

impl Default for SubagentToolHooks<'_> {
    fn default() -> Self {
        Self {
            is_async_tool: None,
            execute_async: None,
            extra_tool_definitions: None,
        }
    }
}

/// run 的扩展参数：工具 schema（发给模型）与推理回调（转发至 UI）。
#[derive(Default)]
pub struct RunExtras<'a> {
    /// 模型可见的工具定义（None = 无工具请求，纯对话）。
    pub tools: Option<Vec<echo_defs::tool::ToolDefinition>>,
    /// 推理回调：（session_id, reasoning_text）。None = 忽略。
    /// 引用式（与 executor 相同哲学）：回调可与调用方的会话状态绑定。
    pub on_reasoning: Option<&'a (dyn Fn(String, String) + Send + Sync)>,
    /// 异步编排工具钩子（subagent 等；默认无）。
    pub subagent_hooks: SubagentToolHooks<'a>,
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
        let mut truncation_continues = 0usize;

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
            // 异步编排工具（spawn_subagent 等）的 schema 由 hook 追加在
            // 注册表定义之后——模型可见性与注册表工具同源同帧。
            let tools = match (&extras.tools, &extras.subagent_hooks.extra_tool_definitions) {
                (Some(base), Some(extra_defs)) => {
                    let extra = extra_defs();
                    if extra.is_empty() {
                        Some(base.clone())
                    } else {
                        let mut merged = base.clone();
                        merged.extend(extra);
                        Some(merged)
                    }
                }
                (tools, _) => tools.clone(),
            };
            let request = AgentRequest {
                session_id: session_id.into(),
                request: ChatRequest {
                    model: model.clone(),
                    messages: messages.clone(),
                    tools,
                    temperature: None,
                    max_tokens: self.options.max_tokens,
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

            // 输出被 token 上限截断：残片（含未写完的工具调用）不能当作
            // 正常收尾——丢弃半截工具调用，把已生成的文本入栈，让模型续写。
            // 历史上这里直接 TurnEnd，表现为"agent 自己断掉、空回复"。
            if response.truncated() {
                truncation_continues += 1;
                tracing::warn!(
                    session = %session_id,
                    step_index,
                    truncation_continues,
                    stop_reason = ?response.stop_reason,
                    "model output truncated at token limit; continuing"
                );
                if truncation_continues > MAX_TRUNCATION_CONTINUES {
                    return Err(LoopError::Truncated(MAX_TRUNCATION_CONTINUES));
                }
                if let Some(text) = &response.content {
                    if !text.trim().is_empty() {
                        messages.push(ChatMessage::assistant_with_reasoning(
                            text.clone(),
                            response.reasoning_content.clone(),
                        ));
                    }
                }
                messages.push(ChatMessage::user(TRUNCATION_CONTINUE_PROMPT));
                continue;
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
                let is_async_tool = extras.subagent_hooks.is_async_tool;
                let execute_async = extras.subagent_hooks.execute_async;
                let result = self
                    .pipeline
                    .run(call, move || {
                        let session_id = session_id_owned.clone();
                        let call = call_for_executor.clone();
                        let execute_tool = execute_tool.clone();
                        Box::pin(async move {
                            if is_async_tool.is_some_and(|f| f(&call.name)) {
                                if let Some(exec) = execute_async {
                                    return exec(&session_id, "", &call).await;
                                }
                            }
                            execute_tool(&session_id, "", &call)
                        })
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
