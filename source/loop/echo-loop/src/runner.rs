//! The default turn runner: a turn/step state machine driving the model
//! request and tool execution loop.

use std::sync::Arc;

use echo_context::{DispatchMode, EventBus};
use echo_defs::message::{ChatMessage, ChatRequest, ChatResponse, ToolCall};
use echo_defs::LlmProvider;
use thiserror::Error;

use crate::event::{
    AgentPreStep, AgentRequest, ModelResponse, StepEnd, StepStart, ToolCallRequested, ToolResult,
    TurnEnd, TurnStart, TurnStopping,
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
    /// 单次模型请求的 completion 预算（max_tokens）。`None` = 无上限
    /// （后端回退到 echo_defs::message::DEFAULT_MAX_TOKENS，128K）。
    ///
    /// 运行期预算变更：harness 的 [`ChatExecutor`] 在每步可自行改写
    /// 请求的 `max_tokens`（本字段仅作缺省值）。
    pub max_tokens: Option<u32>,
}

impl Default for LoopOptions {
    fn default() -> Self {
        Self {
            max_tool_iterations: 1024,
            max_tokens: None,
        }
    }
}

// 截断续跑常量：口径单源 = echo_defs::llm（2026-10 巡检：与内置循环
// 双写已漂移过——统一后两处引用同一常量）。
use echo_defs::llm::{MAX_TRUNCATION_CONTINUES, TRUNCATION_CONTINUE_PROMPT};

/// 工具执行输出：模型可见文本 + 附加图片（多模态）。
///
/// 与 `echo_defs::tool::ToolResult` 同构（echo-loop 不依赖 echo-agent，
/// 故在此独立定义；harness 侧负责转换）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolOutcome {
    /// 模型可见文本。
    pub text: String,
    /// 附加图片（URL 或 data URI；透传到工具消息的 image 块）。
    pub images: Vec<String>,
}

impl ToolOutcome {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    pub fn with_images(text: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            text: text.into(),
            images,
        }
    }
}

