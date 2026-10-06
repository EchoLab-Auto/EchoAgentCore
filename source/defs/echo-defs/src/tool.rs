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

/// The result of a tool execution: model-visible text plus optional
/// multimodal media (image URLs or `data:` URIs).
///
/// `text` stays the primary channel; `images` lets a tool hand pictures to a
/// vision-capable model through the same caller (e.g. screenshots, generated
/// charts, search result thumbnails).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub text: String,
    pub images: Vec<String>,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: vec![],
        }
    }

    pub fn with_images(text: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            text: text.into(),
            images,
        }
    }
}

impl From<String> for ToolResult {
    fn from(value: String) -> Self {
        Self::text(value)
    }
}

impl From<&str> for ToolResult {
    fn from(value: &str) -> Self {
        Self::text(value)
    }
}

/// A capability the LLM can invoke during the agent loop.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// UI grouping category (empty = "内置" on the frontend).
    fn category(&self) -> &'static str {
        "builtin"
    }
    /// JSON Schema for the arguments object.
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    /// Execute the tool, returning model-visible text.
    async fn execute(&self, arguments: Value) -> Result<String, ToolError>;

    /// Self-declared execution timeout parsed from the tool's own arguments.
    /// The agent loop's outer guard uses `max(configured tool_timeout_secs,
    /// hint + grace)` so a tool that explicitly allows a long run (e.g.
    /// `bash` with `timeout_secs: 300`) is not cut short by the
    /// generic guard. `None` = no hint; the configured default applies.
    fn timeout_hint(&self, _arguments: &Value) -> Option<std::time::Duration> {
        None
    }

    /// Execute the tool with multimodal output. Defaults to [`execute`] with
    /// no images; tools that produce media override this to attach them.
    async fn execute_rich(&self, arguments: Value) -> Result<ToolResult, ToolError> {
        let text = self.execute(arguments).await?;
        Ok(ToolResult::text(text))
    }

    /// Optional structured state snapshot for UI visualization. Tools that
    /// expose a state panel (e.g. checklist) override this; the agent emits
    /// the returned value in a `ChecklistUpdated` event after each execution.
    /// This is a UI projection that will move out of the definition layer
    /// (Phase 4: state panels become event subscriptions).
    fn snapshot(&self) -> Option<Value> {
        None
    }

    /// 按**会话**过滤的 UI 投影快照（2026-10）。工具状态带会话维度时
    /// 覆盖此方法（checklist）；默认退化为 [`Self::snapshot`]——无会话
    /// 维度的工具不受影响。事件侧（`ChecklistUpdated`）用本方法，避免把
    /// 其他会话的状态挂到当前会话上广播。
    fn snapshot_for_session(&self, _session_id: &str) -> Option<Value> {
        self.snapshot()
    }
}
