//! Tool system: named, described, executable capabilities the LLM can call.
//!
//! The `Tool` trait and vocabulary live in [`echo_defs`](echo_defs); this
//! module re-exports them (keeping the `echo_agent::tool::…` paths) and owns
//! the concrete [`ToolRegistry`].
//!
//! Registration is reversible: [`ToolRegistry::register_reversible`] returns
//! a disposer that removes the tool and invalidates the definitions cache
//! (the dsh "registrations are effects" rule).

pub use echo_defs::tool::{Tool, ToolDefinition, ToolError, ToolResult};

pub mod builtin;

use std::collections::HashMap;
use std::sync::Arc;

use echo_context::Disposer;
use serde_json::Value;

/// Registry of tools, keyed by name.
///
/// The map is `tokio::sync::RwLock`-protected so reversible registrations can
/// mutate it from `&Arc<Self>` (shared ownership), while static assembly uses
/// the same lock via `try_write` (no contention at boot).
pub struct ToolRegistry {
    tools: tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>,
    /// Cache of the definition list sent to the LLM; invalidated on any
    /// registration change.
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
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: tokio::sync::RwLock::new(HashMap::new()),
            cached_definitions: tokio::sync::RwLock::new(None),
        }
    }

    /// Register a tool during static assembly.
    ///
    /// `&mut self` keeps the boot-time call sites simple; the map lock is
    /// uncontended at this point (`try_write` succeeds). For reversible
    /// registration use [`register_reversible`](Self::register_reversible).
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let mut tools = self
            .tools
            .try_write()
            .expect("tools map uncontended at boot");
        tools.insert(tool.name().to_string(), tool);
        drop(tools);
        self.invalidate_definitions();
    }

    /// Register a tool reversibly, returning a disposer that removes it and
    /// invalidates the definitions cache.
    ///
    /// Works from both sync (assembly) and async (plugin mount) contexts:
    /// inside a runtime the lock is taken without blocking the executor.
    /// Register a tool reversibly, returning a disposer that removes it and
    /// invalidates the definitions cache.
    ///
    /// Uncontended (assembly, test) paths take the lock directly; under
    /// contention inside a multi-thread runtime the lock is taken on the
    /// blocking pool. Disposal uses the same strategy.
    pub fn register_reversible(self: &Arc<Self>, tool: Arc<dyn Tool>) -> Disposer {
        let name = tool.name().to_string();
        if let Ok(mut tools) = self.tools.try_write() {
            tools.insert(name.clone(), tool);
        } else {
            let name_for_blocking = name.clone();
            tokio::task::block_in_place(move || {
                self.tools.blocking_write().insert(name_for_blocking, tool);
            });
        }
        self.invalidate_definitions();
        let registry = self.clone();
        Disposer::from_fn(move || {
            let mut tools = match registry.tools.try_write() {
                Ok(tools) => tools,
                Err(_) => {
                    let registry = registry.clone();
                    let name = name.clone();
                    return tokio::task::block_in_place(move || {
                        registry.tools.blocking_write().remove(&name);
                        registry.invalidate_definitions();
                    });
                }
            };
            tools.remove(&name);
            drop(tools);
            registry.invalidate_definitions();
        })
    }

    fn invalidate_definitions(&self) {
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
        let defs: Vec<ToolDefinition> = {
            let tools = self.tools.read().await;
            tools
                .values()
                .map(|t| ToolDefinition {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    parameters: Some(t.parameters()),
                })
                .collect()
        };
        let arc = Arc::new(defs);
        *cache = Some(Arc::clone(&arc));
        arc
    }

    pub fn names(&self) -> Vec<String> {
        match self.tools.try_read() {
            Ok(tools) => tools.keys().cloned().collect(),
            Err(_) => {
                tokio::task::block_in_place(|| self.tools.blocking_read().keys().cloned().collect())
            }
        }
    }

    /// Structured state snapshot of a registered tool, if it publishes one.
    pub fn snapshot(&self, name: &str) -> Option<Value> {
        match self.tools.try_read() {
            Ok(tools) => tools.get(name).and_then(|tool| tool.snapshot()),
            Err(_) => tokio::task::block_in_place(|| {
                self.tools
                    .blocking_read()
                    .get(name)
                    .and_then(|tool| tool.snapshot())
            }),
        }
    }

    pub async fn execute(&self, name: &str, arguments: Value) -> Result<String, ToolError> {
        let tool = self.tools.read().await.get(name).cloned();
        match tool {
            Some(tool) => tool.execute(arguments).await,
            None => Err(ToolError::NotFound(name.to_string())),
        }
    }

    /// Execute a tool with multimodal output support (text + images).
    pub async fn execute_rich(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<echo_defs::tool::ToolResult, ToolError> {
        let tool = self.tools.read().await.get(name).cloned();
        match tool {
            Some(tool) => tool.execute_rich(arguments).await,
            None => Err(ToolError::NotFound(name.to_string())),
        }
    }

    pub fn len(&self) -> usize {
        match self.tools.try_read() {
            Ok(tools) => tools.len(),
            Err(_) => tokio::task::block_in_place(|| self.tools.blocking_read().len()),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self.tools.try_read() {
            Ok(tools) => tools.is_empty(),
            Err(_) => tokio::task::block_in_place(|| self.tools.blocking_read().is_empty()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct StubTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "stub"
        }
        async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
            Ok("stub".into())
        }
    }

    #[tokio::test]
    async fn reversible_registration_unregisters_and_invalidates_cache() {
        let registry: Arc<ToolRegistry> = Arc::new(ToolRegistry::new());
        let _keep = registry.register_reversible(Arc::new(StubTool { name: "stub" }));
        assert_eq!(registry.names(), vec!["stub"]);
        let defs = registry.definitions().await;
        assert_eq!(defs.len(), 1);

        let disposer = registry.register_reversible(Arc::new(StubTool { name: "stub2" }));
        assert_eq!(registry.definitions().await.len(), 2, "cache rebuilt");

        disposer.dispose();
        assert_eq!(registry.names(), vec!["stub"]);
        assert_eq!(
            registry.definitions().await.len(),
            1,
            "cache invalidated after dispose"
        );
    }

    #[tokio::test]
    async fn static_register_and_reversible_share_the_map() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubTool { name: "static" }));
        let registry: Arc<ToolRegistry> = Arc::new(registry);
        let _keep = registry.register_reversible(Arc::new(StubTool { name: "dynamic" }));
        assert_eq!(registry.definitions().await.len(), 2);
        assert_eq!(
            registry.execute("static", Value::Null).await.unwrap(),
            "stub"
        );
        assert!(registry.execute("missing", Value::Null).await.is_err());
    }
}
