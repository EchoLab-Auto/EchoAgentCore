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

/// 无条件长 `sleep` 的下限（秒）：低于它的 `sleep` 通常是在等文件落盘 /
/// 子进程起头，便宜且常见，放行。
const SLEEP_GUARD_THRESHOLD_SECS: u64 = 20;

/// 拒绝「无条件长 sleep」：等待应该等**条件**，不要等**时间**。
///
/// # 实测依据（2026-10-20，单次会话）
///
/// 工具总耗时 4939s 里有 **2989s（61%）** 花在 `sleep` 上。最典型的是：
///
/// ```text
/// (cargo test --workspace > /tmp/t.log 2>&1; echo DONE >> /tmp/t.log) & sleep 200; cat /tmp/t.log
/// ```
///
/// 测试 130s 就完成了，剩下 70s 纯等。同样的事改成轮询条件，条件一满足立刻
/// 返回——只多写一个 `until`，不是更复杂的代码。
///
/// # 为什么放在宿主（而不是 `echo-plugin-tools`）
///
/// 与 [`core_self_stop_guard`] 同理：这是**宿主的用法约定**，不是工具的机制。
/// 工具 crate 保持中立（它的超时/杀进程组测试本身就要用超长 `sleep` 当夹具，
/// 若把策略写进 `execute` 会误伤自己）。
///
/// # 三类豁免（都是正当用法）
///
/// 1. `sleep < 20s`：等文件落盘 / 进程起头；
/// 2. **轮询循环内**（命令里同时有 `do` 与 `done`）：条件满足即退出；
/// 3. **`systemd-run` 内**：sleep 跑在独立 unit 里，工具立刻返回。
///
/// # 残余风险（已知，可接受）
///
/// 判定用「命令位置 + 纯数字秒数」的保守启发式：`grep "sleep 200" file`、
/// 脚本里的字符串常量都不会误判。若确有命令真需要睡 30s 而无法改写成条件，
/// 用轮询循环（通常反而更快）或 `systemd-run` 脱离执行。
pub(crate) fn reject_unconditional_sleep(command: &str) -> Result<(), ToolError> {
    // 脱离执行：不阻塞工具，随便睡。
    if command.contains("systemd-run") {
        return Ok(());
    }
    // 轮询循环：有 do…done 就视为"等条件"。
    if command.contains(" do ") && command.contains("done") {
        return Ok(());
    }
    for secs in sleep_durations_secs(command) {
        if secs >= SLEEP_GUARD_THRESHOLD_SECS {
            return Err(ToolError::Execution(format!(
                "blocked unconditional `sleep {secs}` (>= {SLEEP_GUARD_THRESHOLD_SECS}s)：\
                 等待要等条件，不要等时间。\n\
                 · 等后台任务：\n    \
                 (cmd > /tmp/t.log 2>&1; echo DONE >> /tmp/t.log) &\n    \
                 until grep -q DONE /tmp/t.log 2>/dev/null; do sleep 2; done; cat /tmp/t.log\n\
                 · 等进程/服务就绪：\n    \
                 until <条件>; do sleep 2; done\n    \
                 for i in $(seq 1 60); do <条件> && break; sleep 2; done\n\
                 · 确实要脱离执行（工具立刻返回）：\n    \
                 systemd-run --user --collect --unit=<name> bash -c '…'"
            )));
        }
    }
    Ok(())
}

/// 提取命令里**命令位置**上的 `sleep N`（N 为纯数字秒数）。
///
/// 只在行首 / `;` / `&` / `|` / `(` / 换行之后（允许夹空白）才算命令位置——
/// 这样 `grep "sleep 60" file`、脚本里的 `re.findall(r"sleep\s+(\d+)")`
/// 这类**数据**不会被误判成真的要睡 60 秒。
pub(crate) fn sleep_durations_secs(command: &str) -> Vec<u64> {
    let bytes = command.as_bytes();
    let mut out = Vec::new();
    for (i, _) in command.match_indices("sleep") {
        // 向前跳过空白：`& sleep 200` / `;\n  sleep 60` 都是命令位置。
        let mut k = i;
        while k > 0 && (bytes[k - 1] == b' ' || bytes[k - 1] == b'\t') {
            k -= 1;
        }
        let at_command_position =
            k == 0 || matches!(bytes[k - 1], b';' | b'&' | b'|' | b'(' | b'\n');
        if !at_command_position {
            continue;
        }
        // `sleepy` / `sleep_ms` 这类标识符不算。
        if bytes
            .get(i + 5)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        {
            continue;
        }
        let mut j = i + 5;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        let start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > start {
            if let Ok(secs) = command[start..j].parse::<u64>() {
                out.push(secs);
            }
        }
    }
    out
}

