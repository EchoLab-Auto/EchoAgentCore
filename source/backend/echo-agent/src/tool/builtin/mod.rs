//! Built-in tools (platform-independent).

pub mod adapter;
pub mod calculator;
pub mod checklist;
pub mod coding;
pub mod websearch;

/// Register all built-in tools into a registry.
pub fn register_all(
    registry: &mut crate::tool::ToolRegistry,
    adapters: std::sync::Arc<echo_adapter::AdapterRegistry>,
    workspace: std::path::PathBuf,
) {
    registry.register(std::sync::Arc::new(calculator::CalculatorTool));
    registry.register(std::sync::Arc::new(websearch::WebSearchTool));
    registry.register(std::sync::Arc::new(checklist::ChecklistTool::new()));
    adapter::register_adapter_tools(registry, adapters);
    coding::register_coding_tools(registry, workspace);
}
