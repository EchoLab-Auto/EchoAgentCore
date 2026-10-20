//! 工具分派（宿主侧）：把 `Agent` 的能力实现成执行链的四个接缝。
//!
//! 2026-11 全插件化 S2：执行链本体（参数预检 / 超时口径 / 调用与结果成对 /
//! 出口脱敏���已搬进机制层 [`echo_tool::execute`]。本文件只剩**宿主的四件事**：
//!
//! | 接缝 | 本文件的实现 |
//! |---|---|
//! | [`redact`](ToolExecutionHost::redact) | `Agent::redact_text_tracked`（组合根注入的脱敏器） |
//! | [`on_call`](ToolExecutionHost::on_call) | `BackendEvent::ToolCall` + `SessionEvent::ToolCall` |
//! | [`on_result`](ToolExecutionHost::on_result) | `BackendEvent::ToolResult` + `SessionEvent::ToolResult` + **状态广播** |
//! | [`run_orchestration`](ToolExecutionHost::run_orchestration) | `spawn_subagent`（异步编排工具） |
//! | [`tool_arguments`](ToolExecutionHost::tool_arguments) | `__session_id` / `__team_id` 的**宿主映射表** |
//!
//! 两条特判（`checklist` 的状态广播、按工具名注入会话参数）都从"执行链里
//! 的 if"变成了"工具声明 + 宿主映射"——执行链不认识任何工具名。

use echo_tool::execute::{tool_state, ToolCallContext, ToolExecutionHost, ToolInvocation};

use super::*;

/// 宿主侧的执行链接缝实现：**借用** `&Agent`。
///
/// 为什么借用而不是持有 `Arc<Agent>`：接缝需要的全部是 `Agent` 上的方法，
/// 存 `Arc` 会与 `Agent` 自身成引用环；而给 `Agent` 加一个"自引用 Arc"字段
/// 只是把环藏起来。执行链按调用传 `&dyn ToolExecutionHost`，借用即可
/// （见 `ToolExecutor::run`）。
struct AgentToolHost<'a> {
    agent: &'a Agent,
}

impl ToolExecutionHost for AgentToolHost<'_> {
    fn redact(&self, text: &str) -> (String, usize) {
        self.agent.redact_text_tracked(text)
    }

    fn on_call(
        &self,
        cx: &ToolCallContext,
        call: &ToolInvocation,
        logged_arguments: &str,
        started_at_ms: i64,
    ) {
        let agent = &self.agent;
        agent.emit(BackendEvent::ToolCall {
            session_id: cx.session_id.clone(),
            team_id: None,
            tool_name: call.name.clone(),
            arguments: logged_arguments.to_string(),
            tool_call_id: call.id.clone(),
            branch_id: cx.branch_id.clone(),
            started_at_ms: Some(started_at_ms),
        });
        // 工具调用是**持久事件**：模型可见的循环（call + result）必须能从
        // 日志重建，否则重载后投影会合成一个没有回执的 tool_use，provider
        // 直接 400。
        agent
            .trunk
            .append_event(echo_session::SessionEvent::ToolCall(
                echo_session::event::ToolCallEvent {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: logged_arguments.to_string(),
                    session: Some(cx.session_id.clone()),
                    started_at_ms: Some(started_at_ms),
                },
            ));
    }

    fn on_result(
        &self,
        cx: &ToolCallContext,
        call: &ToolInvocation,
        result: &echo_tool::ToolResult,
        elapsed_ms: u64,
    ) {
        let agent = &self.agent;
        agent.emit(BackendEvent::ToolResult {
            session_id: cx.session_id.clone(),
            team_id: None,
            tool_name: call.name.clone(),
            result: result.text.clone(),
            tool_call_id: call.id.clone(),
            timed_out: false,
            branch_id: cx.branch_id.clone(),
            elapsed_ms: Some(elapsed_ms),
        });
        agent
            .trunk
            .append_event(echo_session::SessionEvent::ToolResult(
                echo_session::event::ToolResultEvent {
                    tool_call_id: call.id.clone(),
                    result: result.text.clone(),
                    images: result.images.clone(),
                    session: Some(cx.session_id.clone()),
                    elapsed_ms: Some(elapsed_ms),
                },
            ));
        // 状态广播：工具自己声明"我有可见状态"（`Tool::state_key`），执行链
        // 负责在正确的时机问一句，**线上形状**归宿主。
        //
        // 这里的 match 是**协议映射表**（state_key → 线上事件变体），不是分派
        // 逻辑：新增一个有状态面板的工具只需实现 `state_key` + 在协议里加一个
        // 变体，不必碰执行链。等协议收敛成通用的 `ToolStateUpdated`，这张表
        // 整个消失。
        if let Some((key, state)) = tool_state(&agent.tools, &call.name, &cx.session_id) {
            match key {
                "checklist" => agent.emit(BackendEvent::ChecklistUpdated {
                    session_id: cx.session_id.clone(),
                    state,
                    team_id: None, // annotate_team 统一补
                }),
                other => tracing::debug!(
                    tool = %call.name,
                    state_key = other,
                    "工具声明了状态，但协议里没有对应事件（丢弃）"
                ),
            }
        }
    }

    fn run_orchestration(
        &self,
        cx: &ToolCallContext,
        call: &ToolInvocation,
        args: &serde_json::Value,
    ) -> Option<Result<echo_tool::ToolResult, crate::tool::ToolError>> {
        if call.name != crate::subagent::SPAWN_SUBAGENT_TOOL {
            return None;
        }
        let agent = &self.agent;
        // 异步编排工具：插件未挂载 / persona 不允许时按未注册工具报错。
        if !agent.allows_dynamic_tool(crate::subagent::SPAWN_SUBAGENT_TOOL) {
            return Some(Err(crate::tool::ToolError::NotFound(format!(
                "tool '{}' is not registered (subagent 插件未启用或不在本 persona 白名单)",
                crate::subagent::SPAWN_SUBAGENT_TOOL
            ))));
        }
        let (store, spawn) = agent
            .subagent_runtime()
            .expect("allows_dynamic_tool checked the runtime is attached");
        let tool = crate::subagent::SpawnSubagentTool::new(store, spawn);
        let cancel = agent
            .active_inbound_turns
            .get(&cx.branch_id)
            .map(|turn| turn.cancel.clone())
            .unwrap_or_default();
        Some(
            tool.spawn(args, &cx.session_id, cancel, &cx.branch_id)
                .map(crate::tool::ToolResult::text)
                .map_err(crate::tool::ToolError::Execution),
        )
    }

    fn tool_arguments(
        &self,
        cx: &ToolCallContext,
        tool: &str,
        mut args: serde_json::Value,
    ) -> serde_json::Value {
        // 按工具隔离会话状态的两条历史约定（清单按会话、Shell 按人格）。
        //
        // 这仍是宿主知识（它知道哪些工具按会话/人格隔离），但已经**不在执行链
        // 里**——执行链只问一句"这个工具要补什么参数"。终态是工具自己在注册时
        // 声明它需要哪个维度的上下文（等 `Tool` 词汇长出一个 `context_scope`），
        // 届时这张表也消失。
        match tool {
            "checklist" => {
                args["__session_id"] = serde_json::json!(cx.session_id);
            }
            "shell_start" | "shell_exec" | "shell_stop" => {
                args["__team_id"] = serde_json::json!(self.agent.team_id());
            }
            _ => {}
        }
        args
    }
}

