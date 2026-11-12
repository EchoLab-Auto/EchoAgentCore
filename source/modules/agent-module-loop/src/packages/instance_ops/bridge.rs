//! 平台实例 → Agent 的**入站入口**（`AgentMessageHook`）。
//!
//! 这里把一条平台消息变成一次 agent turn：会话键构造（多实例维度）、单/并行
//! 模式的展示会话选择、`SessionUpdated` / `MessageReceived` 事件、turn 启动、
//! 内容脱敏。**这是核心入站编排，不是 adapter 能力**——adapter 能力（平台
//! 协议、收发消息）在 `echo-module-adapter` 与独立仓库的 `agent-module-adapter-qq`
//! 里，早已出进程。
//!
//! 它**不是** trait 实现：实例跑在进程外，接线归组合根（模块 `submit_input`
//! → 有界队列 → 每实例 worker → 本方法）。平台输出只经显式工具可用。
//!
//! 2026-11 全模块化 S3 审计：本文件曾与 `commands.rs`（实例命令域，429 行）
//! 同处一个目录，让"桥接"看起来像 adapter 能力的一部分。命令域已搬进
//! `agent-module-tools`，本文件留在宿主是**刻意的**（理由见上）。
//!
//! ## 2026-11 二次审计：留下来的与下沉出去的
//!
//! 本文件原有两段**纯函数**，与"核心编排"无关，已下沉定义层：
//!
//! | 原位置 | 现位置 | 为什么下沉 |
//! |---|---|---|
//! | `format_hook_input` | [`echo_defs::input_marker::format_instance_hook`] | 它是 hook 信封的**构造方**，而解析方（`structured_message_sequence` / `hook_payload`）一直在定义层——两侧靠字符串字面量对齐 `"message_sequence"`，改字段名不会有编译错误，只会静默丢序号 |
//! | `recall_command_scope` | [`echo_defs::control_command::cancel_scope`] | 判据是"用户打了什么字"，与平台、与 agent 运行态都无关；各入口各写一份就会出现"取消在 A 平台生效、B 平台不生效" |
//!
//! 留下来的部分（会话键构造、展示会话选择、事件发射、turn 准入、取消执行、
//! 脱敏）**每一项都要 `Arc<Agent>`**——它们的共同点是「改变 agent 的状态」，
//! 这正是核心编排，搬不走。

use std::sync::Arc;

use echo_defs::chat::IncomingMessage;

use crate::agent::Agent;
use crate::event::BackendEvent;
use crate::session::SessionKey;

/// Wraps an `Arc<Agent>` behind the instance-facing inbound hook.
pub struct AgentMessageHook {
    agent: Arc<Agent>,
}

impl AgentMessageHook {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self { agent }
    }
}

