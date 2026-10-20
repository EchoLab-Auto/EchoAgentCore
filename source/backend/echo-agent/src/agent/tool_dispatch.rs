//! 工具分派：参数预检、`run_tool` 统一入口（注册表工具 + 编排工具特判）、
//! 中断结果补记（保持事件日志 ToolCall/ToolResult 成对）。
//!
//! 2026-10 架构评审：自 `agent/mod.rs` 拆出（纯搬移，行为不变）。

use tracing::Instrument as _;

use super::*;

impl Agent {
    /// 模型可见的工具参数预检：schema 必需字段缺失时返回纠正性错误文案
    /// （说清"你发了什么、应该发什么"），返回 None 表示通过预检。
    pub(super) async fn tool_arguments_error(
        &self,
        tool_name: &str,
        raw_arguments: &str,
        args: &serde_json::Value,
    ) -> Option<String> {
        crate::agent::tool_exec::tool_arguments_error(&self.tools, tool_name, raw_arguments, args)
            .await
    }

    /// 工具统一分派入口（注册表工具 + 编排工具 spawn_subagent 特判）。
    /// pub：集成测试经此直接驱动分派（不绕 LLM）。
    pub async fn run_tool(
        &self,
        session_id: &str,
        branch_id: &str,
        call: &ToolCall,
    ) -> crate::tool::ToolResult {
        // 执行开始时刻：epoch 毫秒进事件（面板「运行中实时计时」用），
        // Instant 用于本地测「结束减开始」的耗时。
        let started = std::time::Instant::now();
        let started_at_ms = chrono::Utc::now().timestamp_millis();
        // 参数出口脱敏（2026-10 安全）：事件 / 会话落盘 / 面板都不得出现
        // 明文秘密。执行仍用原始参数（功能正确性不受影响）——被替换的
        // 只有"记录面"。
        let logged_arguments = self.redact_text(&call.arguments);
        self.emit(BackendEvent::ToolCall {
            session_id: session_id.to_string(),
            team_id: None,
            tool_name: call.name.clone(),
            arguments: logged_arguments.clone(),
            tool_call_id: call.id.clone(),
            branch_id: branch_id.to_string(),
            started_at_ms: Some(started_at_ms),
        });
        // The tool call is a durable event: the model-visible loop (call +
        // result) must be reconstructable from the log after a reload.
        self.trunk
            .append_event(echo_session::SessionEvent::ToolCall(
                echo_session::event::ToolCallEvent {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: logged_arguments,
                    session: Some(session_id.to_string()),
                    started_at_ms: Some(started_at_ms),
                },
            ));
        //
        // 一次工具调用 = 一个 span（2026-10 日志治理）：并发工具 / 多会话下
        // 「这几条 warn 属于哪个工具」靠 span 归组，JSON 出口自动带
        // `session`/`branch_id`/`tool`/`tool_call_id`。
        let span = tracing::info_span!(
            "tool_call",
            tool = %call.name,
            session = %session_id,
            branch_id = %branch_id,
            tool_call_id = %call.id,
        );
        let result = self
            .dispatch_tool(session_id, branch_id, call)
            .instrument(span)
            .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        // 工具耗时是 turn 耗时的主要成分：慢调用与失败恒 INFO（默认级别下
        // 「时间花在哪」有据可查），快调用降 DEBUG（不给正常路径刷噪声）。
        let characters = result.text.chars().count();
        let failed = result.text.starts_with("error:");
        if elapsed_ms >= 1000 || failed {
            tracing::info!(
                session = %session_id,
                branch_id = %branch_id,
                tool = %call.name,
                elapsed_ms,
                characters,
                ok = !failed,
                "tool call finished"
            );
        } else {
            tracing::debug!(
                session = %session_id,
                branch_id = %branch_id,
                tool = %call.name,
                elapsed_ms,
                characters,
                "tool call finished"
            );
        }
        let result_text = result.text.clone();
        let result_images = result.images.clone();
        self.emit(BackendEvent::ToolResult {
            session_id: session_id.to_string(),
            team_id: None,
            tool_name: call.name.clone(),
            result: result_text.clone(),
            tool_call_id: call.id.clone(),
            timed_out: false,
            branch_id: branch_id.to_string(),
            elapsed_ms: Some(elapsed_ms),
        });
        self.trunk
            .append_event(echo_session::SessionEvent::ToolResult(
                echo_session::event::ToolResultEvent {
                    tool_call_id: call.id.clone(),
                    result: result_text.clone(),
                    images: result_images.clone(),
                    session: Some(session_id.to_string()),
                    elapsed_ms: Some(elapsed_ms),
                },
            ));
        if call.name == "checklist" {
            // 按会话过滤（2026-10 P0 修复）：此前取全量状态挂当前会话广播
            // ——A 会话触发时 B 会话的数据也会挂到 A 上（面板按会话显示必串）。
            if let Some(state) = self.tools.snapshot_for_session(&call.name, session_id) {
                self.emit(BackendEvent::ChecklistUpdated {
                    session_id: session_id.to_string(),
                    state,
                    team_id: None, // annotate_team 统一补
                });
            }
        }
        result
    }

