//! The `Tool` capability seam — Service Definition role.
//!
//! A tool is a named, described, executable capability the LLM can call
//! during the agent loop. The definition is model-facing: schema, results,
//! and errors are plain text/JSON with no UI or transport vocabulary.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

/// Tool execution errors, all model-facing text.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("工具参数无效: {0}")]
    InvalidArguments(String),
    #[error("工具执行失败: {0}")]
    Execution(String),
    #[error("找不到工具: {0}")]
    NotFound(String),
}

/// Tool schema sent to the LLM (OpenAI function-calling format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

/// A capability the LLM can invoke during the agent loop.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the arguments object.
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, arguments: Value) -> Result<String, ToolError>;
    /// Optional structured state snapshot for UI visualization. Tools that
    /// expose a state panel (e.g. checklist) override this; the agent emits
    /// the returned value in a `ChecklistUpdated` event after each execution.
    /// This is a UI projection that will move out of the definition layer
    /// (Phase 4: state panels become event subscriptions).
    fn snapshot(&self) -> Option<Value> {
        None
    }
}