impl AgentMessageHook {
    /// Deliver one inbound platform message into this agent (session lookup,
    /// event emission, turn start). A successful return only acknowledges
    /// processing; it never carries text for the instance to send back.
    pub async fn on_incoming_message(&self, msg: IncomingMessage) -> Result<(), String> {
        // 懒加载（2026-11）：平台入站消息是"真正要用这份历史"的第一个信号
        // ——先读盘，再建会话/turn。
        self.agent.trunk.ensure_loaded().await;
        let agent = &self.agent;
        // 多实例维度：模块实例名即 QQ 实例名（非 "qq" 时进会话键）。
        // 实例名 == 平台名 = 默认实例，不写 account 维度（多实例才需要区分）。
        let account = (!msg.instance_name.eq_ignore_ascii_case(&msg.platform))
            .then(|| msg.instance_name.clone());
        let key = SessionKey {
            platform: msg.platform.clone(),
            scope: if msg.channel.is_group() {
                "group".into()
            } else {
                "dm".into()
            },
            scope_id: msg.channel.group_id().unwrap_or("").to_string(),
            user_id: msg.user_id.clone(),
            account,
        };
        let session =
            agent
                .trunk
                .get_or_create(&key, msg.user_name.clone(), msg.group_name.clone());
        let session_id = session.id.clone();
        let received_at_ms = chrono::Utc::now().timestamp_millis();
        let message_sequence = agent.next_message_sequence();

        // turn 与展示都在默认本地会话进行——展示层事件
        // （SessionUpdated/MessageReceived）也要落在默认会话 id 上，否则
        // 实时消息显示在 QQ 会话、turn 却跑在默认会话（两边都看不到完整
        // 对话）。消息元数据保留 QQ 来源（平台/发送者/群），Panel 的来源
        // 标签照常显示。
        let display_session = agent.trunk.get_or_create(
            &crate::session::SessionKey::local_tui(),
            "local user".into(),
            None,
        );
        let display_session_id = display_session.id.clone();

        tracing::info!(
            message_sequence,
            instance = %msg.instance_name,
            platform = %msg.platform,
            user = %msg.user_id,
            channel = %msg.channel,
            session = %session_id,
            source_timestamp = msg.timestamp,
            received_at_ms,
            receive_lag_ms = received_at_ms.saturating_sub(msg.timestamp.saturating_mul(1000)),
            "inbound instance message received"
        );

        agent.emit(BackendEvent::SessionUpdated {
            session: display_session.info(String::new(), &agent.node_identity()),
        });
        agent.emit(BackendEvent::MessageReceived {
            session_id: display_session_id.clone(),
            instance_name: msg.instance_name.clone(),
            platform: msg.platform.clone(),
            user_id: msg.user_id.clone(),
            user_name: msg.user_name.clone(),
            channel: format!("{}", msg.channel),
            group_name: msg.group_name.clone(),
            // 面板展示同过脱敏（2026-10 安全）：与入站漏斗同口径——
            // 群成员粘贴的密钥不得经展示事件露出（无脱敏器 = 原样）。
            content: agent.redact_text(&msg.content),
            images: msg.images.clone(),
            timestamp: msg.timestamp,
            received_at_ms,
            message_sequence,
            team_id: None,
        });

        let hook_input =
            echo_defs::input_marker::format_instance_hook(&msg, message_sequence, received_at_ms);

        // Cancellation is a transport-level control command. Handle it before
        // reserving a conversation turn so it can interrupt work immediately.
        if let Some(cancel_all) = echo_defs::control_command::cancel_scope(&msg.content) {
            agent
                .record_incoming_and_snapshot(&session, &hook_input)
                .await;
            // 取消必须按 **turn 实际注册的会话** 过滤，而不是平台会话：
            // turn 跑在默认本地会话（display_session），平台会话只承载来源。
            // 此前这里传平台 session_id，单会话模式下永远取消不到——并行模式
            // 删掉后才暴露出来（那时 display_session 就是平台会话，恰好能对上）。
            let cancelled = agent
                .cancel_requested_work(&display_session_id, cancel_all)
                .await;
            tracing::info!(
                session = %display_session_id,
                message_sequence,
                cancel_all,
                cancelled,
                "platform cancellation command handled"
            );
            let reply = if cancelled > 0 {
                if cancel_all && cancelled > 1 {
                    format!("已停止 {cancelled} 个进行中的任务。")
                } else {
                    "已停止当前任务。".into()
                }
            } else {
                "当前没有可停止的任务。".into()
            };
            // 取消回执只记入后端会话（平台侧回执已随模块实例概念移除）。
            agent
                .record_control_reply(&session, reply, message_sequence)
                .await;
            return Ok(());
        }

        // 入站消息投递到该 persona 的**默认本地会话**（local:tui::local_user）
        // ——hook 输入带完整平台/发送者元数据，由 agent 自行判断是否回复
        // （qq-transport 技能约束回复必须走发送工具）。平台会话本身仍被创建
        // （上面 `get_or_create`），用于聚类来源与审计，但不承载 turn。
        Arc::clone(agent)
            .process_inbound_branch(&display_session, &hook_input, message_sequence)
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{
        ChatChunk, ChatRequest, ChatResponse, LlmError, LlmProvider, ToolCall, Usage,
    };
    use crate::tool::{Tool, ToolError};
    use async_trait::async_trait;
    use echo_defs::chat::ChannelType;
    use echo_defs::chat::IncomingMessage;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct BridgeProvider {
        calls: AtomicUsize,
        reply: String,
    }

    #[async_trait]
    impl LlmProvider for BridgeProvider {
        fn name(&self) -> &str {
            "bridge-test"
        }

        fn default_model(&self) -> &str {
            "bridge-test"
        }

        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                let is_group = request
                    .messages
                    .iter()
                    .any(|message| message.content.contains(r#""type": "group""#));
                let (name, arguments) = if is_group {
                    ("send_group_msg", r#"{"group_id":999,"content":"ok"}"#)
                } else {
                    ("send_private_msg", r#"{"user_id":123456,"content":"ok"}"#)
                };
                return Ok(ChatResponse {
                    stop_reason: None,
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: "send".into(),
                        name: name.into(),
                        arguments: arguments.into(),
                    }],
                    usage: Usage::default(),
                });
            }
            Ok(ChatResponse {
                stop_reason: None,
                content: Some(self.reply.clone()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: Usage::default(),
            })
        }

        async fn chat_stream(
            &self,
            request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<ChatResponse, LlmError> {
            // 测试桩：流式与非流式同效（主对话默认经此路径；增量断言由专门用例覆盖）。
            self.chat(request).await
        }
    }

    struct SendTool(&'static str);

    #[async_trait]
    impl Tool for SendTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "test QQ send"
        }

        fn produces_visible_output(&self) -> bool {
            // 测试替身和真实模块实例工具一样声明「这一调用就是可见回复」。
            true
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, _arguments: serde_json::Value) -> Result<String, ToolError> {
            Ok("message sent".into())
        }
    }

    struct BlockingProvider {
        entered: Arc<AtomicUsize>,
        seen_sequences: Arc<std::sync::Mutex<Vec<Vec<u64>>>>,
    }

    #[async_trait]
    impl LlmProvider for BlockingProvider {
        fn name(&self) -> &str {
            "blocking-test"
        }

        fn default_model(&self) -> &str {
            "blocking-test"
        }

        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            let sequences = request
                .messages
                .iter()
                .filter_map(|message| crate::agent::structured_message_sequence(&message.content))
                .collect::<Vec<_>>();
            self.seen_sequences
                .lock()
                .expect("seen sequences poisoned")
                .push(sequences);
            self.entered.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }

        async fn chat_stream(
            &self,
            request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<ChatResponse, LlmError> {
            // 测试桩：流式与非流式同效（主对话默认经此路径；增量断言由专门用例覆盖）。
            self.chat(request).await
        }
    }

    struct CountingSendTool {
        name: &'static str,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CountingSendTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "count test sends"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, _arguments: serde_json::Value) -> Result<String, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("message sent".into())
        }
    }

    fn mock_agent(reply: &str) -> Arc<Agent> {
        let provider = Arc::new(BridgeProvider {
            calls: AtomicUsize::new(0),
            reply: reply.to_string(),
        });
        let mut tools = crate::tool::ToolRegistry::new();
        tools.register(Arc::new(SendTool("send_private_msg")));
        tools.register(Arc::new(SendTool("send_group_msg")));
        // 控制回复走 `ModuleInstance::send_message`（平台中立的 typed seam），
        // 因此测试也需要一个同 platform 的模块实例。
        let instances = echo_defs::module_instance::InstanceRegistry::new();
        instances.register(Arc::new(
            crate::packages::instance_ops::test_support::MockInstance::configured("qq", "qq", true),
        ));
        Arc::new(Agent::new(
            provider,
            crate::config::AgentConfig::default(),
            crate::skill::SkillRegistry::new(),
            tools,
            Arc::new(instances),
        ))
    }

    fn dm_message(content: &str) -> IncomingMessage {
        IncomingMessage {
            instance_name: "qq".into(),
            platform: "qq".into(),
            user_id: "123456".into(),
            user_name: "tester".into(),
            channel: ChannelType::Direct,
            group_name: None,
            content: content.into(),
            timestamp: 1700000000,
            at_me: false,
            metadata: serde_json::Value::Null,
            images: vec![],
            files: vec![],
        }
    }

    /// 显式声明循环模块（贴近真实人格配置；单会话本就是默认）。
    async fn use_single_loop(agent: &Arc<Agent>) {
        agent
            .apply_capabilities(&crate::config::TeamMember {
                enabled_modules: vec![crate::modules::SINGLE_LOOP_MODULE_ID.into()],
                ..Default::default()
            })
            .await;
    }

    /// 记录 `ModuleInstance::send_message` 的控制回复（取消回执 / 等待回复走
    /// 平台中立的 typed seam，不再经过 `send_*` 工具）。
    async fn wait_until(mut predicate: impl FnMut() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !predicate() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition should become true");
    }

    #[tokio::test]
    async fn routes_message_through_one_way_hook() {
        let hook = AgentMessageHook::new(mock_agent("后台完成"));
        use_single_loop(&hook.agent).await;
        let result = hook.on_incoming_message(dm_message("hello")).await;
        assert_eq!(result, Ok(()));
        // turn 跑在默认本地会话（每会话一条时间线）；平台会话另行创建，
        // 只承载"来源"（面板按它聚类，不承载 turn）。
        let local_key = SessionKey::local_tui();
        wait_until(|| {
            hook.agent
                .trunk
                .all()
                .iter()
                .find(|s| s.session_key == local_key)
                .and_then(|session| session.history.try_lock())
                .is_some_and(|history| history.len() >= 4)
        })
        .await;

        // 平台会话保留 QQ 作用域（来源可追溯）。
        let sessions = hook.agent.trunk.all();
        let platform_session = sessions
            .iter()
            .find(|s| s.session_key.platform == "qq")
            .expect("平台会话应当被创建（来源聚类）");
        assert_eq!(platform_session.session_key.scope, "dm");
        assert_eq!(platform_session.session_key.user_id, "123456");

        let session = sessions
            .iter()
            .find(|s| s.session_key == local_key)
            .expect("默认本地会话承载 turn");
        let history = session.history.lock();
        assert!(history[0].content.starts_with("<qq_message_hook>"));
        assert!(history[0].content.contains("\"event\": \"qq_message\""));
        assert!(history[0].content.contains("\"user_id\": \"123456\""));
        assert!(history[0].content.contains("\"content\": \"hello\""));
        // user + synthesized assistant tool_use + tool result + final reply.
        assert_eq!(history[1].role, crate::llm::ChatRole::Assistant);
        assert_eq!(
            history[1].tool_calls.as_ref().unwrap()[0].name,
            "send_private_msg"
        );
        assert_eq!(history[2].role, crate::llm::ChatRole::Tool);
        assert_eq!(history[3].content, "后台完成");
    }

    #[tokio::test]
    async fn group_message_uses_group_scope() {
        let hook = AgentMessageHook::new(mock_agent("ok"));
        use_single_loop(&hook.agent).await;
        let mut msg = dm_message("hello");
        msg.channel = ChannelType::Group {
            group_id: "999".into(),
        };
        let _ = hook.on_incoming_message(msg).await;
        let local_key = SessionKey::local_tui();
        // 入站在任务内注册（不阻塞 QQ 入站），因此等该条用户事件落到默认会话。
        wait_until(|| {
            hook.agent
                .trunk
                .all()
                .iter()
                .find(|s| s.session_key == local_key)
                .and_then(|session| session.history.try_lock())
                .is_some_and(|history| !history.is_empty())
        })
        .await;

        // 群作用域体现在平台会话上（来源）；turn 的历史在默认会话里，
        // hook 输入里带着群的完整元数据。
        let sessions = hook.agent.trunk.all();
        let platform_session = sessions
            .iter()
            .find(|s| s.session_key.platform == "qq")
            .expect("平台会话应当被创建（来源聚类）");
        assert_eq!(platform_session.session_key.scope, "group");
        assert_eq!(platform_session.session_key.scope_id, "999");

        let local = sessions
            .iter()
            .find(|s| s.session_key == local_key)
            .expect("默认本地会话承载 turn");
        let history = local.history.lock();
        assert!(history[0].content.contains("\"type\": \"group\""));
        assert!(history[0].content.contains("\"group_id\": \"999\""));
    }

    /// 单会话模式（默认）：QQ 入站消息投递到默认本地会话（hook 输入带
    /// 完整平台/发送者元数据），不开临时回复分支、不发 QQ 临时回复；
    /// 是否回复由 agent 自行判断（回复经发送工具）。
    #[tokio::test]
    async fn single_mode_delivers_to_default_session_without_branch() {
        let hook = AgentMessageHook::new(mock_agent("后台完成"));

        let (bridge, handle) = crate::create_bridge();
        hook.agent.attach(std::sync::Arc::new(handle));
        let result = hook.on_incoming_message(dm_message("hello")).await;
        assert_eq!(result, Ok(()));
        wait_until(|| {
            hook.agent.trunk.all().iter().any(|s| {
                s.history
                    .try_lock()
                    .is_some_and(|h| h.iter().any(|m| m.content.contains("hello")))
            })
        })
        .await;

        // 消息落在默认本地会话，QQ 会话只注册为身份（不开分支、不跑 turn）。
        let sessions = hook.agent.trunk.all();
        let default = sessions
            .iter()
            .find(|s| s.id == "local:tui::local_user")
            .expect("default local session");
        {
            let history = default.history.lock();
            assert!(history[0].content.starts_with("<qq_message_hook>"));
            assert!(history[0].content.contains("\"user_id\": \"123456\""));
        }

        // 会话键仍按 QQ 作用域注册（供 Panel 会话列表展示）。
        assert!(sessions
            .iter()
            .any(|s| s.session_key.platform == "qq" && s.session_key.user_id == "123456"));

        // 展示事件（SessionUpdated/MessageReceived）落在默认会话 id 上——
        // 实时消息才会显示在默认会话（与 turn 所在的会话一致）。
        let mut received = Vec::new();
        while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
            received.push(event);
        }
        let message = received
            .iter()
            .find_map(|e| match e {
                crate::event::BackendEvent::MessageReceived {
                    session_id,
                    content,
                    ..
                } => Some((session_id.clone(), content.clone())),
                _ => None,
            })
            .expect("MessageReceived emitted");
        assert_eq!(message.0, "local:tui::local_user");
        assert_eq!(message.1, "hello");
    }