/// How the runner executes one tool call. The harness owns the concrete
/// executor (registry lookup + orchestration tools); the runner only drives
/// the lifecycle around it.
///
/// **异步签名**（2026-10 巡检）：此前为同步闭包（内部 block_in_place +
/// block_on 桥接）——无法在途取消、current_thread runtime 下 panic。现与
/// chat 出口一样返回 boxed future，harness 可在内部与 turn 取消竞速。
/// 引用式（非 Arc/`'static`）：executor 可与调用方的会话状态绑定。
pub type ToolExecutor<'a> = &'a (dyn Fn(
    &str,
    &str,
    &ToolCall,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutcome> + Send + 'a>>
         + Send
         + Sync);

/// 异步版工具执行器：异步编排工具（如 `spawn_subagent`）经此通道进入管线。
///
/// 签名是「同步返回 future 句柄」而非 async 闭包：harness 侧的编排通常需要
/// tokio::spawn 一个后台任务，其结果经 oneshot 通道传回——这避免了
/// `async Fn` 闭包捕获引用时未来生命周期无法表达为 Send 的经典困境。
pub type AsyncToolExecutor<'a> = &'a (dyn Fn(
    &str,
    &str,
    &ToolCall,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutcome> + Send>>
         + Send
         + Sync);

/// 每 step 的模型出口（2026-10 巡检新增）：harness 在此解析**当前**
/// provider 并执行请求——运行期 provider/预算切换（persona API 切换、
/// 配置更新）对 echo-loop 即时生效，不再固化于 runner 构造期。
///
/// 返回 boxed future（`'a` 绑定 harness 借用）；runner 以 `tokio::select!`
/// 与 turn 取消竞速。
pub type ChatExecutor<'a> = &'a (dyn Fn(
    ChatRequest,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<ChatResponse, echo_defs::LlmError>> + Send + 'a>,
> + Send
         + Sync);

/// 异步编排工具的内联处理钩子（subagent 等）。
///
/// loop 拥有"先管线拦截、后回退注册表"的分派点——不在 agent 层的
/// `run_tool` 里硬编码特判；其他异步编排工具将来复用同一组钩子。
/// 默认实现全部 no-op（无异步编排工具时零成本）。
#[derive(Default)]
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

/// run 的扩展参数：工具 schema（发给模型）、推理回调（转发至 UI）、
/// 动态模型名与 chat 出口。
#[derive(Default)]
pub struct RunExtras<'a> {
    /// 模型可见的工具定义（None = 无工具请求，纯对话）。
    pub tools: Option<Vec<echo_defs::tool::ToolDefinition>>,
    /// 推理回调：（session_id, reasoning_text）。None = 忽略。
    /// 引用式（与 executor 相同哲学）：回调可与调用方的会话状态绑定。
    pub on_reasoning: Option<&'a (dyn Fn(String, String) + Send + Sync)>,
    /// 异步编排工具钩子（subagent 等；默认无）。
    pub subagent_hooks: SubagentToolHooks<'a>,
    /// 本 turn 的模型名（harness 每 turn 解析一次；None = 构造期
    /// `llm.default_model()`）。
    ///
    /// 粒度说明（2026-10 巡检）：内置循环每 step 读取 active_model；
    /// echo-loop 按 **turn** 解析——运行期切换在下一 turn 生效（同一
    /// turn 内切换模型属极端边缘场景）。
    pub model: Option<String>,
    /// 每 step 的 chat 出口（None = 构造期 `llm`）。harness 在此解析
    /// 当前 provider 与预算（见 [`ChatExecutor`]）。
    pub chat: Option<ChatExecutor<'a>>,
    /// 本 turn 的调用方标识（如分支 id）：随 `AgentRequest`/`ModelResponse`
    /// 事件回传，供多分支并发场景区分事件归属（2026-10 巡检）。
    pub turn_id: Option<String>,
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

    /// 生命周期事件总线（2026-10 巡检新增）：消费方（agent 侧的事件翻译、
    /// UI 转发、hook）订阅 turn/step 事件；订阅按会话 id 过滤（runner 可
    /// 被多会话并发共享）。
    pub fn bus(&self) -> &Arc<EventBus> {
        &self.bus
    }

    /// Drive one turn: admitted input → final reply (or error).
    ///
    /// `execute_tool` is the harness's tool executor (called through the
    /// pipeline). `history` is the point-in-time model context; the runner
    /// appends assistant/tool messages to it as steps progress.
    // 8 个参数：turn 的入参就是这么多；合并成新结构体属于公共 API 重构，
    // 不在本次 clippy 清理范围内（行为不变）。
    #[allow(clippy::too_many_arguments)]
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

        // 模型名：harness 每 turn 解析（None = 构造期 llm 缺省）。
        let model = extras
            .model
            .clone()
            .unwrap_or_else(|| self.llm.default_model().to_string());
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
                turn_id: extras.turn_id.clone(),
                request: ChatRequest {
                    model: model.clone(),
                    messages: messages.clone(),
                    tools,
                    temperature: None,
                    max_tokens: self.options.max_tokens,
                },
            };
            let request = self.bus.emit_sync(request, DispatchMode::Waterfall);

            // chat 出口（2026-10 巡检）：harness 的 ChatExecutor 每步解析
            // **当前** provider 与预算；缺省回退构造期 llm。外层 select 与
            // turn 取消竞速——在途模型请求可被立即中止。
            let response = {
                let fut: std::pin::Pin<
                    Box<
                        dyn std::future::Future<Output = Result<ChatResponse, echo_defs::LlmError>>
                            + Send
                            + '_,
                    >,
                > = match extras.chat {
                    Some(chat) => chat(request.request.clone()),
                    None => Box::pin(self.llm.chat(&request.request)),
                };
                tokio::select! {
                    response = fut => response.map_err(|e| LoopError::Model(e.to_string()))?,
                    _ = cancel.cancelled() => return Err(LoopError::Cancelled),
                }
            };
            // 用量回传（step/model）：消费方据此转发 UI（LlmResponse）与记账。
            self.bus.emit_sync(
                ModelResponse {
                    session_id: session_id.into(),
                    turn_id: extras.turn_id.clone(),
                    model: request.request.model.clone(),
                    prompt_tokens: response.usage.prompt_tokens,
                    completion_tokens: response.usage.completion_tokens,
                },
                DispatchMode::Observe,
            );
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
            messages.push(ChatMessage::assistant_with_tool_calls(
                response.content.clone().unwrap_or_default(),
                response.reasoning_content.clone(),
                response.tool_calls.clone(),
            ));
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
                let is_async_tool = extras.subagent_hooks.is_async_tool;
                let execute_async = extras.subagent_hooks.execute_async;
                let result = self
                    .pipeline
                    .run(call, move || {
                        let session_id = session_id_owned.clone();
                        let call = call_for_executor.clone();
                        Box::pin(async move {
                            if is_async_tool.is_some_and(|f| f(&call.name)) {
                                if let Some(exec) = execute_async {
                                    return exec(&session_id, "", &call).await;
                                }
                            }
                            execute_tool(&session_id, "", &call).await
                        })
                    })
                    .await;
                let outcome = match result {
                    crate::pipeline::ToolPipelineResult::ShortCircuit(outcome) => outcome,
                    crate::pipeline::ToolPipelineResult::Continue => ToolOutcome::text(""),
                };
                self.bus.emit_sync(
                    ToolResult {
                        session_id: session_id.into(),
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        result: outcome.text.clone(),
                    },
                    DispatchMode::Observe,
                );
                // 工具消息携带图片（与内置循环同口径，2026-10 巡检：
                // 此前只取 text——工具产出的图片在多模态链路上丢失）。
                messages.push(ChatMessage::tool_with_images(
                    echo_defs::media::compact_embedded_media(&outcome.text, &outcome.images),
                    &call.id,
                    outcome.images.clone(),
                ));
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
