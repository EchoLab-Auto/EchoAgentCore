//! 后台 bash 会话工具：`bash-start` / `bash-exec` / `bash-stop`。
//!
//! 与 Panel 的 Shell 面板共用同一个进程级 [`ShellHandle`]（`agent-module-shell` 的
//! 会话管理器）：LLM 可借此维护持久 shell 上下文（cd、变量、长任务），面板
//! 做终端可视化。
//!
//! ## 命名（2026-11）
//!
//! 这一族工具的名字统一到 `bash-` 前缀，与同步的一次性工具 [`bash`] 区分：
//!
//! | 工具 | 语义 |
//! |---|---|
//! | `bash` | 一次性执行，**不等**状态、命令结束即进程消失 |
//! | `bash-start` | 起一条**常驻**会话（返回 `session_id`） |
//! | `bash-exec` | 在常驻会话里执行一条命令（`cd` / `export` 跨调用保持） |
//! | `bash-stop` | 销毁会话（终止 bash 及其全部子孙进程组） |
//!
//! 此前叫 `shell_exec`——但它与 `bash` 是同一件事的两种模式，名字不一致
//! 会让模型在两套命名之间犹豫（实测有模型因此在长任务里选错工具）。
//!
//! ## 为什么不再叫 `shell_*`
//!
//! 其中 `bash-exec` 是用户直接点名的重命名；`bash-start` / `bash-stop` 一并
//! 对齐，否则同一族工具横跨两个前缀，模型的工具选择会退化成猜。

use async_trait::async_trait;
use serde_json::json;

use crate::session::{CommandGuard, ShellHandle};

/// 这次调用的归属人格。
///
/// **优先取调用上下文**（`Tool::execute_in` 递进来的 `cx.team_id`）；直接经
/// 注册表执行时没有上下文，才回退历史上的 `__team_id` 参数（模型看不到它）。
/// 归属校验本身不变：`ensure_owned_by` 决定放行与否。
fn requester<'a>(
    cx: Option<&'a echo_defs::tool::ToolContext>,
    arguments: &'a serde_json::Value,
) -> Option<&'a str> {
    cx.and_then(|cx| cx.team_id.as_deref())
        .filter(|id| !id.is_empty())
        .or_else(|| arguments.get("__team_id").and_then(|v| v.as_str()))
}

/// 注册 shell 工具（`bash-start` / `bash-exec` / `bash-stop`）。
///
/// `guard`：执行前的命令护栏（见 [`agent_module_shell::self_stop_guard`]）。
pub fn register_shell_tools(
    registry: &mut agent_module_tools::ToolRegistry,
    guard: Option<CommandGuard>,
    shell: ShellHandle,
) {
    registry.register(std::sync::Arc::new(BashStartTool {
        shell: shell.clone(),
    }));
    registry.register(std::sync::Arc::new(BashExecTool {
        guard,
        shell: shell.clone(),
    }));
    registry.register(std::sync::Arc::new(BashStopTool { shell }));
}

/// `bash-start`：新建一条 shell 会话（不执行命令，故无命令护栏）。
pub struct BashStartTool {
    shell: ShellHandle,
}

#[async_trait]
impl agent_module_tools::Tool for BashStartTool {
    fn name(&self) -> &str {
        "bash-start"
    }
    fn description(&self) -> &str {
        "Start a persistent background bash session. Commands run in the same shell keep state (cwd, exports, background jobs). Returns session_id used by bash-exec / bash-stop. Optional workdir. Also available on remote federated nodes as `<peer>__bash-start` — start the session there, run long jobs with `&`, and poll from later calls instead of blocking a synchronous `bash` window."
    }
    // ── 远程面声明（2026-11）──
    /// 可被对端节点直接调用：会话住在**执行端**的进程里，返回的
    /// `session_id` 只是一个不透明句柄——跨机带回大脑侧后原样塞进
    /// `bash-exec` / `bash-stop` 即可。
    ///
    /// ## 为什么这三个工具必须能远程
    ///
    /// 同步 `bash` 是远程唯一可用的执行原语：没有句柄、没有后台作业、
    /// 不会在完成时唤醒。于是"等训练出结果"只能写成 `sleep N; grep log`。
    /// 实测某个算法会话里，**64% 的工具时间**（4.83h / 7.52h）耗在含
    /// `sleep` 的调用上，其中 65 次是 `sleep 60~290` 的定长等待，且
    /// `sleep` 的取值一路从 60 试到 290（说明是在猜日志何时出现）。
    /// 有了远程会话，起任务立刻返回、期间干别的、回头 poll 才是可能的。
    ///
    /// ## 归属语义（与本地调用的唯一差别）
    ///
    /// 远程 invoke 走 `registry.execute`，不带 `ToolContext`，因此
    /// `requester` 为 `None`。`ShellManager::ensure_owned_by` 的校验需要
    /// **owner 与 requester 同时存在**才比较，所以远程调用一律放行。
    /// 这不构成新增风险面：peer 策略为 `allow_tools` 白名单，能调本工具的
    /// peer 本来就能调 `bash`（任意命令）；而 `session_id` 由执行端生成、
    /// 不外泄，跨 peer 猜不到。真正需要隔离时应收敛 `allow_tools`。
    fn remote_capable(&self) -> bool {
        true
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "workdir": {"type": "string", "description": "Working directory for the shell (default: framework workspace)"}
            }
        })
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> Result<String, agent_module_tools::ToolError> {
        self.apply(None, arguments).await
    }

    /// 归属人格来自**上下文**（不再是 `__team_id` 魔术参数）。
    async fn execute_in(
        &self,
        cx: &echo_defs::tool::ToolContext,
        arguments: serde_json::Value,
    ) -> Result<echo_defs::tool::ToolResult, agent_module_tools::ToolError> {
        self.apply(Some(cx), arguments)
            .await
            .map(echo_defs::tool::ToolResult::text)
    }
}