    #[test]
    fn hook_input_includes_downloaded_files() {
        let mut msg = dm_message("看这个文件");
        msg.files = vec![
            echo_defs::IncomingFile {
                name: "a.zip".into(),
                path: Some("/tmp/qq/a.zip".into()),
                size: 10,
                error: None,
            },
            echo_defs::IncomingFile {
                name: "b.zip".into(),
                path: None,
                size: 20,
                error: Some("获取下载链接失败：boom".into()),
            },
        ];
        let text = echo_defs::input_marker::format_instance_hook(&msg, 7, 123);
        let payload = crate::input_marker::hook_payload(&text).expect("hook payload");
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["message_sequence"], 7);
        assert_eq!(value["files"][0]["name"], "a.zip");
        assert_eq!(value["files"][0]["path"], "/tmp/qq/a.zip");
        assert_eq!(value["files"][0]["size"], 10);
        assert_eq!(value["files"][0]["error"], serde_json::Value::Null);
        assert_eq!(value["files"][1]["path"], serde_json::Value::Null);
        assert_eq!(value["files"][1]["error"], "获取下载链接失败：boom");
    }

    #[test]
    fn hook_input_without_files_has_empty_list() {
        let msg = dm_message("hi");
        let text = echo_defs::input_marker::format_instance_hook(&msg, 1, 1);
        let payload = crate::input_marker::hook_payload(&text).expect("hook payload");
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["files"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn successful_qq_delivery_marks_the_branch_as_replied() {
        let agent = mock_agent("done");
        let msg = dm_message("hello");
        let session = agent.trunk.get_or_create(
            &SessionKey::parse("qq:dm::123456").unwrap(),
            "tester".into(),
            None,
        );
        let hook_input = echo_defs::input_marker::format_instance_hook(
            &msg,
            1,
            chrono::Utc::now().timestamp_millis(),
        );
        let snapshot = agent
            .record_incoming_and_snapshot(&session, &hook_input)
            .await;
        let (reply_tx, reply_rx) = tokio::sync::watch::channel(false);

        agent
            .process_recorded_message_with_progress(
                &session,
                &hook_input,
                snapshot,
                tokio_util::sync::CancellationToken::new(),
                Some(reply_tx),
                "test-branch",
            )
            .await
            .unwrap();

        assert!(*reply_rx.borrow());
    }

    /// 同一会话两条入站 + 一条取消命令：第一条在跑、第二条在闸门上排队，
    /// 两者都是"进行中"，取消全部把它们一起停掉。
    ///
    /// （并行模式下这两条曾是并发分支；删掉并行后它们变成串行排队，但"取消
    /// 全部"要停的仍然是两条——排队中的那条同样可被取消，这是排队必须先注册
    /// 的原因。）
    #[tokio::test]
    async fn queued_and_running_turns_are_both_cancelled_by_recall() {
        let entered = Arc::new(AtomicUsize::new(0));
        let sends = Arc::new(AtomicUsize::new(0));
        let seen_sequences = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(BlockingProvider {
            entered: Arc::clone(&entered),
            seen_sequences: Arc::clone(&seen_sequences),
        });
        let mut tools = crate::tool::ToolRegistry::new();
        for name in ["send_private_msg", "send_group_msg"] {
            tools.register(Arc::new(CountingSendTool {
                name,
                calls: Arc::clone(&sends),
            }));
        }
        let agent = Arc::new(Agent::new(
            provider,
            crate::config::AgentConfig::default(),
            crate::skill::SkillRegistry::new(),
            tools,
            Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
        ));
        use_single_loop(&agent).await;
        let hook = AgentMessageHook::new(Arc::clone(&agent));

        hook.on_incoming_message(dm_message("第一句话"))
            .await
            .unwrap();
        wait_until(|| entered.load(Ordering::SeqCst) >= 1).await;
        hook.on_incoming_message(dm_message("第二句话"))
            .await
            .unwrap();
        // 串行闸门：第二条在排队，尚未进入模型——但已注册为"进行中"。
        // 注册（`register_inbound_turn`）发生在闸门外、append 之前，所以等注册表
        // 计数即可；落盘要等拿到闸，排队中的 turn 反而**不会**落盘用户事件。
        wait_until(|| agent.active_inbound_turn_count() == 2).await;
        assert_eq!(entered.load(Ordering::SeqCst), 1);
        let snapshots = seen_sequences
            .lock()
            .expect("seen sequences poisoned")
            .clone();
        assert_eq!(snapshots, vec![vec![1]]);

        hook.on_incoming_message(dm_message("取消全部任务"))
            .await
            .unwrap();
        wait_until(|| agent.active_inbound_turn_count() == 0).await;
        // 取消回执不再由框架代发（2026-10 选项 B）：不经过任何发送工具。
        assert_eq!(sends.load(Ordering::SeqCst), 0);

        // 取消命令自身走 `record_control_reply`（记控制回执，不是用户 turn）。
        // 落盘的用户序列存在合法竞态：第二条（seq=2）可能在闸门外就被取消
        // （不落盘），也可能在第一条让出闸后拿到闸、落盘再被取消（序列含 2）。
        // 两种结果都正确，所以这里只断言**第一条一定在**、且序列里**没有**第三条
        // 用户 turn（取消命令不是用户 turn）。
        let session = agent
            .trunk
            .all()
            .into_iter()
            .find(|s| s.session_key == SessionKey::local_tui())
            .expect("默认会话承载 turn");
        let history = session.history.lock();
        let user_sequences = history
            .iter()
            .filter_map(|message| crate::agent::structured_message_sequence(&message.content))
            .collect::<Vec<_>>();
        assert!(
            user_sequences.contains(&1),
            "第一条用户消息必须在: {user_sequences:?}"
        );
        assert!(
            !user_sequences.contains(&3),
            "取消命令不是用户 turn: {user_sequences:?}"
        );
        // 取消回执记在**平台会话**（控制回执走 `record_control_reply(&session, …)`，
        // 与 turn 所在的默认会话不是同一个）——两条路径分属两个会话，别混着查。
        let platform_session = agent
            .trunk
            .all()
            .into_iter()
            .find(|s| s.session_key.platform == "qq")
            .expect("平台会话存在");
        let platform_history = platform_session.history.lock();
        assert!(platform_history
            .iter()
            .any(|message| message.content == "已停止 2 个进行中的任务。"));
    }
}
