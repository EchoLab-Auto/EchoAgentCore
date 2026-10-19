//! Turn 驱动（harness 侧）：把一条已登记的输入交给 echo-loop `TurnRunner`
//! 执行，并负责提示词构建、工具执行器（超时 / 取消 / visible_reply）、
//! 流式转发与 runner 生命周期事件 → Panel 事件的翻译。
//!
//! 2026-10 架构评审：从 `agent/mod.rs` 抽出，并删除并行存在的内置循环
//! （单轨化，见 `document/core/core-agent-loop.md`）。

use anyhow::{anyhow, Result};

use super::boundary::BoundaryKind;
use super::streaming::spawn_delta_forwarder;
use super::tool_dispatch::tool_timeout_notice;
use super::{structured_message_sequence, Agent, TURN_CANCELLED};
use crate::event::BackendEvent;
use crate::llm::{ChatMessage, ChatRequest};
use crate::session::Session;

/// 按内容标记（而非会话平台）判定输入边界：平台会话也可能收到后台 / TUI
/// 输入（无 hook），那类输入必须在后台作答、绝不推送到平台。
///
/// **框架不认识任何平台名**：平台由标记本身（`<X_message_hook>`）带出来，
/// 「是否平台会话」用 [`SessionKey::is_local`] 判断（`local` 是宿主自己的
/// 平台标识）。
pub(super) fn input_boundary(session: &Session, content: &str) -> Option<BoundaryKind> {
    if let Some(platform) = crate::input_marker::platform_hook_of(content) {
        Some(BoundaryKind::PlatformHook {
            platform: platform.to_string(),
        })
    } else if content.contains(crate::input_marker::TIMER_EVENT_OPEN) {
        Some(BoundaryKind::Timer)
    } else if !session.session_key.is_local() {
        Some(BoundaryKind::BackendInput)
    } else {
        None
    }
}

impl Agent {
    /// 当前 turn 驱动：组合根注入的共享 runner；未注入（单元测试 / 独立
    /// 嵌入）时按本 agent 的 provider 与配置惰性构建一个默认 runner 并缓存。
    pub(super) async fn turn_runner(&self) -> std::sync::Arc<echo_loop::TurnRunner> {
        if let Some(runner) = self.loop_runner.read().await.clone() {
            return runner;
        }
        let mut slot = self.loop_runner.write().await;
        if let Some(runner) = slot.clone() {
            return runner;
        }
        let (max_tool_iterations, max_tokens) = {
            let cfg = self.config.read().await;
            (cfg.max_tool_iterations.max(1), cfg.effective_max_tokens())
        };
        let runner = std::sync::Arc::new(echo_loop::TurnRunner::new(
            std::sync::Arc::new(echo_context::EventBus::default()),
            self.provider.read().await.clone(),
            std::sync::Arc::new(echo_loop::ToolPipeline::new()),
            echo_loop::LoopOptions {
                max_tool_iterations,
                max_tokens,
            },
        ));
        *slot = Some(runner.clone());
        runner
    }