impl BashStartTool {
    async fn apply(
        &self,
        cx: Option<&echo_defs::tool::ToolContext>,
        arguments: serde_json::Value,
    ) -> Result<String, agent_module_tools::ToolError> {
        let manager = self.shell.manager().ok_or_else(|| {
            agent_module_tools::ToolError::Execution("shell manager unavailable".into())
        })?;
        let workdir = arguments["workdir"].as_str().map(|s| s.to_string());
        let team_id = requester(cx, &arguments).map(str::to_string);
        let info = manager
            .start(workdir, team_id, &self.shell.emit())
            .await
            .map_err(agent_module_tools::ToolError::Execution)?;
        Ok(format!(
            "shell session started: {} (workdir: {})",
            info.session_id, info.workdir
        ))
    }
}

/// `bash-exec`：在指定会话里执行一条命令。
///
/// 与一次性工具 `bash` 的差别是**状态**：`cd` / `export` / 后台作业都留在
/// 会话里，下一次 `bash-exec` 接着用；超时也**不销毁**会话（可以回去看进度）。
pub struct BashExecTool {
    shell: ShellHandle,
    guard: Option<CommandGuard>,
}

#[async_trait]
impl agent_module_tools::Tool for BashExecTool {
    fn name(&self) -> &str {
        "bash-exec"
    }
    fn description(&self) -> &str {
        "Run one command inside a persistent bash session (see bash-start). Output is streamed to the Panel live **on the node that owns the session**. Timeout: 120s default, max 300s; on timeout the SESSION IS KEPT (you may poll again or continue). Background jobs survive: start them with `&` and poll in the next call. Also available on remote federated nodes as `<peer>__bash-exec` — the timeout semantics are identical there, and the session also survives it."
    }
    /// 可远程：理由见 [`BashStartTool::remote_capable`]。
    ///
    /// **已知限制**：远程调用只回终态（`FedFrame::InvokeResult`），不吃
    /// `InvokeOutput` 流式帧（`handle_invoke` 目前不发它）。所以执行端的
    /// 实时输出只出现在**执行端自己的面板**，大脑侧要等这一条命令返回
    /// 才看得到文本。对"起任务 + 稍后 poll"的用法没有影响。
    fn remote_capable(&self) -> bool {
        true
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {"type": "string", "description": "Session id from bash-start"},
                "command": {"type": "string", "description": "Shell command to run"},
                "timeout_secs": {"type": "integer", "description": "Timeout in seconds (default 120, max 300)"}
            },
            "required": ["session_id", "command"]
        })
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> Result<String, agent_module_tools::ToolError> {
        self.apply(None, arguments).await
    }

    async fn execute_in(
        &self,
        cx: &echo_defs::tool::ToolContext,
        arguments: serde_json::Value,
    ) -> Result<echo_defs::tool::ToolResult, agent_module_tools::ToolError> {
        self.apply(Some(cx), arguments)
            .await
            .map(echo_defs::tool::ToolResult::text)
    }
}

impl BashExecTool {
    async fn apply(
        &self,
        cx: Option<&echo_defs::tool::ToolContext>,
        arguments: serde_json::Value,
    ) -> Result<String, agent_module_tools::ToolError> {
        let session_id = arguments["session_id"].as_str().ok_or_else(|| {
            agent_module_tools::ToolError::InvalidArguments("session_id required".into())
        })?;
        let command = arguments["command"].as_str().ok_or_else(|| {
            agent_module_tools::ToolError::InvalidArguments("command required".into())
        })?;
        if let Some(guard) = &self.guard {
            guard(command)?;
        }
        let timeout = arguments["timeout_secs"].as_u64().map(|t| t.min(300));
        let manager = self.shell.manager().ok_or_else(|| {
            agent_module_tools::ToolError::Execution("shell manager unavailable".into())
        })?;
        // 归属校验（2026-10）：不得操作其他智能体的 shell 会话。
        let requester = requester(cx, &arguments);
        manager
            .ensure_owned_by(session_id, requester)
            .map_err(agent_module_tools::ToolError::Execution)?;
        let (output, success, timed_out) = manager
            .exec(session_id, command, timeout, &self.shell.emit())
            .await
            .map_err(agent_module_tools::ToolError::Execution)?;
        let mut text = output;
        if timed_out {
            text.push_str("\n[notice: command timed out; session kept, you may retry or continue]");
        }
        if success {
            Ok(text)
        } else {
            // 超时/命令失败不按致命 error 处理（与工具超时策略一致）。
            Ok(text)
        }
    }
}

