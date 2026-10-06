//! Turn/step lifecycle events — the loop's extension points.
//!
//! These events are dispatched on the harness `EventBus` (dsh's typed event
//! system). Listeners subscribe by event type; waterfall listeners can
//! rewrite the request or short-circuit (dsh `next()` semantics).

use echo_context::Event;

/// A turn is opening: `turn/start`. Carries the admitted input.
#[derive(Debug, Clone)]
pub struct TurnStart {
    pub session_id: String,
    pub input: String,
}

/// The model-facing input is being prepared: `agent/pre-step`.
///
/// Waterfall listeners may rewrite `messages` or reject the step
/// (`accept == false` short-circuits the turn with no model request).
#[derive(Debug, Clone)]
pub struct AgentPreStep {
    pub session_id: String,
    pub messages: Vec<echo_defs::message::ChatMessage>,
    /// When a listener sets this false, the turn closes with no step.
    pub accept: bool,
}

/// A step is opening: `step/start`.
#[derive(Debug, Clone)]
pub struct StepStart {
    pub session_id: String,
    pub step_index: usize,
}

/// The model request is being prepared: `agent/request`.
///
/// Waterfall listeners may rewrite the request before it reaches the
/// provider.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub session_id: String,
    /// 本 turn 的调用方标识（harness 提供，如分支 id）——多分支并发同一
    /// 会话时，消费方凭它区分事件归属（2026-10 巡检）。
    pub turn_id: Option<String>,
    pub request: echo_defs::message::ChatRequest,
}

/// A tool call was requested by the model: `tool/call`.
#[derive(Debug, Clone)]
pub struct ToolCallRequested {
    pub session_id: String,
    pub call: echo_defs::message::ToolCall,
}

/// A tool execution finished: `tool/result`.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub session_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub result: String,
}

/// A model response arrived for a step: `step/model`.
///
/// Carries the usage counters so consumers (UI/usage accounting) can render
/// token spend without reaching into the runner. Emitted after `agent/request`
/// and before any tool calls of the same step.
#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub session_id: String,
    /// 本 turn 的调用方标识（见 [`AgentRequest::turn_id`]）。
    pub turn_id: Option<String>,
    pub model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// A step ended: `step/end`.
#[derive(Debug, Clone)]
pub struct StepEnd {
    pub session_id: String,
    pub step_index: usize,
}

/// The turn is stopping: `agent/turn-stopping` (serial, no `next()`).
#[derive(Debug, Clone)]
pub struct TurnStopping {
    pub session_id: String,
}

/// The turn ended: `turn/end`.
#[derive(Debug, Clone)]
pub struct TurnEnd {
    pub session_id: String,
}

impl Event for TurnStart {}
impl Event for AgentPreStep {}
impl Event for StepStart {}
impl Event for AgentRequest {}
impl Event for ToolCallRequested {}
impl Event for ToolResult {}
impl Event for ModelResponse {}
impl Event for StepEnd {}
impl Event for TurnStopping {}
impl Event for TurnEnd {}
