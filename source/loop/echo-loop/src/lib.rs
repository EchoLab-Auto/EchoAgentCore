//! EchoAgentCore agent loop — the default TurnRunner.
//!
//! dsh's turn/step model, ported to Rust:
//!
//! - a **turn** drains one admitted input and ends when nothing is owed;
//! - a **step** is one model request plus the tool executions it caused;
//! - the loop is the default driver, replaceable by mounting a different
//!   `TurnRunner` implementation.
//!
//! The loop drives the lifecycle as typed events on the harness
//! [`EventBus`](echo_context::EventBus) (dsh's extension points), and runs
//! tool executions through a waterfall pipeline
//! (`tools/pre-execute → tools/execute → tools/post-execute`) so policy
//! (timeouts, approval, audit, rate limits) is middleware, not loop code.
//!
//! # Crate layout
//!
//! | module | owns |
//! |---|---|
//! | [`event`] | turn/step lifecycle events (dispatchable on the bus) |
//! | [`pipeline`] | the tool execution pipeline (waterfall around middleware) |
//! | [`runner`] | the `TurnRunner` state machine |

pub mod event;
pub mod pipeline;
pub mod runner;

pub use event::{
    AgentPreStep, AgentRequest, StepEnd, StepStart, ToolCallRequested, ToolResult, TurnEnd,
    TurnStart, TurnStopping,
};
pub use pipeline::{ToolPipeline, ToolPipelineResult};
pub use runner::{LoopError, LoopOptions, TurnRunner};