    /// 驱动一轮 turn（**唯一循环实现**：echo-loop `TurnRunner`）。
    ///
    /// 2026-10 架构评审单轨化：此前普通输入走 echo-loop、QQ hook / 定时器 /
    /// QQ 会话走内置循环（两套 ~300 行的同构循环，靠"逐项同口径"注释维持
    /// 一致，已多次漂移）。内置循环对边界输入的唯一差异是提示词边界块
    /// （[`BoundaryKind`]），现由本函数统一注入；其余语义（截断续写、超时
    /// 守卫、取消、visible_reply、图片透传、流式）本就由 runner + 本函数的
    /// executor 承担。
    ///
    /// `history_snapshot` 由调用方提供时以点切快照为基准，不重复读取。
    ///
    /// 承担的 harness 职责：
    /// - **UI 事件**：`AgentThinking` 起、`LlmRequest`/`LlmResponse`（经 runner
    ///   总线翻译）、`AgentReasoning`（回调）、`AgentCompleted` 收；
    /// - **/context 视图**：写 `last_prompt_blocks`；
    /// - **动态模型/provider**：模型每 turn 解析、provider 每 step 由
    ///   `ChatExecutor` 解析——运行期 persona API 切换即时生效；
    /// - **在途取消**：模型请求与工具执行均与 turn 取消竞速；
    /// - **工具产出**：图片透传（`ToolOutcome`）、成功 `send_*` 触发
    ///   visible_reply 抑制；
    /// - **超时守卫**：`tool_guard_timeout` + 共享 notice（超时只中止该工具）。
    pub(super) async fn process_message_inner(
        &self,
        session: &Session,
        content: &str,
        history_snapshot: Option<Vec<ChatMessage>>,
        turn_cancel: tokio_util::sync::CancellationToken,
        visible_reply: Option<tokio::sync::watch::Sender<bool>>,
        branch_id: &str,
    ) -> Result<String> {
        let runner = self.turn_runner().await;
        let queued_at = std::time::Instant::now();
        let message_sequence = structured_message_sequence(content);
        let session_id = session.id.clone();
        tracing::info!(
            session = %session_id,
            branch_id = %branch_id,
            message_sequence = message_sequence.unwrap_or_default(),
            "agent turn started"
        );
        self.emit(BackendEvent::AgentThinking {
            session_id: session_id.clone(),
            team_id: None,
        });
        // 系统提示词（分层）：base（persona
        // system_prompt 覆盖值）→ 全局 system 技能 → persona 系统技能
        // （capabilities.system_skills）→ … → 工作区会话注入 → 输入边界。
        let boundary = input_boundary(session, content);
        let (base, persona_skills, workspace) = {
            let base = self.config.read().await.system_prompt.clone();
            let persona_skills = self
                .capabilities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map(|c| c.system_skills.clone())
                .unwrap_or_default();
            // 按**本次 turn 的会话**解析工作区（2026-10 多窗口修复）：
            // 全局 active 是 per-persona 单槽，另一个窗口激活工作区后会被
            // 注入到这里，导致 B 的对话拿到 A 的项目上下文。
            let workspace = self.workspace_prompt_text_for(&session_id);
            (base, persona_skills, workspace)
        };
        let blocks = {
            let skills = self.skills.lock().await;
            crate::agent::prompt::build_prompt_blocks(
                &skills,
                &base,
                content,
                boundary,
                &persona_skills,
                workspace,
            )
            .await
        };
        let system_prompt = crate::agent::prompt::join_prompt_blocks(&blocks);
        // `/context` 视图：保存最近一次提示词块（按会话记录，多会话互不覆盖）。
        self.last_prompt_blocks.insert(session_id.clone(), blocks);
        let history = history_snapshot.unwrap_or_else(|| session.history.lock().clone());
        // 工具 schema：注册表是唯一真源。编排工具
        // （spawn_subagent）同样由注册表提供（组合根装配时注册），这里不得
        // 再追加同名定义——重复工具名会让 API 拒绝整个请求
        // （"Tool names must be unique"）。
        let tools = (*self.tools.definitions().await).clone();

        // 模型名（2026-10 巡检：每 turn 解析——运行期模型/provider 切换在
        // 下一 turn 生效；此前 runner 构造期固化，切换完全失效）。
        // 会话级覆盖（2026-10 用户报障修正）：有覆盖时必须取覆盖 profile
        // 的模型（provider 与模型成对解析），否则请求会带 persona 模型打
        // 到覆盖端点 → 404 model_not_found。
        let model = self.session_model(&session_id).await;

        // 推理回调（引用式，生命周期 = run 调用作用域）。
        let reasoning_cb = |sid: String, text: String| {
            self.emit_reasoning(&sid, branch_id, &Some(text));
        };

        // ── 模型往返观测（2026-10-19 审计缺口修复）──
        // 每步落一条 `ModelStep` 事件：step_index / 模型 / token /
        // 请求→响应耗时 / 本步工具调用 id。此前日志只有逐个 ToolCall，
        // "一条响应里几个工具"只能靠时间戳间隔猜（审计实测 95% 的响应
        // 被判定为单调用轮，无法区分"模型只发一个"与"框架丢弃并行"）。
        //
        // 时钟在同 turn 的 AgentRequest 观察者里刷新，在 ModelResponse 里
        // 读差值——两者都是 emit_sync、顺序固定（见 echo-loop runner）。
        let step_clock: std::sync::Arc<std::sync::Mutex<std::time::Instant>> =
            std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
        let step_seq = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // ── 生命周期事件翻译（2026-10 巡检）──
        // 订阅 runner 的事件总线，把 turn/step 事件转发为 Panel 事件
        // 。订阅按 session_id 过滤——runner 可被
        // 多会话并发共享；Disposer 随本作用域保管，函数返回即注销。
        let emit_handle = self.emit_handle();
        let _subscriptions = {
            let bus = runner.bus().clone();
            let mut disposers: Vec<echo_context::Disposer> = Vec::new();
            {
                let emit = emit_handle.clone();
                let sid = session_id.clone();
                let turn = branch_id.to_string();
                let clock = std::sync::Arc::clone(&step_clock);
                disposers.push(bus.observe::<echo_loop::AgentRequest, _>(move |evt| {
                    // session + turn 双过滤：并行模式同会话并发分支时
                    // 各 turn 的订阅互不串收（2026-10 巡检）。
                    if evt.session_id == sid && evt.turn_id.as_deref() == Some(turn.as_str()) {
                        // 模型请求发出：记下时刻，供本步 ModelStep 算往返耗时。
                        if let Ok(mut slot) = clock.lock() {
                            *slot = std::time::Instant::now();
                        }
                        emit(BackendEvent::LlmRequest {
                            session_id: evt.session_id.clone(),
                            model: evt.request.model.clone(),
                        });
                    }
                }));
            }
            {
                let emit = emit_handle.clone();
                let sid = session_id.clone();
                let turn = branch_id.to_string();
                let clock = std::sync::Arc::clone(&step_clock);
                let seq = std::sync::Arc::clone(&step_seq);
                let trunk = self.trunk.clone();
                disposers.push(bus.observe::<echo_loop::ModelResponse, _>(move |evt| {
                    if evt.session_id == sid && evt.turn_id.as_deref() == Some(turn.as_str()) {
                        // 本步往返耗时（请求发出 → 响应到达）。
                        let elapsed_ms = clock
                            .lock()
                            .ok()
                            .map(|since| since.elapsed().as_millis() as u64);
                        // 步号自增：`StepStart` 没有 turn_id（并发分支无法
                        // 归因），故用本 turn 自己的计数，口径与 runner 的
                        // step_index 一致（每步一次 ModelResponse）。
                        let step_index = seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // 落 durable 事件（模型不可见 / 不进时间线 / 不进
                        // 压缩转录）：审计据此算"每轮打包了几个工具"、每步
                        // 的 token 与延迟（2026-10-19 审计缺口）。
                        trunk.append_event(echo_session::SessionEvent::ModelStep(
                            echo_session::event::ModelStepEvent {
                                step_index,
                                model: evt.model.clone(),
                                prompt_tokens: evt.prompt_tokens,
                                completion_tokens: evt.completion_tokens,
                                tool_call_ids: evt
                                    .tool_calls
                                    .iter()
                                    .map(|call| call.id.clone())
                                    .collect(),
                                turn: evt.turn_id.clone(),
                                elapsed_ms,
                                started_at_ms: chrono::Utc::now().timestamp_millis(),
                                session: Some(evt.session_id.clone()),
                            },
                        ));
                        emit(BackendEvent::LlmResponse {
                            session_id: evt.session_id.clone(),
                            model: evt.model.clone(),
                            prompt_tokens: evt.prompt_tokens,
                            completion_tokens: evt.completion_tokens,
                        });
                    }
                }));
            }
            disposers
        };

        // 动态 chat 出口（2026-10 巡检）：每 step 解析**当前** provider 与
        // 预算（persona API 切换即时生效）；参数转 owned、future 仅借 self。
        //
        // 流式接线（2026-10）：主对话 turn 经 `chat_stream` + 增量转发
        // （`AgentContentDelta` / `AgentReasoningDelta`，~40ms 合并批）；
        // `stream_output=false` 时退回整段 `chat`。每 step 一个转发任务，
        // 返回前 flush 收尾（增量先于后续事件到达）。
        let stream_emitter = self.emit_handle();
        let stream_session = session_id.clone();
        let stream_branch = branch_id.to_string();
        let chat_executor: echo_loop::ChatExecutor<'_> = &|mut request: ChatRequest| {
            let this = self;
            let emitter = stream_emitter.clone();
            let sid = stream_session.clone();
            let bid = stream_branch.clone();
            Box::pin(async move {
                request.max_tokens = this.config.read().await.effective_max_tokens();
                // 会话级覆盖（2026-10）：该会话有 provider 覆盖时优先使用。
                let provider = this.resolve_session_provider(&sid).await;
                if !this.config.read().await.stream_output {
                    return provider.chat(&request).await;
                }
                let (delta_tx, delta_rx) = tokio::sync::mpsc::unbounded_channel();
                let forwarder = spawn_delta_forwarder(delta_rx, emitter, sid, bid);
                let result = provider.chat_stream(&request, delta_tx).await;
                let _ = forwarder.await;
                result
            })
        };