/// 组合护栏：自停检查 + 无条件长 sleep 检查。
///
/// 两者都是**宿主的用法约定**，合成一个 `CommandGuard` 注入给 coding / shell
/// 工具（工具侧只认 `Arc<dyn Fn>`，不认识具体规则）。
pub(crate) fn host_command_guard() -> echo_plugin_tools::CommandGuard {
    let self_stop = core_self_stop_guard();
    std::sync::Arc::new(move |command: &str| {
        self_stop(command)?;
        reject_unconditional_sleep(command)
    })
}

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
    let guard = host_command_guard();
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
    use super::{
        core_self_stop_guard, register_all, reject_unconditional_sleep, sleep_durations_secs,
    };

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

    /// 无条件长 sleep 必须被拦——这是 61% 工具耗时所在。
    #[test]
    fn blocks_unconditional_long_sleep() {
        for command in [
            "sleep 200",
            "sleep 60; echo -n state:; systemctl --user is-active echo-agent-core",
            "(cargo test --workspace > /tmp/t.log 2>&1; echo DONE >> /tmp/t.log) & sleep 200; cat /tmp/t.log",
            "true && sleep 110",
            "cd /tmp && sleep 90 && cat x",
            "echo a | sleep 30",
        ] {
            assert!(
                reject_unconditional_sleep(command).is_err(),
                "应当拦下无条件长 sleep：{command}"
            );
        }
    }

    /// 三类正当用法不能误伤。
    #[test]
    fn allows_legitimate_sleep_forms() {
        for command in [
            // 短 sleep：等文件落盘 / 进程起头
            "sleep 2",
            "sleep 5 && echo hi",
            "echo a; sleep 19",
            // 轮询循环：条件满足即退出
            "until grep -q DONE /tmp/t.log 2>/dev/null; do sleep 2; done; cat /tmp/t.log",
            "for i in $(seq 1 90); do sleep 2; systemctl --user is-active x; done",
            "while ! test -f /tmp/ready; do sleep 1; done",
            // 脱离执行：工具立刻返回
            "systemd-run --user --collect --unit=foo bash -c 'sleep 45; systemctl --user restart x'",
            // 数据，不是命令：grep 的模式 / 脚本里的正则
            "grep 'sleep 200' log.txt",
            "python3 -c \"import re; print(re.findall(r'sleep 300', s))\"",
        ] {
            assert!(
                reject_unconditional_sleep(command).is_ok(),
                "不该拦：{command}"
            );
        }
    }

    /// 解析器只认「命令位置 + 纯数字」，不认标识符或字符串里的数字。
    #[test]
    fn sleep_parser_is_conservative() {
        assert_eq!(sleep_durations_secs("sleep 300"), vec![300]);
        assert_eq!(sleep_durations_secs("a; sleep 7"), vec![7]);
        assert_eq!(sleep_durations_secs("a && sleep 7"), vec![7]);
        assert_eq!(sleep_durations_secs("\nsleep 7"), vec![7]);
        // 非命令位置 / 非纯数字 → 不算
        assert!(sleep_durations_secs("grep 'sleep 300' f").is_empty());
        assert!(sleep_durations_secs("echo sleep 300").is_empty());
        assert!(sleep_durations_secs("sleepy 300").is_empty());
        assert!(sleep_durations_secs("sleep_ms=300").is_empty());
        // 小数秒解析出 0（远低于阈值 → 放行），不是"没解析到"。
        assert_eq!(sleep_durations_secs("sleep 0.5"), vec![0]);
        assert!(reject_unconditional_sleep("sleep 0.5").is_ok());
        // 多个：取全部
        assert_eq!(sleep_durations_secs("sleep 30; sleep 40"), vec![30, 40]);
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
