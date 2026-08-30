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
    /// Names of tools disabled at runtime (excluded from the LLM definition
    /// list; calls fail with NotFound). Persisted via `[agent].disabled_tools`.
    disabled: tokio::sync::RwLock<std::collections::HashSet<String>>,
    /// Tool -> owning package id (e.g. "echo-agent.adapter.qq"). Set by the
    /// composition root when assembling; the frontend uses it to group
    /// checkboxes so a whole package (tools + skills) can be toggled at once.
    packages: tokio::sync::RwLock<std::collections::HashMap<String, String>>,
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
            disabled: tokio::sync::RwLock::new(std::collections::HashSet::new()),
            packages: tokio::sync::RwLock::new(std::collections::HashMap::new()),
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

    /// Toggle a tool's runtime enable/disable state. `false` when the tool
    /// does not exist. Invalidates the definition cache when changed.
    pub async fn set_enabled(&self, name: &str, enabled: bool) -> bool {
        if !self.tools.read().await.contains_key(name) {
            return false;
        }
        let mut disabled = self.disabled.write().await;
        let changed = if enabled {
            disabled.remove(name)
        } else {
            disabled.insert(name.to_string())
        };
        drop(disabled);
        if changed {
            self.invalidate_definitions();
        }
        true
    }

    /// Whether a registered tool is currently disabled.
    pub async fn is_disabled(&self, name: &str) -> bool {
        self.disabled.read().await.contains(name)
    }

    /// Assign the owning package id for a tool (metadata for the Panel).
    pub fn set_package(&self, name: &str, package: impl Into<String>) {
        self.packages
            .try_write()
            .map(|mut p| {
                p.insert(name.to_string(), package.into());
            })
            .ok();
    }

    /// Package id of a tool (None = not assigned).
    pub fn package_of(&self, name: &str) -> Option<String> {
        self.packages
            .try_read()
            .ok()
            .and_then(|p| p.get(name).cloned())
    }

    /// All disabled tool names.
    pub async fn disabled_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.disabled.read().await.iter().cloned().collect();
        names.sort();
        names
    }

    /// Build (or reuse) the tool definition list sent to the LLM.
    ///
    /// Disabled tools are excluded: the model never sees them, and calls
    /// against them fail with [`ToolError::NotFound`].
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
            let disabled = self.disabled.read().await;
            let mut list: Vec<ToolDefinition> = tools
                .values()
                .filter(|t| !disabled.contains(t.name()))
                .map(|t| ToolDefinition {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    parameters: Some(t.parameters()),
                })
                .collect();
            list.sort_by(|a, b| a.name.cmp(&b.name));
            list
        };
        let arc = Arc::new(defs);
        *cache = Some(Arc::clone(&arc));
        arc
    }

    /// Full tool definition list (including disabled tools) for the frontend
    /// browser. Not cached — requested on demand by the Panel.
    /// Returns `(definition, category, enabled, package)`.
    pub async fn full_definitions(&self) -> Vec<(ToolDefinition, String, bool, Option<String>)> {
        let tools = self.tools.read().await;
        let disabled = self.disabled.read().await;
        let packages = self.packages.try_read().ok();
        let mut list: Vec<(ToolDefinition, String, bool, Option<String>)> = tools
            .values()
            .map(|t| {
                let enabled = !disabled.contains(t.name());
                (
                    ToolDefinition {
                        name: t.name().to_string(),
                        description: t.description().to_string(),
                        parameters: Some(t.parameters()),
                    },
                    t.category().to_string(),
                    enabled,
                    packages.as_ref().and_then(|p| p.get(t.name()).cloned()),
                )
            })
            .collect();
        list.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        list
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

    /// Parameter JSON schema of a registered tool, if present.
    pub async fn parameters(&self, name: &str) -> Option<Value> {
        self.tools
            .read()
            .await
            .get(name)
            .map(|tool| tool.parameters())
    }

    /// Self-declared execution timeout of a registered tool for the given
    /// arguments, if the tool publishes one (see [`Tool::timeout_hint`]).
    pub async fn timeout_hint(
        &self,
        name: &str,
        arguments: &Value,
    ) -> Option<std::time::Duration> {
        self.tools
            .read()
            .await
            .get(name)
            .and_then(|tool| tool.timeout_hint(arguments))
    }

    pub async fn execute(&self, name: &str, arguments: Value) -> Result<String, ToolError> {
        let (tool, disabled) = {
            let tools = self.tools.read().await;
            let disabled = self.disabled.read().await;
            (tools.get(name).cloned(), disabled.contains(name))
        };
        if disabled {
            return Err(ToolError::NotFound(name.to_string()));
        }
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
        let (tool, disabled) = {
            let tools = self.tools.read().await;
            let disabled = self.disabled.read().await;
            (tools.get(name).cloned(), disabled.contains(name))
        };
        if disabled {
            return Err(ToolError::NotFound(name.to_string()));
        }
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
    async fn disable_hides_tool_from_definitions_and_blocks_calls() {
        let registry: Arc<ToolRegistry> = Arc::new(ToolRegistry::new());
        let _keep_stub = registry.register_reversible(Arc::new(StubTool { name: "stub" }));
        let _keep_keeper = registry.register_reversible(Arc::new(StubTool { name: "keeper" }));
        assert_eq!(registry.definitions().await.len(), 2);

        assert!(registry.set_enabled("stub", false).await);
        let names: Vec<String> = registry
            .definitions()
            .await
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert_eq!(names, vec!["keeper"], "disabled tool hidden from LLM");

        assert!(registry.is_disabled("stub").await);
        assert!(
            registry.execute("stub", Value::Null).await.is_err(),
            "disabled tool calls fail"
        );

        // Re-enable restores visibility and callability.
        assert!(registry.set_enabled("stub", true).await);
        assert_eq!(registry.definitions().await.len(), 2);
        assert_eq!(registry.execute("stub", Value::Null).await.unwrap(), "stub");
        // Unknown tool cannot be toggled.
        assert!(!registry.set_enabled("nope", false).await);
    }

    #[tokio::test]
    async fn full_definitions_reports_categories_and_enabled_state() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubTool { name: "stub" }));
        let registry = Arc::new(registry);
        let list = registry.full_definitions().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0.name, "stub");
        assert_eq!(list[0].1, "builtin");
        assert!(list[0].2);
        registry.set_enabled("stub", false).await;
        let list = registry.full_definitions().await;
        assert!(!list[0].2, "disabled state reported for frontend");
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