        // 工具执行器（异步版）：超时守卫（`tool_guard_timeout`）
        // + turn 取消竞速（在途工具可立即中止）+ visible_reply 抑制。
        // run_tool 内部已发 ToolCall/ToolResult 事件并写事件日志；被 drop 的
        // future 已记录 ToolCall 事件，补记中断结果保持事件日志成对。
        // 取消令牌的克隆副本（executor 闭包持有其引用；原令牌稍后 move 进
        // runner.run，#[E0505] 的规避——两者语义一致，同源取消）。
        let cancel_for_tools = turn_cancel.clone();
        let executor: echo_loop::ToolExecutor<'_> =
            &|sid: &str, _branch: &str, call: &crate::llm::ToolCall| {
                let this = self;
                let call = call.clone();
                let sid = sid.to_string();
                let branch = branch_id.to_string();
                let cancel = cancel_for_tools.clone();
                let visible = visible_reply.clone();
                Box::pin(async move {
                    // 工具开始执行时刻（中断补记 elapsed_ms 用；语义 = 开始到中断判定）。
                    let tool_started = std::time::Instant::now();
                    let guard = this.tool_guard_timeout(&call).await;
                    tokio::select! {
                        result = tokio::time::timeout(guard, this.run_tool(&sid, &branch, &call)) => {
                            match result {
                                Ok(tool_result) => {
                                    // 成功的 send_* 即该分支的可见回复：抑制
                                    // 并行的临时等待回复。
                                    let failed =
                                        tool_result.text.trim_start().starts_with("error:");
                                    // 「这一调用就是给用户的可见回复」由**工具自己
                                    // 声明**（`Tool::produces_visible_output`），
                                    // 不在循环里列平台工具名。
                                    if !failed
                                        && this.tools_produce_visible_output(&call.name).await
                                    {
                                        if let Some(visible) = &visible {
                                            let _ = visible.send(true);
                                        }
                                    }
                                    echo_loop::ToolOutcome::with_images(
                                        tool_result.text.clone(),
                                        tool_result.images.clone(),
                                    )
                                }
                                Err(_) => {
                                    let text = tool_timeout_notice(&call.name, guard.as_secs());
                                    this.record_interrupted_tool_result(
                                        &sid,
                                        &branch,
                                        &call,
                                        &text,
                                        true,
                                        tool_started.elapsed().as_millis() as u64,
                                    );
                                    echo_loop::ToolOutcome::text(text)
                                }
                            }
                        }
                        _ = cancel.cancelled() => {
                            this.record_interrupted_tool_result(
                                &sid,
                                &branch,
                                &call,
                                "error: tool execution cancelled",
                                false,
                                tool_started.elapsed().as_millis() as u64,
                            );
                            echo_loop::ToolOutcome::text("error: tool execution cancelled")
                        }
                    }
                })
            };

