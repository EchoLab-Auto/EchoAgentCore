//! 后台 shell 工具：shell_start / shell_exec / shell_stop。
//!
//! 与 Panel 的 Shell 面板共用同一个进程级 [`ShellManager`]：
//! LLM 可借此维护持久 shell 上下文（cd、变量、长任务），面板做终端可视化。

use async_trait::async_trait;
use serde_json::json;

use crate::shell::shell_manager_global;

pub fn register_shell_tools(registry: &mut crate::tool::ToolRegistry) {
    registry.register(std::sync::Arc::new(ShellStartTool));
    registry.register(std::sync::Arc::new(ShellExecTool));
    registry.register(std::sync::Arc::new(ShellStopTool));
}

struct ShellStartTool;

#[async_trait]
impl crate::tool::Tool for ShellStartTool {
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
    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> Result<String, crate::tool::ToolError> {
        let manager = shell_manager_global()
            .ok_or_else(|| crate::tool::ToolError::Execution("shell manager unavailable".into()))?;
        let workdir = arguments["workdir"].as_str().map(|s| s.to_string());
        let info = manager
            .start(
                workdir,
                crate::shell::current_tool_team_id(),
                &crate::shell::shell_emit(),
            )
            .await
            .map_err(crate::tool::ToolError::Execution)?;
        Ok(format!(
            "shell session started: {} (workdir: {})",
            info.session_id, info.workdir
        ))
    }
}

struct ShellExecTool;

#[async_trait]
impl crate::tool::Tool for ShellExecTool {
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
    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> Result<String, crate::tool::ToolError> {
        let session_id = arguments["session_id"].as_str().ok_or_else(|| {
            crate::tool::ToolError::InvalidArguments("session_id required".into())
        })?;
        let command = arguments["command"]
            .as_str()
            .ok_or_else(|| crate::tool::ToolError::InvalidArguments("command required".into()))?;
        super::reject_core_self_stop(command)?;
        let timeout = arguments["timeout_secs"].as_u64().map(|t| t.min(300));
        let manager = shell_manager_global()
            .ok_or_else(|| crate::tool::ToolError::Execution("shell manager unavailable".into()))?;
        let (output, success, timed_out) = manager
            .exec(session_id, command, timeout, &crate::shell::shell_emit())
            .await
            .map_err(crate::tool::ToolError::Execution)?;
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

struct ShellStopTool;

#[async_trait]
impl crate::tool::Tool for ShellStopTool {
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
    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> Result<String, crate::tool::ToolError> {
        let session_id = arguments["session_id"].as_str().ok_or_else(|| {
            crate::tool::ToolError::InvalidArguments("session_id required".into())
        })?;
        let manager = shell_manager_global()
            .ok_or_else(|| crate::tool::ToolError::Execution("shell manager unavailable".into()))?;
        manager
            .stop(session_id, &crate::shell::shell_emit())
            .await
            .map_err(crate::tool::ToolError::Execution)?;
        Ok(format!("shell session {session_id} stopped"))
    }
}
