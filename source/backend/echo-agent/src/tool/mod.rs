//! Tool system: named, described, executable capabilities the LLM can call.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use thiserror::Error;

use crate::llm::ToolDefinition;

pub mod builtin;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("工具参数无效: {0}")]
    InvalidArguments(String),
    #[error("工具执行失败: {0}")]
    Execution(String),
    #[error("找不到工具: {0}")]
    NotFound(String),
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
    fn snapshot(&self) -> Option<Value> {
        None
    }
}

/// Registry of tools, keyed by name.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    /// Tokio RwLock: `definitions()` runs inside async agent loops and must
    /// never block a worker thread on a contended std RwLock.
    cached_definitions: tokio::sync::RwLock<Option<Arc<Vec<ToolDefinition>>>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("names", &self.names())
            .finish()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self {
            tools: HashMap::new(),
            cached_definitions: tokio::sync::RwLock::new(None),
        }
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            cached_definitions: tokio::sync::RwLock::new(None),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
        // Best-effort cache invalidation; a concurrent definitions() may
        // still return the previous list for one call, which is harmless.
        if let Ok(mut cache) = self.cached_definitions.try_write() {
            *cache = None;
        }
    }

    /// Build (or reuse) the tool definition list sent to the LLM.
    pub async fn definitions(&self) -> Arc<Vec<ToolDefinition>> {
        // Fast path: read lock.
        if let Some(defs) = self.cached_definitions.read().await.as_ref() {
            return Arc::clone(defs);
        }
        // Slow path: write lock with a re-check, so a concurrent rebuild
        // between the read and write is not wasted.
        let mut cache = self.cached_definitions.write().await;
        if let Some(defs) = cache.as_ref() {
            return Arc::clone(defs);
        }
        let defs: Vec<ToolDefinition> = self
            .tools
            .values()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: Some(t.parameters()),
            })
            .collect();
        let arc = Arc::new(defs);
        *cache = Some(Arc::clone(&arc));
        arc
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Structured state snapshot of a registered tool, if it publishes one.
    pub fn snapshot(&self, name: &str) -> Option<Value> {
        self.tools.get(name).and_then(|tool| tool.snapshot())
    }

    pub async fn execute(&self, name: &str, arguments: Value) -> Result<String, ToolError> {
        match self.tools.get(name) {
            Some(tool) => tool.execute(arguments).await,
            None => Err(ToolError::NotFound(name.to_string())),
        }
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
