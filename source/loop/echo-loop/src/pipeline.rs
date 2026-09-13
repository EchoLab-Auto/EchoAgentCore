//! The tool execution pipeline: `tools/pre-execute → tools/execute →
//! tools/post-execute` as waterfall middleware.
//!
//! Policy (timeouts, approval, audit, rate limits) is **middleware**, not loop
//! code: each listener receives the call plus a `next()` handle; delegating
//! runs the next stage, short-circuiting returns without calling `next()`.
//! This is dsh's around-middleware semantics for tool execution.

use std::sync::Arc;

use echo_defs::message::ToolCall;

/// Outcome of a pipeline stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPipelineResult {
    /// Continue to the next stage (or execute the tool).
    Continue,
    /// Stop the pipeline; the result as set by this listener is final.
    ShortCircuit(String),
}

/// One pipeline stage: sees the call, may rewrite/deny, delegates via
/// `next()`. `next` returns the outcome of the remainder of the pipeline
/// (including the tool execution itself for the last pre/post listener).
pub type PipelineStage =
    Arc<dyn Fn(&ToolCall, &dyn Fn() -> ToolPipelineResult) -> ToolPipelineResult + Send + Sync>;

/// The tool execution pipeline.
///
/// Stages run in registration order for `pre` (around the execution), then
/// the executor runs, then `post` stages run (around the result). A stage
/// that returns [`ToolPipelineResult::ShortCircuit`] without calling `next()`
/// stops the pipeline with its own result.
#[derive(Default)]
pub struct ToolPipeline {
    pre: Vec<PipelineStage>,
    post: Vec<PipelineStage>,
}

impl ToolPipeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pre-execution stage (runs before the tool, in order).
    pub fn push_pre(&mut self, stage: PipelineStage) {
        self.pre.push(stage);
    }

    /// Register a post-execution stage (runs after the tool, in order).
    pub fn push_post(&mut self, stage: PipelineStage) {
        self.post.push(stage);
    }

    /// Run the pipeline around a tool execution.
    ///
    /// `execute` is the actual tool implementation (the last "stage"),
    /// returning a future that `run` awaits; pre stages run first (each may
    /// short-circuit), then `execute` runs unless short-circuited, then post
    /// stages run on the result.
    pub async fn run<'a>(
        &self,
        call: &ToolCall,
        execute: impl FnOnce()
            -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>>,
    ) -> ToolPipelineResult {
        // Pre stages: each may short-circuit before execution.
        let mut index = 0usize;
        let pre_result = loop {
            if index >= self.pre.len() {
                break ToolPipelineResult::Continue;
            }
            let stage = &self.pre[index];
            index += 1;
            let result = stage(call, &|| ToolPipelineResult::Continue);
            if result != ToolPipelineResult::Continue {
                break result;
            }
        };
        if pre_result != ToolPipelineResult::Continue {
            return pre_result;
        }

        let result = execute().await;
        let mut outcome = ToolPipelineResult::Continue;
        for stage in &self.post {
            let current = result.clone();
            let stage_result = stage(call, &|| ToolPipelineResult::ShortCircuit(current.clone()));
            match stage_result {
                ToolPipelineResult::Continue => {}
                ToolPipelineResult::ShortCircuit(replaced) => {
                    outcome = ToolPipelineResult::ShortCircuit(replaced);
                }
            }
        }
        match outcome {
            ToolPipelineResult::ShortCircuit(replaced) => {
                ToolPipelineResult::ShortCircuit(replaced)
            }
            ToolPipelineResult::Continue => ToolPipelineResult::ShortCircuit(result),
        }
    }

    pub fn pre_len(&self) -> usize {
        self.pre.len()
    }

    pub fn post_len(&self) -> usize {
        self.post.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pipeline_executes_tool() {
        let pipeline = ToolPipeline::new();
        let call = ToolCall {
            id: "c1".into(),
            name: "t".into(),
            arguments: "{}".into(),
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(pipeline.run(&call, || Box::pin(async { "executed".to_string() })));
        assert_eq!(result, ToolPipelineResult::ShortCircuit("executed".into()));
    }

    #[tokio::test]
    async fn pre_stage_can_short_circuit_before_execution() {
        let mut pipeline = ToolPipeline::new();
        pipeline.push_pre(Arc::new(|_call, _next| {
            ToolPipelineResult::ShortCircuit("denied".into())
        }));
        let call = ToolCall {
            id: "c1".into(),
            name: "t".into(),
            arguments: "{}".into(),
        };
        let result = pipeline
            .run(&call, || Box::pin(async { "executed".to_string() }))
            .await;
        assert_eq!(
            result,
            ToolPipelineResult::ShortCircuit("denied".into()),
            "pre stage short-circuits; tool never runs"
        );
    }

    #[tokio::test]
    async fn pre_stage_delegating_runs_tool() {
        let mut pipeline = ToolPipeline::new();
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_ref = ran.clone();
        pipeline.push_pre(Arc::new(move |_call, next| {
            ran_ref.store(true, std::sync::atomic::Ordering::SeqCst);
            next()
        }));
        let call = ToolCall {
            id: "c1".into(),
            name: "t".into(),
            arguments: "{}".into(),
        };
        let result = pipeline
            .run(&call, || Box::pin(async { "executed".to_string() }))
            .await;
        assert_eq!(result, ToolPipelineResult::ShortCircuit("executed".into()));
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn post_stage_can_rewrite_the_result() {
        let mut pipeline = ToolPipeline::new();
        pipeline.push_post(Arc::new(|_call, _| {
            // Rewrite: replace whatever the tool produced.
            ToolPipelineResult::ShortCircuit("post-processed".into())
        }));
        let call = ToolCall {
            id: "c1".into(),
            name: "t".into(),
            arguments: "{}".into(),
        };
        let result = pipeline
            .run(&call, || Box::pin(async { "raw".to_string() }))
            .await;
        assert_eq!(
            result,
            ToolPipelineResult::ShortCircuit("post-processed".into())
        );
    }

    #[tokio::test]
    async fn pre_and_post_chain_in_order() {
        let mut pipeline = ToolPipeline::new();
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let order1 = order.clone();
        pipeline.push_pre(Arc::new(move |_call, next| {
            order1.lock().unwrap().push("pre");
            next()
        }));
        let order2 = order.clone();
        pipeline.push_post(Arc::new(move |_call, _| {
            order2.lock().unwrap().push("post");
            ToolPipelineResult::Continue
        }));
        let call = ToolCall {
            id: "c1".into(),
            name: "t".into(),
            arguments: "{}".into(),
        };
        let _ = pipeline
            .run(&call, || {
                order.lock().unwrap().push("execute");
                Box::pin(async { "result".to_string() })
            })
            .await;
        assert_eq!(*order.lock().unwrap(), vec!["pre", "execute", "post"]);
    }
}
