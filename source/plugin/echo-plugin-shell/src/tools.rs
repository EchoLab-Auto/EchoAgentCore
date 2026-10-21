//! 后台 bash 会话工具：`bash-start` / `bash-exec` / `bash-stop`。
//!
//! 与 Panel 的 Shell 面板共用同一个进程级 [`ShellHandle`]（`echo-shell` 的
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

use echo_shell::{CommandGuard, ShellHandle};

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
/// `guard`：执行前的命令护栏（见 [`echo_shell::self_stop_guard`]）。
pub fn register_shell_tools(
    registry: &mut echo_tool::ToolRegistry,
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
impl echo_tool::Tool for BashStartTool {
    fn name(&self) -> &str {
        "bash-start"
    }
    fn description(&self) -> &str {
        "Start a persistent background bash session. Commands run in the same shell keep state (cwd, exports, background jobs). Returns session_id used by bash-exec / bash-stop. Optional workdir."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "workdir": {"type": "string", "description": "Working directory for the shell (default: framework workspace)"}
            }
        })
    }
    async fn execute(&self, arguments: serde_json::Value) -> Result<String, echo_tool::ToolError> {
        self.apply(None, arguments).await
    }

    /// 归属人格来自**上下文**（不再是 `__team_id` 魔术参数）。
    async fn execute_in(
        &self,
        cx: &echo_defs::tool::ToolContext,
        arguments: serde_json::Value,
    ) -> Result<echo_defs::tool::ToolResult, echo_tool::ToolError> {
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
    ) -> Result<String, echo_tool::ToolError> {
        let manager = self
            .shell
            .manager()
            .ok_or_else(|| echo_tool::ToolError::Execution("shell manager unavailable".into()))?;
        let workdir = arguments["workdir"].as_str().map(|s| s.to_string());
        let team_id = requester(cx, &arguments).map(str::to_string);
        let info = manager
            .start(workdir, team_id, &self.shell.emit())
            .await
            .map_err(echo_tool::ToolError::Execution)?;
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
impl echo_tool::Tool for BashExecTool {
    fn name(&self) -> &str {
        "bash-exec"
    }
    fn description(&self) -> &str {
        "Run one command inside a persistent bash session (see bash-start). Output is streamed to the Panel live. Timeout: 120s default, max 300s; on timeout the SESSION IS KEPT (you may poll again or continue). Background jobs survive: start them with `&` and poll in the next call."
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
    async fn execute(&self, arguments: serde_json::Value) -> Result<String, echo_tool::ToolError> {
        self.apply(None, arguments).await
    }

    async fn execute_in(
        &self,
        cx: &echo_defs::tool::ToolContext,
        arguments: serde_json::Value,
    ) -> Result<echo_defs::tool::ToolResult, echo_tool::ToolError> {
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
    ) -> Result<String, echo_tool::ToolError> {
        let session_id = arguments["session_id"]
            .as_str()
            .ok_or_else(|| echo_tool::ToolError::InvalidArguments("session_id required".into()))?;
        let command = arguments["command"]
            .as_str()
            .ok_or_else(|| echo_tool::ToolError::InvalidArguments("command required".into()))?;
        if let Some(guard) = &self.guard {
            guard(command)?;
        }
        let timeout = arguments["timeout_secs"].as_u64().map(|t| t.min(300));
        let manager = self
            .shell
            .manager()
            .ok_or_else(|| echo_tool::ToolError::Execution("shell manager unavailable".into()))?;
        // 归属校验（2026-10）：不得操作其他智能体的 shell 会话。
        let requester = requester(cx, &arguments);
        manager
            .ensure_owned_by(session_id, requester)
            .map_err(echo_tool::ToolError::Execution)?;
        let (output, success, timed_out) = manager
            .exec(session_id, command, timeout, &self.shell.emit())
            .await
            .map_err(echo_tool::ToolError::Execution)?;
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
impl echo_tool::Tool for BashStopTool {
    fn name(&self) -> &str {
        "bash-stop"
    }
    fn description(&self) -> &str {
        "Stop and destroy a persistent bash session (kills the whole process group, including background jobs)."
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
    async fn execute(&self, arguments: serde_json::Value) -> Result<String, echo_tool::ToolError> {
        self.apply(None, arguments).await
    }

    async fn execute_in(
        &self,
        cx: &echo_defs::tool::ToolContext,
        arguments: serde_json::Value,
    ) -> Result<echo_defs::tool::ToolResult, echo_tool::ToolError> {
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
    ) -> Result<String, echo_tool::ToolError> {
        let session_id = arguments["session_id"]
            .as_str()
            .ok_or_else(|| echo_tool::ToolError::InvalidArguments("session_id required".into()))?;
        let manager = self
            .shell
            .manager()
            .ok_or_else(|| echo_tool::ToolError::Execution("shell manager unavailable".into()))?;
        // 归属校验（2026-10）：不得停止其他智能体的 shell 会话。
        let requester = requester(cx, &arguments);
        manager
            .ensure_owned_by(session_id, requester)
            .map_err(echo_tool::ToolError::Execution)?;
        manager
            .stop(session_id, &self.shell.emit())
            .await
            .map_err(echo_tool::ToolError::Execution)?;
        Ok(format!("shell session {session_id} stopped"))
    }
}