impl Agent {
    /// 工具统一入口（注册表工具 + 编排工具）。
    ///
    /// pub：集成测试经此直接驱动分派（不绕 LLM）。签名保持不变——执行链搬家
    /// 不改变调用方的任何一行。
    pub async fn run_tool(
        &self,
        session_id: &str,
        branch_id: &str,
        call: &ToolCall,
    ) -> crate::tool::ToolResult {
        let cx = ToolCallContext {
            session_id: session_id.to_string(),
            branch_id: branch_id.to_string(),
            team_id: None,
        };
        let invocation =
            ToolInvocation::new(call.id.clone(), call.name.clone(), call.arguments.clone());
        let host = AgentToolHost { agent: self };
        self.tool_executor().run(&host, &cx, &invocation).await
    }

    /// 执行器实例（每次调用构造一次：它只持有两个 `Arc`，构造成本可忽略，
    /// 换来的是"执行链接缝绑定本 agent"这件事不需要存字段）。
    pub(crate) fn tool_executor(&self) -> echo_tool::execute::ToolExecutor {
        echo_tool::execute::ToolExecutor::new(self.tools.clone())
    }

    /// Record the `ToolResult` for a call whose `run_tool` future was dropped
    /// (timeout or cancellation). `run_tool` appends the `ToolCall` event
    /// before dispatching, so without this the durable log would keep a
    /// dangling call: after a reload the projection synthesizes an assistant
    /// tool_use no result answers, and the provider rejects the request
    /// (HTTP 400). Also emits the backend event so the UI can close out the
    /// pending tool row. `elapsed_ms` = 开始执行到中断判定为止的耗时。
    pub(super) fn record_interrupted_tool_result(
        &self,
        session_id: &str,
        branch_id: &str,
        call: &ToolCall,
        result: &str,
        timed_out: bool,
        elapsed_ms: u64,
    ) {
        self.emit(BackendEvent::ToolResult {
            session_id: session_id.to_string(),
            team_id: None,
            tool_name: call.name.clone(),
            result: result.to_string(),
            tool_call_id: call.id.clone(),
            timed_out,
            branch_id: branch_id.to_string(),
            elapsed_ms: Some(elapsed_ms),
        });
        self.trunk
            .append_event(echo_session::SessionEvent::ToolResult(
                echo_session::event::ToolResultEvent {
                    tool_call_id: call.id.clone(),
                    result: result.to_string(),
                    images: vec![],
                    session: Some(session_id.to_string()),
                    elapsed_ms: Some(elapsed_ms),
                },
            ));
    }
}

/// 工具超时提示文案（所有 turn 路径共用同一文案）。
pub(super) fn tool_timeout_notice(tool: &str, secs: u64) -> String {
    format!(
        "notice: tool '{tool}' timed out after {secs}s and its execution was aborted. You may retry this tool (e.g. with a shorter command) or continue the answer directly with the information you already have; do not treat this timeout as a fatal failure."
    )
}

