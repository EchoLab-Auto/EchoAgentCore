//! Built-in tools (platform-independent).

use regex::Regex;

use crate::tool::ToolError;

pub mod adapter;
pub mod api_profiles;
pub mod calculator;
pub mod checklist;
pub mod coding;
mod shell_tools;
pub mod websearch;

/// Reject commands that would stop or kill the Core service from inside
/// Core's own process tree.
///
/// `systemctl stop` is not followed by `start` when executed by an agent turn:
/// systemd terminates the whole service cgroup, including the shell running the
/// command, so the remainder of `stop && cp ... && start` is never executed.
/// `systemctl restart` is safe because systemd owns the restart job even if the
/// caller is terminated.
pub(crate) fn reject_core_self_stop(command: &str) -> Result<(), ToolError> {
    let regex = echo_context::kernel::get_or_init::<Regex>(|| {
        Regex::new(
            r#"(?im)(?:^|[;&|]\s*|(?:ba)?sh\s+-[A-Za-z]*c\b\s*['\"]?\s*)\s*(?:sudo\s+)?(?:command\s+)?(?:/usr/bin/)?systemctl\b[^\n;&|]*\b(?:stop|kill)\b[^\n;&|]*\becho-agent-core(?:\.service)?\b"#,
        )
        .expect("self-stop regex is valid")
    });
    if regex.is_match(command) {
        return Err(ToolError::Execution(
            "blocked command: stopping or killing echo-agent-core from inside its own process would terminate this tool before later cleanup/start steps run. Use `systemctl --user restart echo-agent-core.service` or the managed updater `scripts/update.sh` instead.".into(),
        ));
    }
    Ok(())
}

/// Register all built-in tools into a registry.
///
/// 每个工具打上 `echo-agent.tools.builtin` 包标签：插件
/// `echo-agent.tools.builtin` 的启停按包批量生效（见
/// [`crate::tool::ToolRegistry::set_package_enabled`]）。
pub fn register_all(
    registry: &mut crate::tool::ToolRegistry,
    adapters: std::sync::Arc<echo_adapter::AdapterRegistry>,
    workspace: std::path::PathBuf,
    config_store: echo_adapter::ConfigStore,
) {
    registry.register(std::sync::Arc::new(calculator::CalculatorTool));
    registry.register(std::sync::Arc::new(websearch::WebSearchTool));
    registry.register(std::sync::Arc::new(checklist::ChecklistTool::new()));
    registry.register(std::sync::Arc::new(api_profiles::ListApiProfilesTool::new(
        config_store,
    )));
    adapter::register_adapter_tools(registry, adapters);
    coding::register_coding_tools(registry, workspace.clone());
    shell_tools::register_shell_tools(registry);
    for name in registry.names() {
        registry.set_package(&name, crate::plugins::TOOLS_BUILTIN_PLUGIN_ID);
    }
}

#[cfg(test)]
mod tests {
    use super::reject_core_self_stop;

    #[test]
    fn blocks_common_core_self_stop_variants() {
        for command in [
            "systemctl --user stop echo-agent-core.service",
            "systemctl --user stop echo-agent-core",
            "true && systemctl --user kill -s TERM echo-agent-core.service",
            "/usr/bin/systemctl stop echo-agent-core.service",
            "bash -lc 'systemctl --user stop echo-agent-core.service'",
        ] {
            assert!(
                reject_core_self_stop(command).is_err(),
                "not blocked: {command}"
            );
        }
    }

    #[test]
    fn allows_restart_status_and_text_search() {
        for command in [
            "systemctl --user restart echo-agent-core.service",
            "systemctl --user status echo-agent-core.service",
            "rg 'systemctl --user stop echo-agent-core' README.md",
        ] {
            assert!(
                reject_core_self_stop(command).is_ok(),
                "unexpectedly blocked: {command}"
            );
        }
    }
}
