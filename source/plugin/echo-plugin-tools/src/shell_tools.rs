//! 后台 shell 工具：shell_start / shell_exec / shell_stop。
//!
//! 与 Panel 的 Shell 面板共用同一个进程级 [`ShellManager`]：
//! LLM 可借此维护持久 shell 上下文（cd、变量、长任务），面板做终端可视化。

use async_trait::async_trait;
use serde_json::json;

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


use echo_shell::ShellHandle;

/// 注册 Shell 工具（`shell_start` / `shell_exec` / `shell_stop`）。
///
/// `guard`：执行前的命令护栏（宿主注入；见 [`crate::CommandGuard`]）。
pub fn register_shell_tools(
    registry: &mut echo_tool::ToolRegistry,
    guard: Option<crate::CommandGuard>,
    shell: ShellHandle,
) {
    registry.register(std::sync::Arc::new(ShellStartTool {
        shell: shell.clone(),
    }));
    registry.register(std::sync::Arc::new(ShellExecTool {
        guard,
        shell: shell.clone(),
    }));
    registry.register(std::sync::Arc::new(ShellStopTool { shell }));
}

/// `shell_start`：新建一条 Shell 会话（不执行命令，故无命令护栏）。
pub struct ShellStartTool {
    shell: ShellHandle,
}

#[async_trait]
impl echo_tool::Tool for ShellStartTool {
    fn name(&self) -> &str {
        "shell_start"
    }
    fn description(&self) -> &str {
        "Start a persistent background bash session. Commands run in the same shell keep state (cwd, exports). Returns session_id used by shell_exec / shell_stop. Optional workdir."
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

impl ShellStartTool {
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

/// `shell_exec`：在指定会话里执行一条命令。
pub struct ShellExecTool {
    shell: ShellHandle,
    guard: Option<crate::CommandGuard>,
}

#[async_trait]
impl echo_tool::Tool for ShellExecTool {
    fn name(&self) -> &str {
        "shell_exec"
    }
    fn description(&self) -> &str {
        "Run one command inside a persistent shell session (see shell_start). Output is streamed to the Panel live. Timeout: 120s default, max 300s; on timeout the session is kept."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {"type": "string", "description": "Session id from shell_start"},
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

impl ShellExecTool {
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

/// `shell_stop`：销毁会话（终止 bash 及全部子孙进程组）。
pub struct ShellStopTool {
    shell: ShellHandle,
}

#[async_trait]
impl echo_tool::Tool for ShellStopTool {
    fn name(&self) -> &str {
        "shell_stop"
    }
    fn description(&self) -> &str {
        "Stop and destroy a persistent shell session."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {"type": "string", "description": "Session id from shell_start"}
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

impl ShellStopTool {
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