    /// 工具执行体（参数解析 → 预检 → 分派 → 结果脱敏）。
    ///
    /// 与 [`Self::run_tool`] 拆开是为了让整段执行被 `tool_call` span 包裹：
    /// `Instrument` 需要一个 future，而 `run_tool` 开头的同步序言
    /// （发出 ToolCall 事件 + 会话落盘）不应计入工具耗时。
    async fn dispatch_tool(
        &self,
        session_id: &str,
        branch_id: &str,
        call: &ToolCall,
    ) -> crate::tool::ToolResult {
        // 参数解析与预检：给模型可纠正的错误反馈。非法 JSON 或缺少必需
        // 字段时，错误文案必须说清"你发了什么、应该发什么"——一句模糊的
        // "command required" 只会让模型原样重试，形成空调用退化循环。
        let result = match serde_json::from_str::<serde_json::Value>(&call.arguments) {
            Err(error) => crate::tool::ToolResult::text(format!(
                "error: 工具参数不是合法 JSON（{error}）。你发送的原始参数: {}。请修正为合法 JSON 后重新调用 {}。",
                crate::llm::truncate(&call.arguments, 200),
                call.name,
            )),
            Ok(args) => match call.name.as_str() {
            crate::subagent::SPAWN_SUBAGENT_TOOL => {
                // 异步编排工具：插件未挂载 / persona 不允许时按未注册工具报错。
                if !self.allows_dynamic_tool(crate::subagent::SPAWN_SUBAGENT_TOOL) {
                    Err(format!(
                        "tool '{}' is not registered (subagent 插件未启用或不在本 persona 白名单)",
                        crate::subagent::SPAWN_SUBAGENT_TOOL
                    ))
                } else {
                    match self.tool_arguments_error(
                        crate::subagent::SPAWN_SUBAGENT_TOOL,
                        &call.arguments,
                        &args,
                    ).await {
                        Some(message) => Err(message),
                        None => {
                            let (store, spawn) = self
                                .subagent_runtime()
                                .expect("allows_dynamic_tool checked the runtime is attached");
                            let tool = crate::subagent::SpawnSubagentTool::new(store, spawn);
                            let cancel = self
                                .active_inbound_turns
                                .get(branch_id)
                                .map(|turn| turn.cancel.clone())
                                .unwrap_or_default();
                            tool.spawn(&args, session_id, cancel, branch_id)
                                .map(crate::tool::ToolResult::text)
                        }
                    }
                }
            }
            other => {
                // Every dynamic tool must live in the single dispatch
                match self.tool_arguments_error(other, &call.arguments, &args).await {
                    Some(message) => Err(message),
                    None => {
                        // 会话/归属上下文注入（2026-10）：在 schema 校验
                        // 之后、执行之前——checklist 按会话隔离状态（P0
                        // 修复：此前全局共享，跨会话互看互覆）；shell
                        // 三件套按归属校验（替代线程本地方案，跨 await
                        // 不可靠问题顺带消除）。不进 ToolCall 事件（事件
                        // 用原始参数）。
                        let mut args = args;
                        match other {
                            "checklist" => {
                                args["__session_id"] = serde_json::json!(session_id);
                            }
                            "shell_start" | "shell_exec" | "shell_stop" => {
                                args["__team_id"] = serde_json::json!(self.team_id());
                            }
                            _ => {}
                        }
                        self.tools
                            .execute_rich(other, args)
                            .await
                            .map_err(|error| error.to_string())
                    }
                }
            }
        }
        .unwrap_or_else(|error| crate::tool::ToolResult::text(format!("error: {error}"))),
        };
        let mut result = result;
        let (result_text, hit_count) = self.redact_text_tracked(&result.text);
        if hit_count > 0 {
            tracing::warn!(
                session = %session_id,
                tool = %call.name,
                hits = hit_count,
                "tool result contained sensitive data; redacted at exit"
            );
        }
        result.text = result_text;
        result
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
