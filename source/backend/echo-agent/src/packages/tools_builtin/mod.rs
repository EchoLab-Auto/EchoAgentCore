//! Built-in tools (platform-independent).

use regex::Regex;

use crate::tool::ToolError;

pub mod api_profiles;

// 工具实现已外迁到 `echo-plugin-tools`；这里保留原模块路径作为宿主兼容面。
pub use echo_plugin_tools::{calculator, checklist, coding, instance, shell_tools, websearch};

/// Reject commands that would stop or kill the Core service from inside
/// Core's own process tree.
///
/// `systemctl stop` is not followed by `start` when executed by an agent turn:
/// systemd terminates the whole service cgroup, including the shell running the
/// command, so the remainder of `stop && cp ... && start` is never executed.
/// `systemctl restart` is safe because systemd owns the restart job even if the
/// caller is terminated.
///
/// Broad `pkill -f` / `killall` patterns are also rejected when they mention
/// `core.toml` or `echo-agent-core`: even when intended for a smoke-test
/// process, `pkill -f core.toml` also matches the production Core command line
/// (`--config .../core.toml`).
const CORE_SELF_STOP_PATTERN: &str = r#"(?im)(?:^|[;&|]\s*|(?:ba)?sh\s+-[A-Za-z]*c\b\s*['\"]?\s*)\s*(?:sudo\s+)?(?:command\s+)?(?:(?:/usr/bin/)?systemctl\b[^\n;&|]*\b(?:stop|kill)\b[^\n;&|]*\becho-agent-core(?:\.service)?\b|(?:pkill|killall)\b[^\n;&|]*(?:(?:\[e\]|e)cho-agent-core|(?:\[c\]|c)ore\.toml))"#;

/// 构造自停护栏：[`CommandGuard`] 闭包，正则**编译一次**并捕获进去。
///
/// 2026-10 技术债清理：此前用 `echo_bootstrap::get_or_init` 存那份编译好的
/// `Regex`——但正则编译产物是**纯计算常量**，不是宿主状态，塞进进程级表纯属
/// 借用现成设施。改为「构造护栏时编译一次、move 进闭包」：零全局态，且护栏
/// 的唯一实例由调用方持有。
pub(crate) fn core_self_stop_guard() -> echo_plugin_tools::CommandGuard {
    let regex = Regex::new(CORE_SELF_STOP_PATTERN).expect("self-stop regex is valid");
    std::sync::Arc::new(move |command: &str| {
        if regex.is_match(command) {
            return Err(ToolError::Execution(
                "blocked command: stopping or killing echo-agent-core from inside its own process would terminate this tool before later cleanup/start steps run. Use `systemctl --user restart echo-agent-core.service` or the managed updater `scripts/update.sh` instead.".into(),
            ));
        }
        Ok(())
    })
}

/// Register all built-in tools into a registry.
///
/// 每个工具打上 `echo-agent.tools.builtin` 包标签：插件
/// `echo-agent.tools.builtin` 的启停按包批量生效（见
/// [`crate::tool::ToolRegistry::set_package_enabled`]）。
pub fn register_all(
    registry: &mut crate::tool::ToolRegistry,
    instances: std::sync::Arc<echo_defs::plugin_instance::InstanceRegistry>,
    workspace: std::path::PathBuf,
    config_store: echo_config::ConfigStore,
    invoker: Option<echo_plugin_tools::RemoteInvoker>,
    shell: echo_shell::ShellHandle,
) {
    let guard = core_self_stop_guard();
    echo_plugin_tools::register_core_tools(registry);
    registry.register(std::sync::Arc::new(api_profiles::ListApiProfilesTool::new(
        config_store,
    )));
    echo_plugin_tools::register_instance_tools(registry, instances);
    echo_plugin_tools::register_coding_tools(registry, workspace, Some(guard.clone()), invoker);
    echo_plugin_tools::register_shell_tools(registry, Some(guard), shell);
    for name in registry.names() {
        registry.set_package(&name, crate::plugins::TOOLS_BUILTIN_PLUGIN_ID);
    }
}

#[cfg(test)]
mod tests {
    use super::{core_self_stop_guard, register_all};

    /// 内置工具集内部不能有重名：`ToolRegistry::register` 是覆盖语义，
    /// 撞名会让一个工具静默消失（工具定义还在，执行却落到另一个实现上）。
    /// 这条不变量此前靠人肉检查——踩过一次（`calculator` 与插件示例同名）。
    #[test]
    fn builtin_tools_have_unique_names() {
        let dir = std::env::temp_dir().join(format!("echo-builtin-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = echo_config::ConfigStore::new(dir.join("core.toml"));
        let mut registry = crate::tool::ToolRegistry::new();
        register_all(
            &mut registry,
            std::sync::Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
            dir.clone(),
            store,
            None,
            echo_shell::ShellHandle::new(),
        );
        let conflicts = registry.name_conflicts();
        assert!(
            conflicts.is_empty(),
            "内置工具集内部重名（后者覆盖前者）: {conflicts:?}"
        );
        let names = registry.names();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "工具名必须唯一: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blocks_common_core_self_stop_variants() {
        for command in [
            "systemctl --user stop echo-agent-core.service",
            "systemctl --user stop echo-agent-core",
            "true && systemctl --user kill -s TERM echo-agent-core.service",
            "/usr/bin/systemctl stop echo-agent-core.service",
            "bash -lc 'systemctl --user stop echo-agent-core.service'",
            "pkill -f core.toml",
            "pkill -f \"core.toml\"",
            "pkill -f '[e]cho-agent-core'",
            "true && killall echo-agent-core",
        ] {
            assert!(
                core_self_stop_guard()(command).is_err(),
                "not blocked: {command}"
            );
        }
    }

    #[test]
    fn allows_restart_status_and_text_search() {
        for command in [
            "systemctl --user restart echo-agent-core.service",
            "systemctl --user status echo-agent-core.service",
            "pgrep -af '[e]cho-agent-core'",
            "rg 'systemctl --user stop echo-agent-core' README.md",
            "rg 'pkill -f core.toml' README.md",
        ] {
            assert!(
                core_self_stop_guard()(command).is_ok(),
                "unexpectedly blocked: {command}"
            );
        }
    }
}
