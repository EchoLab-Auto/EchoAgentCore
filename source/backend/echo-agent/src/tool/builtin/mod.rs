//! Built-in tools (platform-independent).

pub mod adapter;
pub mod calculator;
pub mod checklist;
pub mod coding;
mod shell_tools;
pub mod websearch;

/// Register all built-in tools into a registry.
///
/// 每个工具打上 `echo-agent.tools.builtin` 包标签：插件
/// `echo-agent.tools.builtin` 的启停按包批量生效（见
/// [`ToolRegistry::set_package_enabled`]）。
pub fn register_all(
    registry: &mut crate::tool::ToolRegistry,
    adapters: std::sync::Arc<echo_adapter::AdapterRegistry>,
    workspace: std::path::PathBuf,
) {
    registry.register(std::sync::Arc::new(calculator::CalculatorTool));
    registry.register(std::sync::Arc::new(websearch::WebSearchTool));
    registry.register(std::sync::Arc::new(checklist::ChecklistTool::new()));
    adapter::register_adapter_tools(registry, adapters);
    coding::register_coding_tools(registry, workspace.clone());
    shell_tools::register_shell_tools(registry, workspace);
    for name in registry.names() {
        registry.set_package(&name, crate::plugins::TOOLS_BUILTIN_PLUGIN_ID);
    }
}