        // Subagent hook（echo-loop 的 hook 注入接口）：spawn_subagent 的
        // future 不 Send，无法走普通执行器——经 `execute_async` 通道直接进
        // 管线（模型可见的 schema 由注册表统一提供，不再经
        // extra_tool_definitions 追加）。spawn_subagent 本身是同步受理
        // （注册 + 后台拉起），结果立即可得——同步 body 内一次性算好文本，
        // 再包一个 'static ready future 返回（AsyncToolExecutor 的设计意图：
        // harness 编排需要 tokio::spawn 时经通道把结果带回，这里无需 spawn
        // 因此直接 ready）。
        let is_async_subagent = |name: &str| name == crate::subagent::SPAWN_SUBAGENT_TOOL;
        let execute_async_subagent = move |sid: &str,
                                           _branch: &str,
                                           call: &crate::llm::ToolCall|
              -> std::pin::Pin<
            Box<dyn std::future::Future<Output = echo_loop::ToolOutcome> + Send>,
        > {
            // 观测缺口修复（2026-10 审计）：spawn_subagent 经本异步通道直入
            // 管线、不经过 `run_tool`，此前调用与回执均不落事件日志（审计
            // 实测 7620 条 tool_call 里 0 条 spawn 记录——委派既无 spawn
            // 时间戳可算时延，也读不到 profile/node/timeout/task 实参）。
            // 现在与普通工具同口径：先记 ToolCall（参数脱敏），受理或拒绝后
            // 记 ToolResult（回执文本），保证日志成对、面板可见。
            let started = std::time::Instant::now();
            let started_at_ms = chrono::Utc::now().timestamp_millis();
            let logged_arguments = self.redact_text(&call.arguments);
            self.emit(BackendEvent::ToolCall {
                session_id: sid.to_string(),
                team_id: None,
                tool_name: call.name.clone(),
                arguments: logged_arguments.clone(),
                tool_call_id: call.id.clone(),
                branch_id: branch_id.to_string(),
                started_at_ms: Some(started_at_ms),
            });
            self.trunk
                .append_event(echo_session::SessionEvent::ToolCall(
                    echo_session::event::ToolCallEvent {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: logged_arguments,
                        session: Some(sid.to_string()),
                        started_at_ms: Some(started_at_ms),
                    },
                ));
            let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
            let text = if !self.allows_dynamic_tool(crate::subagent::SPAWN_SUBAGENT_TOOL) {
                format!(
                    "error: tool '{}' is not registered (subagent 插件未启用或不在本 persona 白名单)",
                    crate::subagent::SPAWN_SUBAGENT_TOOL
                )
            } else {
                match self.subagent_runtime() {
                    None => "error: subagent runtime not attached".to_string(),
                    Some((store, spawn)) => {
                        let tool = crate::subagent::SpawnSubagentTool::new(store, spawn);
                        let cancel = self
                            .active_inbound_turns
                            .get(branch_id)
                            .map(|turn| turn.cancel.clone())
                            .unwrap_or_default();
                        match tool.spawn(&args, sid, cancel, branch_id) {
                            Ok(receipt) => receipt,
                            Err(error) => format!("error: {error}"),
                        }
                    }
                }
            };
            let elapsed_ms = started.elapsed().as_millis() as u64;
            self.emit(BackendEvent::ToolResult {
                session_id: sid.to_string(),
                team_id: None,
                tool_name: call.name.clone(),
                result: text.clone(),
                tool_call_id: call.id.clone(),
                timed_out: false,
                branch_id: branch_id.to_string(),
                elapsed_ms: Some(elapsed_ms),
            });
            self.trunk
                .append_event(echo_session::SessionEvent::ToolResult(
                    echo_session::event::ToolResultEvent {
                        tool_call_id: call.id.clone(),
                        result: text.clone(),
                        images: vec![],
                        session: Some(sid.to_string()),
                        elapsed_ms: Some(elapsed_ms),
                    },
                ));
            Box::pin(async move { echo_loop::ToolOutcome::text(text) })
        };

