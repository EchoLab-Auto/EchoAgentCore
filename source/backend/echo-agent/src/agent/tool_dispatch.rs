//! 工具分派（宿主侧）：把 `Agent` 的能力实现成执行链的四个接缝。
//!
//! 2026-11 全插件化 S2：执行链本体（参数预检 / 超时口径 / 调用与结果成对 /
//! 出口脱敏）已搬进机制层 [`echo_tool::execute`]。本文件只剩**宿主的三件事**：
//!
//! | 接缝 | 本文件的实现 |
//! |---|---|
//! | [`redact`](ToolExecutionHost::redact) | `Agent::redact_text_tracked`（组合根注入的脱敏器） |
//! | [`on_call`](ToolExecutionHost::on_call) | `BackendEvent::ToolCall` + `SessionEvent::ToolCall` |
//! | [`on_result`](ToolExecutionHost::on_result) | `BackendEvent::ToolResult` + `SessionEvent::ToolResult` + **状态广播** |
//!
//! ## 这里曾经有三张按工具名的表（2026-11 S7 删完）
//!
//! | 表 | 曾经的形状 | 现在 |
//! |---|---|---|
//! | 参数注入 | `match tool { "checklist" => __session_id, "shell_*" => __team_id }` | 工具覆写 `Tool::execute_in` 直接拿上下文 |
//! | 编排分派 | `if call.name == "spawn_subagent"` → 另一条通道 | 它就是普通工具，覆写 `execute_in` |
//! | 状态广播 | `match state_key { "checklist" => ChecklistUpdated }` | 绑定时声明（见 [`Agent::tool_state_events`]） |
//!
//! 判据很简单：**执行链不该认识任何一个具体工具的名字**。名字只出现在两个
//! 地方——工具自己的实现、以及绑定它的那行注册。

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
        // 状态广播：工具自己声明"我有可见状态"（`Tool::state_key`），**绑定它
        // 的那个插件**声明线上形状（`Agent::tool_state_events` 里的构造器）。
        //
        // 这里曾经是一张 `match state_key { "checklist" => … }` 表——那等于让
        // 执行链认识具体工具的名字。现在只剩一次查表：没有绑定就记一条 debug
        // （工具声明了状态但没人认领它的线上形状）。
        if let Some((key, state)) = tool_state(&agent.tools, &call.name, &cx.session_id) {
            match agent.tool_state_event(&key) {
                Some(build) => agent.emit(build(&cx.session_id, &state)),
                None => tracing::debug!(
                    tool = %call.name,
                    state_key = %key,
                    "工具声明了状态，但没有注册它的线上事件（丢弃）"
                ),
            }
        }
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
        // 取消信号：本分支（若有）的令牌包一层定义层能力递进去——工具要
        // 「主 turn 取消时我也停」时自己等它，宿主不必知道谁需要。
        let cancel = self
            .cancel_signal_of(branch_id)
            .map(|token| std::sync::Arc::new(TurnCancellation(token))
                as std::sync::Arc<dyn echo_defs::tool::Cancellation>);
        let cx = ToolCallContext {
            session_id: session_id.to_string(),
            branch_id: branch_id.to_string(),
            team_id: self.team_id(),
            cancel,
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


/// 状态快照 → 线上事件的构造器（词汇在协议层：插件与宿主都要用）。
pub use echo_protocol::ToolStateEventBuilder;

/// 把宿主的 `CancellationToken` 包成定义层的 [`echo_defs::tool::Cancellation`]。
///
/// 为什么需要这一层：定义层不能依赖 `tokio_util`（那会把一个运行时类型钉进
/// 词汇表），而工具要的是"能不能等它取消"这个**能力**。包装只有两个方法，
/// 却让 `spawn_subagent` 不必认识宿主的令牌类型。
struct TurnCancellation(tokio_util::sync::CancellationToken);

impl echo_defs::tool::Cancellation for TurnCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    fn when_cancelled(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.0.cancelled())
    }
}

impl Agent {
    /// 某个 turn 分支的取消令牌（找不到 = 该分支已结束 / 无分支语义）。
    pub(crate) fn cancel_signal_of(
        &self,
        branch_id: &str,
    ) -> Option<tokio_util::sync::CancellationToken> {
        self.active_inbound_turns
            .get(branch_id)
            .map(|turn| turn.cancel.clone())
    }
}

impl Agent {
    /// 注册一个「状态快照 key → 线上事件」的绑定（**由绑定该工具的插件声明**）。
    ///
    /// 例（`echo-plugin-tools` 装配 checklist 时）：
    /// ```ignore
    /// agent.register_tool_state_event("checklist", |session_id, state| {
    ///     BackendEvent::ChecklistUpdated { session_id: session_id.into(), state: state.clone(), team_id: None }
    /// });
    /// ```
    ///
    /// 同 key 重复注册 = 后到者赢（插件重装时刷新形状；这也是热重载的语义）。
    pub fn register_tool_state_event(
        &self,
        key: impl Into<String>,
        build: ToolStateEventBuilder,
    ) {
        if let Ok(mut map) = self.tool_state_events.write() {
            map.insert(key.into(), build);
        }
    }

    /// 查一个状态 key 的线上事件构造器（执行链用；无绑定 = `None`）。
    pub(crate) fn tool_state_event(&self, key: &str) -> Option<ToolStateEventBuilder> {
        self.tool_state_events
            .read()
            .ok()
            .and_then(|map| map.get(key).cloned())
    }
}
