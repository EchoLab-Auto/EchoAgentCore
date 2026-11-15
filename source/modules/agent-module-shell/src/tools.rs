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

use crate::session::{Caller, CommandGuard, ShellHandle};

/// 这次调用的**主体**：只来自执行上下文（`Tool::execute_in` 递进来的
/// [`echo_defs::tool::ToolContext`]）。
///
/// - `cx.team_id`：本地 persona（宿主执行链注入）；
/// - `cx.peer`：跨节点来源（联邦 `handle_invoke` 从链路注入）——**绝不**
///   从参数读取；
/// - 没有上下文 = 直接经 `registry.execute` 的宿主/管理路径
///   （`manage = true`，只能由宿主代码走到）。
///
/// 历史魔术参数 `__team_id` 已删除：参数是模型可见面，拿它当身份等于让
/// 被授权者自带授权（远端调用只要塞一个 `__team_id` 就能操作别人的会话）。
fn caller(cx: Option<&echo_defs::tool::ToolContext>) -> Caller<'_> {
    match cx {
        Some(cx) => Caller {
            team: cx.team_id.as_deref().filter(|id| !id.is_empty()),
            peer: cx.peer.as_deref().filter(|id| !id.is_empty()),
            manage: false,
        },
        None => Caller {
            team: None,
            peer: None,
            manage: true,
        },
    }
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
    /// ## 归属语义（2026-11 收紧）
    ///
    /// 远端 invoke 现在**带上下文**：联邦 `handle_invoke` 用
    /// [`echo_defs::tool::ToolContext::for_remote_peer`] 注入来源 `node_id`，
    /// 会话创建时记入 `owner_peer`。此后：
    ///
    /// - 同一 peer 可以继续 `bash-exec` / `bash-stop`；
    /// - **其他 peer** 与**本地 persona** 默认一律拒绝（此前远端调用
    ///   `requester = None` 直接放行，等于任何能调 `bash` 的 peer 都能操作
    ///   任意会话）；面板的显式管理路径（`manage`）仍可清理泄漏会话。
    ///
    /// `session_id` 只是句柄：旧 id（`sh-1`）可枚举、新 id 带随机后缀，
    /// 但**授权从不看 id**——只看创建时绑定的归属主体。
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
        // 归属在创建时绑定：本地 persona 或远端 peer（二者互斥）。
        let caller = caller(cx);
        let info = manager
            .start_owned(
                workdir,
                caller.team.map(str::to_string),
                caller.peer.map(str::to_string),
                &self.shell.emit(),
            )
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
        // 归属校验：只认执行上下文里的主体（persona / 远端 peer），
        // 不认参数、不认会话 id。
        manager
            .ensure_access(session_id, &caller(cx))
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
        // 归属校验：同 exec——主体来自上下文，不来自参数。
        manager
            .ensure_access(session_id, &caller(cx))
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

    /// 远端会话归属（2026-11 安全修复）：来源 peer 绑定在创建时写入，
    /// 之后只有同一 peer 能操作；其他 peer / 本地 persona 一律拒绝，
    /// **参数里的 `__team_id` 不再是身份**。
    #[tokio::test]
    async fn remote_sessions_are_bound_to_their_origin_peer() {
        let manager = std::sync::Arc::new(crate::session::ShellManager::new());
        let shell = ShellHandle::new();
        shell.bind(manager.clone(), std::sync::Arc::new(|_| {}));
        let start = BashStartTool {
            shell: shell.clone(),
        };
        let exec = BashExecTool {
            shell: shell.clone(),
            guard: None,
        };
        let stop = BashStopTool { shell };

        // 来源 peer 起会话。
        let origin = echo_defs::tool::ToolContext::for_remote_peer("node-gpu");
        let started = start
            .execute_in(&origin, json!({}))
            .await
            .expect("remote bash-start");
        let session_id = started
            .text
            .split_whitespace()
            .nth(3)
            .expect("session id in output")
            .to_string();
        assert!(
            manager
                .session_info(&session_id)
                .expect("session exists")
                .team_id
                .as_deref()
                .is_some_and(|team| team == "peer:node-gpu"),
            "远端会话应显示为 peer:node-gpu"
        );

        // 另一 peer：即使显式塞 `__team_id`，也拒绝。
        let other = echo_defs::tool::ToolContext::for_remote_peer("node-evil");
        let error = exec
            .execute_in(
                &other,
                json!({
                    "session_id": session_id,
                    "command": "echo hacked",
                    "__team_id": "node-gpu",
                }),
            )
            .await
            .expect_err("cross-peer exec must be rejected");
        assert!(error.to_string().contains("无权操作"), "{error}");

        // 本地 persona：拒绝。
        let local = echo_defs::tool::ToolContext {
            team_id: Some("EchoCode".into()),
            ..Default::default()
        };
        let error = exec
            .execute_in(
                &local,
                json!({"session_id": session_id, "command": "echo local"}),
            )
            .await
            .expect_err("local persona must not touch a remote session");
        assert!(error.to_string().contains("无权操作"), "{error}");

        // 来源 peer：放行（exec + stop）。
        let output = exec
            .execute_in(
                &origin,
                json!({"session_id": session_id, "command": "echo ok"}),
            )
            .await
            .expect("owner peer exec");
        assert!(output.text.contains("ok"), "{}", output.text);
        stop.execute_in(&origin, json!({"session_id": session_id}))
            .await
            .expect("owner peer stop");
        assert!(manager.session_info(&session_id).is_none());
    }

    /// 本地 persona 会话：远端 peer 带 `__team_id` 也进不去（参数不是身份）。
    #[tokio::test]
    async fn arguments_cannot_forge_a_persona_identity() {
        let manager = std::sync::Arc::new(crate::session::ShellManager::new());
        let shell = ShellHandle::new();
        shell.bind(manager.clone(), std::sync::Arc::new(|_| {}));
        let info = manager
            .start(Some("/tmp".into()), Some("EchoCode".into()), &shell.emit())
            .await
            .expect("local session");
        let exec = BashExecTool {
            shell: shell.clone(),
            guard: None,
        };
        let remote = echo_defs::tool::ToolContext::for_remote_peer("node-gpu");
        let error = exec
            .execute_in(
                &remote,
                json!({
                    "session_id": info.session_id,
                    "command": "echo hacked",
                    "__team_id": "EchoCode",
                }),
            )
            .await
            .expect_err("static team_id arg must not authorize");
        assert!(error.to_string().contains("无权操作"), "{error}");
        manager
            .stop(&info.session_id, &shell.emit())
            .await
            .expect("stop");
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