/// `bash-stop`：销毁会话（终止 bash 及全部子孙进程组）。
pub struct BashStopTool {
    shell: ShellHandle,
}

#[async_trait]
impl agent_module_tools::Tool for BashStopTool {
    fn name(&self) -> &str {
        "bash-stop"
    }
    fn description(&self) -> &str {
        "Stop and destroy a persistent bash session (kills the whole process group, including background jobs). Also available on remote federated nodes as `<peer>__bash-stop`."
    }
    /// 可远程：理由见 [`BashStartTool::remote_capable`]。
    ///
    /// 与 `bash-start` **成对**声明：只给创建不给销毁，远程起的会话就
    /// 只能等执行端 Core 重启才回收（会话是运行期资源，`MAX_SHELL_SESSIONS`
    /// 上限 8 个，泄漏几次就再也起不来）。
    fn remote_capable(&self) -> bool {
        true
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {"type": "string", "description": "Session id from bash-start"}
            },
            "required": ["session_id"]
        })
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> Result<String, agent_module_tools::ToolError> {
        self.apply(None, arguments).await
    }

    async fn execute_in(
        &self,
        cx: &echo_defs::tool::ToolContext,
        arguments: serde_json::Value,
    ) -> Result<echo_defs::tool::ToolResult, agent_module_tools::ToolError> {
        self.apply(Some(cx), arguments)
            .await
            .map(echo_defs::tool::ToolResult::text)
    }
}

impl BashStopTool {
    async fn apply(
        &self,
        cx: Option<&echo_defs::tool::ToolContext>,
        arguments: serde_json::Value,
    ) -> Result<String, agent_module_tools::ToolError> {
        let session_id = arguments["session_id"].as_str().ok_or_else(|| {
            agent_module_tools::ToolError::InvalidArguments("session_id required".into())
        })?;
        let manager = self.shell.manager().ok_or_else(|| {
            agent_module_tools::ToolError::Execution("shell manager unavailable".into())
        })?;
        // 归属校验（2026-10）：不得停止其他智能体的 shell 会话。
        let requester = requester(cx, &arguments);
        manager
            .ensure_owned_by(session_id, requester)
            .map_err(agent_module_tools::ToolError::Execution)?;
        manager
            .stop(session_id, &self.shell.emit())
            .await
            .map_err(agent_module_tools::ToolError::Execution)?;
        Ok(format!("shell session {session_id} stopped"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_module_tools::Tool;

    /// 远程面声明（2026-11）：三个会话工具必须都能被对端节点直接调用。
    ///
    /// 这条锁的是一个真实的能力缺口：声明缺失时它们不进握手 `NodeCaps.tools`，
    /// 大脑侧就不会注册 `<peer>__bash-start` 等代理工具——于是跨机只剩同步
    /// `bash`，长任务只能靠定长 `sleep` 等（实测某会话 64% 的工具时间耗在
    /// 含 `sleep` 的调用上）。删掉任何一条声明都会让这个问题静默回来。
    #[test]
    fn shell_tools_declare_their_remote_surface() {
        let shell = ShellHandle::new();
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(BashStartTool {
                shell: shell.clone(),
            }),
            Box::new(BashExecTool {
                shell: shell.clone(),
                guard: None,
            }),
            Box::new(BashStopTool { shell }),
        ];
        let mut names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        names.sort();
        assert_eq!(names, vec!["bash-exec", "bash-start", "bash-stop"]);
        for tool in &tools {
            assert!(
                tool.remote_capable(),
                "{}: 远程面声明缺失——对端将看不见它，跨机只能退回同步 bash",
                tool.name()
            );
        }
    }

    /// `bash-start` / `bash-exec` / `bash-stop` 都**不**声明路径参数。
    ///
    /// 理由与 `bash` 一致（见 `coding.rs` 的同名注释）：`workdir` /
    /// `command` 是工作目录与命令行，不是可裁决的路径参数。假装有沙箱比
    /// 没有沙箱更糟——它会让人以为远程会话被目录边界约束住了，实际边界是
    /// peer 策略（`allow_tools`）。
    #[test]
    fn shell_tools_do_not_fake_a_path_sandbox() {
        let shell = ShellHandle::new();
        let start = BashStartTool {
            shell: shell.clone(),
        };
        let exec = BashExecTool {
            shell: shell.clone(),
            guard: None,
        };
        let stop = BashStopTool { shell };
        for tool in [&start as &dyn Tool, &exec as &dyn Tool, &stop as &dyn Tool] {
            assert!(
                tool.path_arguments().is_empty(),
                "{}: 不应声明 path_arguments（边界是 peer 策略，不是目录）",
                tool.name()
            );
        }
    }
}