        let result = {
            let subagent_hooks = echo_loop::SubagentToolHooks {
                is_async_tool: Some(&is_async_subagent),
                execute_async: Some(&execute_async_subagent),
                // schema 由注册表提供（见上）；hook 只负责异步执行分派。
                extra_tool_definitions: None,
            };
            let extras = echo_loop::runner::RunExtras {
                tools: Some(tools),
                on_reasoning: Some(&reasoning_cb),
                subagent_hooks,
                model: Some(model),
                chat: Some(chat_executor),
                turn_id: Some(branch_id.to_string()),
                // 每 turn 读配置：运行期调整 max_tool_iterations 下一 turn 生效。
                max_iterations: Some(self.config.read().await.max_tool_iterations.max(1)),
            };
            runner
                .run(
                    &session_id,
                    content.to_string(),
                    system_prompt,
                    history,
                    turn_cancel,
                    executor,
                    extras,
                )
                .await
        };

        match result {
            Ok(reply) => {
                // turn 完成：busy 收尾。
                self.emit(BackendEvent::AgentCompleted {
                    session_id: session_id.clone(),
                });
                tracing::info!(
                    session = %session_id,
                    branch_id = %branch_id,
                    message_sequence = message_sequence.unwrap_or_default(),
                    elapsed_ms = queued_at.elapsed().as_millis() as u64,
                    "agent turn completed"
                );
                Ok(reply)
            }
            Err(echo_loop::runner::LoopError::Cancelled) => Err(anyhow!(TURN_CANCELLED)),
            Err(other) => {
                // 模型错误（含 provider 返回的 402/余额不足）：发
                // AgentCompleted 收尾 busy 状态 + 把错误事件给
                // 前端（否则用户在面板上看到的是"转了圈后什么都没发生"，
                // 且持续挂着"正在思考"的活动浮条）。
                self.emit(BackendEvent::AgentCompleted {
                    session_id: session_id.clone(),
                });
                self.emit(BackendEvent::Error {
                    session_id: Some(session_id.clone()),
                    message: other.to_string(),
                });
                tracing::warn!(
                    session = %session_id,
                    branch_id = %branch_id,
                    %other,
                    elapsed_ms = queued_at.elapsed().as_millis() as u64,
                    "agent turn failed"
                );
                Err(anyhow!(other.to_string()))
            }
        }
    }
}
