//! End-to-end integration tests: a real Agent driven through the bridge.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use echo_adapter::AdapterRegistry;
use echo_agent::llm::{
    ChatChunk, ChatRequest, ChatResponse, LlmError, LlmProvider, ToolCall, Usage,
};
use echo_agent::tool::{Tool, ToolError};
use echo_agent::{
    Agent, AgentConfig, BackendCommand, BackendEvent, SessionKey, SkillRegistry, ToolRegistry,
};
use std::collections::VecDeque;

/// A static-reply provider that counts calls and records the last system prompt.
struct StaticProvider {
    reply: &'static str,
    calls: AtomicUsize,
    last_system_prompt: tokio::sync::Mutex<Option<String>>,
}

#[async_trait]
impl LlmProvider for StaticProvider {
    fn name(&self) -> &str {
        "static"
    }
    fn default_model(&self) -> &str {
        "static-model"
    }
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let system = &request.messages[0];
        assert_eq!(system.role, echo_agent::llm::ChatRole::System);
        *self.last_system_prompt.lock().await = Some(system.content.clone());
        Ok(ChatResponse {
            stop_reason: None,
            content: Some(self.reply.into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        })
    }
    async fn chat_stream(
        &self,
        _request: &ChatRequest,
        _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<(), LlmError> {
        Ok(())
    }
}

impl StaticProvider {
    async fn last_system_prompt(&self) -> String {
        self.last_system_prompt
            .lock()
            .await
            .clone()
            .unwrap_or_default()
    }
}

/// A provider that replays a fixed script of responses, recording the last
/// system prompt seen. Used to exercise the delivery-correction loop.
struct ScriptedProvider {
    script: tokio::sync::Mutex<VecDeque<ChatResponse>>,
    calls: AtomicUsize,
    last_system_prompt: tokio::sync::Mutex<Option<String>>,
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }
    fn default_model(&self) -> &str {
        "scripted-model"
    }
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let system = &request.messages[0];
        assert_eq!(system.role, echo_agent::llm::ChatRole::System);
        *self.last_system_prompt.lock().await = Some(system.content.clone());
        self.script
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| LlmError::Config("script exhausted".into()))
    }
    async fn chat_stream(
        &self,
        _request: &ChatRequest,
        _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<(), LlmError> {
        Ok(())
    }
}

impl ScriptedProvider {
    fn new(script: Vec<ChatResponse>) -> Self {
        Self {
            script: tokio::sync::Mutex::new(script.into()),
            calls: AtomicUsize::new(0),
            last_system_prompt: tokio::sync::Mutex::new(None),
        }
    }
    async fn last_system_prompt(&self) -> String {
        self.last_system_prompt
            .lock()
            .await
            .clone()
            .unwrap_or_default()
    }
}

/// A tool that returns a canned result.
struct MockTool {
    name: &'static str,
    result: String,
}

#[async_trait]
impl Tool for MockTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "mock tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(&self, _arguments: serde_json::Value) -> Result<String, ToolError> {
        Ok(self.result.clone())
    }
}

fn test_agent(reply: &'static str) -> (Arc<Agent>, Arc<StaticProvider>) {
    let provider = Arc::new(StaticProvider {
        reply,
        calls: AtomicUsize::new(0),
        last_system_prompt: tokio::sync::Mutex::new(None),
    });
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    ));
    // 去主智能体：会话类命令必须带 team_id，测试 agent 统一用 "t"。
    agent.set_team_id(Some("t".into()));
    (agent, provider)
}

#[tokio::test]
async fn send_message_command_produces_agent_output_event() {
    let (agent, provider) = test_agent("你好，世界");
    let (bridge, handle) = echo_agent::create_bridge();
    agent.attach(Arc::new(handle));

    agent
        .apply_command(BackendCommand::SendMessage {
            session_id: "qq:dm::123".into(),
            content: "hello".into(),
            images: vec![],
            team_id: Some("t".into()),
        })
        .await;

    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    // Events flow back through the bridge.
    let mut events = Vec::new();
    {
        let mut rx = bridge.event_rx.lock().await;
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
    }
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            BackendEvent::AgentOutput { content, .. } if content == "你好，世界"
        )),
        "AgentOutput event expected, got {events:?}"
    );
}

#[tokio::test]
async fn send_message_creates_session() {
    let (agent, _provider) = test_agent("ok");
    agent
        .apply_command(BackendCommand::SendMessage {
            session_id: "qq:group:999:456".into(),
            content: "hi".into(),
            images: vec![],
            team_id: Some("t".into()),
        })
        .await;

    let sessions = agent.trunk.all();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_key.platform, "qq");
    assert_eq!(sessions[0].session_key.scope, "group");
    assert_eq!(sessions[0].session_key.scope_id, "999");
    assert_eq!(sessions[0].session_key.user_id, "456");
}

#[tokio::test]
async fn set_system_prompt_changes_behaviour() {
    let (agent, provider) = test_agent("ok");
    agent
        .apply_command(BackendCommand::SetSystemPrompt {
            prompt: "你是翻译助手".into(),
        })
        .await;
    // The next message must carry the new prompt to the LLM.
    agent
        .apply_command(BackendCommand::SendMessage {
            session_id: "qq:dm::123".into(),
            content: "translate".into(),
            images: vec![],
            team_id: Some("t".into()),
        })
        .await;
    let prompt = provider.last_system_prompt().await;
    assert!(prompt.contains("你是翻译助手"), "prompt: {prompt}");
}

#[tokio::test]
async fn set_system_prompt_persists_to_config() {
    let (agent, _provider) = test_agent("ok");
    let tmp = std::env::temp_dir().join(format!("echo-agent-prompt-{}.toml", uuid::Uuid::new_v4()));
    std::fs::write(
        &tmp,
        "[agent]\nprovider = \"openai\"\nsystem_prompt = \"old\"\n",
    )
    .unwrap();
    agent.set_config_path(tmp.clone());
    agent
        .apply_command(BackendCommand::SetSystemPrompt {
            prompt: "你是翻译助手".into(),
        })
        .await;
    let content = std::fs::read_to_string(&tmp).unwrap();
    // 系统提示词由 [plugins.system_prompt] 插件段拥有（Core 插件注入后端 API
    // 配置无关），持久化目标是 plugins 段而非 [agent] 段。
    assert!(
        content.contains("[plugins.system_prompt]"),
        "plugins section missing: {content}"
    );
    assert!(
        content.contains("text = \"你是翻译助手\""),
        "prompt not persisted into plugins: {content}"
    );
    // 运行期以 plugins 段为准（Core 启动时用 plugins 覆盖 agent 段）。
    std::fs::remove_file(&tmp).ok();
}

#[tokio::test]
async fn switch_model_updates_active_model() {
    let (agent, _provider) = test_agent("ok");
    agent
        .apply_command(BackendCommand::SwitchModel {
            model: "gpt-4o".into(),
        })
        .await;
    assert_eq!(agent.active_model().await, "gpt-4o");
}

#[tokio::test]
async fn switch_model_persists_to_config() {
    let (agent, _provider) = test_agent("ok");
    let tmp = std::env::temp_dir().join(format!("echo-agent-model-{}.toml", uuid::Uuid::new_v4()));
    std::fs::write(
        &tmp,
        "[agent]\nprovider = \"openai\"\nmodel = \"old-model\"\n",
    )
    .unwrap();
    agent.set_config_path(tmp.clone());
    agent
        .apply_command(BackendCommand::SwitchModel {
            model: "gpt-4o".into(),
        })
        .await;
    let content = std::fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("model = \"gpt-4o\""),
        "unexpected: {content}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[tokio::test]
async fn switch_api_activates_profile() {
    let (agent, _provider) = test_agent("ok");
    // Seed a profile via UpdateApiConfig.
    agent
        .apply_command(BackendCommand::UpdateApiConfig {
            name: "deepseek".into(),
            provider: "deepseek".into(),
            model: "ds-v4".into(),
            base_url: "http://x".into(),
            api_key: String::new(),
            thinking: None,
            reasoning_effort: None,
        })
        .await;
    agent
        .apply_command(BackendCommand::SwitchApi {
            name: "deepseek".into(),
        })
        .await;
    let cfg = agent.api_config().await;
    assert_eq!(cfg.active_api, "deepseek");
    assert_eq!(cfg.api_profiles.len(), 1);
}

#[tokio::test]
async fn delete_api_removes_profile() {
    let (agent, _provider) = test_agent("ok");
    agent
        .apply_command(BackendCommand::UpdateApiConfig {
            name: "temp".into(),
            provider: "openai".into(),
            model: "m".into(),
            base_url: String::new(),
            api_key: String::new(),
            thinking: None,
            reasoning_effort: None,
        })
        .await;
    assert_eq!(agent.api_config().await.api_profiles.len(), 1);
    agent
        .apply_command(BackendCommand::DeleteApi {
            name: "temp".into(),
        })
        .await;
    assert!(
        agent.api_config().await.api_profiles.is_empty(),
        "profile removed"
    );
}

/// A provider that delays before replying, to expose branch concurrency.
struct SlowProvider {
    delay_ms: u64,
    replies: tokio::sync::Mutex<Vec<&'static str>>,
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for SlowProvider {
    fn name(&self) -> &str {
        "slow"
    }
    fn default_model(&self) -> &str {
        "slow-model"
    }
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let reply = self.replies.lock().await.pop().unwrap_or("late");
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        let _ = request;
        Ok(ChatResponse {
            stop_reason: None,
            content: Some(reply.into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        })
    }
    async fn chat_stream(
        &self,
        _request: &ChatRequest,
        _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<(), LlmError> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_messages_merge_by_request_sequence() {
    let provider = Arc::new(SlowProvider {
        delay_ms: 50,
        replies: tokio::sync::Mutex::new(vec!["second", "first"]),
        calls: AtomicUsize::new(0),
    });
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(AdapterRegistry::new()),
    ));
    let key = SessionKey::parse("qq:dm::1").unwrap();
    let session = agent.trunk.get_or_create(&key, "u".into(), None);

    // Start the first branch, then ensure the second enters while it is still
    // waiting. Structured inputs carry the production ordering anchor.
    let a1 = agent.clone();
    let s1 = session.clone();
    let t1 = tokio::spawn(async move {
        a1.process_message(
            &s1,
            r#"<backend_message_hook>{"message_sequence":1,"content":"msg1"}</backend_message_hook>"#,
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while provider.calls.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first branch should enter the provider");
    let a2 = agent.clone();
    let s2 = session.clone();
    let t2 = tokio::spawn(async move {
        a2.process_message(
            &s2,
            r#"<backend_message_hook>{"message_sequence":2,"content":"msg2"}</backend_message_hook>"#,
        )
        .await
        .unwrap()
    });

    let (r1, r2) = tokio::join!(t1, t2);
    // Both requests are committed before either branch completes, then each
    // reply is merged directly after its own request.
    let hist = session.history.lock().await.clone();
    let roles: Vec<&str> = hist
        .iter()
        .map(|m| match m.role {
            echo_agent::llm::ChatRole::User => "user",
            echo_agent::llm::ChatRole::Assistant => "assistant",
            _ => "other",
        })
        .collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "user", "assistant"],
        "branch results must merge by request sequence: {roles:?}"
    );
    let _ = (r1, r2);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn plugin_list_and_toggle_roundtrip() {
    use echo_plugin::{BuiltinPlugin, PluginKind, PluginManifest};
    let (agent, _provider) = test_agent("ok");
    // 注册一个内置插件并挂载。
    agent
        .plugin_host()
        .register_and_mount(std::sync::Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin(
                "test.plugin.one",
                "测试插件",
                "0.1",
                PluginKind::Tool,
                "t",
                "d",
            ),
            |_| Ok(vec![]),
        )))
        .unwrap();
    assert_eq!(agent.plugin_host().descriptors().len(), 1);

    // 拉取列表。
    agent
        .apply_command(BackendCommand::RequestPluginsList)
        .await;
    // 禁用 → descriptor enabled=false；再启用恢复。
    agent
        .apply_command(BackendCommand::TogglePlugin {
            id: "test.plugin.one".into(),
            enabled: false,
        })
        .await;
    assert!(!agent.plugin_host().descriptors()[0].enabled);
    agent
        .apply_command(BackendCommand::TogglePlugin {
            id: "test.plugin.one".into(),
            enabled: true,
        })
        .await;
    assert!(agent.plugin_host().descriptors()[0].enabled);
    // 未知插件报错（通过 Error 事件，不 panic）。
    agent
        .apply_command(BackendCommand::TogglePlugin {
            id: "nope.nope".into(),
            enabled: false,
        })
        .await;
}

#[tokio::test]
async fn toggle_management_panel_plugin_disable_is_refused() {
    use echo_plugin::{BuiltinPlugin, PluginKind, PluginManifest};
    let (agent, _provider) = test_agent("ok");
    agent
        .plugin_host()
        .register_and_mount(std::sync::Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin(
                echo_agent::plugins::MANAGEMENT_PANEL_PLUGIN_ID,
                "管理面",
                "0.1",
                PluginKind::Management,
                "m",
                "d",
            ),
            |_| Ok(vec![]),
        )))
        .unwrap();
    // 防自锁：经 TogglePlugin 禁用管理面插件被拒绝，插件保持启用。
    agent
        .apply_command(BackendCommand::TogglePlugin {
            id: echo_agent::plugins::MANAGEMENT_PANEL_PLUGIN_ID.into(),
            enabled: false,
        })
        .await;
    assert!(agent.plugin_host().descriptors()[0].enabled);
    // 启用路径不受影响（幂等）。
    agent
        .apply_command(BackendCommand::TogglePlugin {
            id: echo_agent::plugins::MANAGEMENT_PANEL_PLUGIN_ID.into(),
            enabled: true,
        })
        .await;
    assert!(agent.plugin_host().descriptors()[0].enabled);
}

#[tokio::test]
async fn reply_branch_visibility_follows_loop_mode() {
    use echo_agent::config::TeamMember;
    use echo_agent::plugins::{PARALLEL_LOOP_PLUGIN_ID, SINGLE_LOOP_PLUGIN_ID};
    let (agent, _provider) = test_agent("ok");
    // 未配置 capabilities：默认单会话（分支不可见、turn 串行）。
    assert_eq!(agent.loop_mode(), echo_defs::LoopMode::Single);
    assert!(!agent.shows_reply_branches());
    // 并行多会话 → 可见性事件开启
    agent
        .apply_capabilities(&TeamMember {
            enabled_plugins: vec![PARALLEL_LOOP_PLUGIN_ID.into()],
            ..Default::default()
        })
        .await;
    assert_eq!(agent.loop_mode(), echo_defs::LoopMode::Parallel);
    assert!(agent.shows_reply_branches());
    // 单会话 id → 关闭（分支仍执行，仅不发射可见性事件）
    agent
        .apply_capabilities(&TeamMember {
            enabled_plugins: vec![SINGLE_LOOP_PLUGIN_ID.into()],
            ..Default::default()
        })
        .await;
    assert!(!agent.shows_reply_branches());
    // 非空白名单无模式 id → 单会话兜底
    agent
        .apply_capabilities(&TeamMember {
            enabled_plugins: vec!["echo-agent.tools.builtin".into()],
            ..Default::default()
        })
        .await;
    assert!(!agent.shows_reply_branches());
}

#[tokio::test]
async fn toggle_skill_enables_and_disables() {
    let (agent, _provider) = test_agent("ok");
    agent.skills.lock().await.register(echo_agent::Skill {
        metadata: echo_agent::skill::SkillMetadata {
            name: "calc".into(),
            description: "d".into(),
            keywords: vec!["计算".into()],
            always: false,
            system: false,
            enabled: true,
            category: String::new(),
            package: None,
        },
        instructions: "use calculator".into(),
    });
    agent
        .apply_command(BackendCommand::ToggleSkill {
            name: "calc".into(),
            enabled: false,
        })
        .await;
    assert!(
        !agent
            .skills
            .lock()
            .await
            .get("calc")
            .unwrap()
            .metadata
            .enabled
    );
    agent
        .apply_command(BackendCommand::ToggleSkill {
            name: "calc".into(),
            enabled: true,
        })
        .await;
    assert!(
        agent
            .skills
            .lock()
            .await
            .get("calc")
            .unwrap()
            .metadata
            .enabled
    );
}

#[tokio::test]
async fn skill_keyword_injects_instructions_into_prompt() {
    let (agent, provider) = test_agent("ok");
    agent.skills.lock().await.register(echo_agent::Skill {
        metadata: echo_agent::skill::SkillMetadata {
            name: "calc".into(),
            description: "算术计算".into(),
            keywords: vec!["计算".into()],
            always: false,
            system: false,
            enabled: true,
            category: String::new(),
            package: None,
        },
        instructions: "使用 calculator 工具计算表达式".into(),
    });

    // A message containing the trigger keyword.
    agent
        .apply_command(BackendCommand::SendMessage {
            session_id: "qq:dm::123".into(),
            content: "帮我计算 1+1".into(),
            images: vec![],
            team_id: Some("t".into()),
        })
        .await;
    let prompt = provider.last_system_prompt().await;
    assert!(
        prompt.contains("Triggered skill: calc"),
        "triggered skill section missing: {prompt}"
    );
    assert!(
        prompt.contains("使用 calculator 工具计算表达式"),
        "skill instructions missing: {prompt}"
    );
    assert!(
        prompt.contains("Available skills"),
        "Tier-1 listing present"
    );
}

#[tokio::test]
async fn always_skill_coexists_with_keyword_skill_and_qq_context() {
    let (agent, provider) = test_agent("ok");
    agent.skills.lock().await.register(echo_agent::Skill {
        metadata: echo_agent::skill::SkillMetadata {
            name: "concise-dialogue".into(),
            description: "简洁表达".into(),
            keywords: vec![],
            always: true,
            system: false,
            enabled: true,
            category: String::new(),
            package: None,
        },
        instructions: "QQ 单段通常不超过 30 个字符".into(),
    });
    agent.skills.lock().await.register(echo_agent::Skill {
        metadata: echo_agent::skill::SkillMetadata {
            name: "calc".into(),
            description: "算术计算".into(),
            keywords: vec!["计算".into()],
            always: false,
            system: false,
            enabled: true,
            category: String::new(),
            package: None,
        },
        instructions: "使用 calculator 工具".into(),
    });

    agent
        .apply_command(BackendCommand::SendMessage {
            session_id: "qq:dm::123".into(),
            content: "帮我计算 1+1".into(),
            images: vec![],
            team_id: Some("t".into()),
        })
        .await;

    let prompt = provider.last_system_prompt().await;
    assert!(prompt.contains("Active skill: concise-dialogue"));
    assert!(prompt.contains("QQ 单段通常不超过 30 个字符"));
    assert!(prompt.contains("Triggered skill: calc"));
    assert!(prompt.contains("使用 calculator 工具"));
    // This message was typed in the backend/TUI (no hook), so it must be
    // answered in the backend — not forced through a QQ send tool.
    assert!(
        prompt.contains("# Backend input boundary"),
        "backend input boundary missing: {prompt}"
    );
    assert!(
        prompt.contains("Do NOT call send_private_msg"),
        "backend input must forbid QQ sends: {prompt}"
    );
    assert!(
        !prompt.contains("MUST be answered exactly once"),
        "backend input must not trigger QQ hook rules: {prompt}"
    );
}

#[tokio::test]
async fn qq_hook_input_enforces_transport_boundary() {
    // First answer comes without a send tool, forcing a delivery correction;
    // then the agent calls send_private_msg and finishes with a direct reply.
    let script = vec![
        ChatResponse {
            stop_reason: None,
            content: Some("backend only".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        },
        ChatResponse {
            stop_reason: None,
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "send_1".into(),
                name: "send_private_msg".into(),
                arguments: r#"{"user_id":123,"content":"收到"}"#.into(),
            }],
            usage: Usage::default(),
        },
        ChatResponse {
            stop_reason: None,
            content: Some("delivered".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        },
    ];
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool {
        name: "send_private_msg",
        result: "private message sent".into(),
    }));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    ));
    let key = SessionKey::parse("qq:dm::123").unwrap();
    let session = agent.trunk.get_or_create(&key, "u".into(), None);
    let hook = concat!(
        "<qq_message_hook>\n",
        "{\"event\":\"qq_message\",\"channel\":{\"type\":\"private\"},\"sender\":{\"user_id\":\"123\"},\"content\":\"hello\"}\n",
        "</qq_message_hook>"
    );

    agent.process_message(&session, hook).await.unwrap();

    let prompt = provider.last_system_prompt().await;
    assert!(
        prompt.contains("# QQ transport boundary"),
        "QQ hook must get the transport boundary: {prompt}"
    );
    assert!(
        prompt.contains("Answer every <qq_message_hook> exactly once"),
        "QQ hook must force a send tool reply: {prompt}"
    );
    assert!(
        prompt.contains("never send twice"),
        "QQ hook must forbid double sends: {prompt}"
    );
}

#[tokio::test]
async fn backend_input_in_qq_session_stays_in_backend() {
    let (agent, provider) = test_agent("ok");
    let key = SessionKey::parse("qq:dm::123").unwrap();
    let session = agent.trunk.get_or_create(&key, "u".into(), None);

    // Backend/TUI input routed into a QQ session: no hook marker.
    agent.process_message(&session, "你好").await.unwrap();

    let prompt = provider.last_system_prompt().await;
    assert!(
        prompt.contains("# Backend input boundary"),
        "backend input boundary missing: {prompt}"
    );
    assert!(
        !prompt.contains("# QQ transport boundary"),
        "backend input must not get QQ transport rules: {prompt}"
    );
}

#[tokio::test]
async fn local_tui_input_has_no_qq_boundary() {
    let (agent, provider) = test_agent("ok");
    let key = SessionKey::local_tui();
    let session = agent.trunk.get_or_create(&key, "u".into(), None);

    agent.process_message(&session, "你好").await.unwrap();

    let prompt = provider.last_system_prompt().await;
    assert!(!prompt.contains("# QQ transport boundary"));
    assert!(!prompt.contains("# Backend input boundary"));
}

/// A tool that fails its execution, to exercise the error path.
struct FailingTool {
    name: &'static str,
}

#[async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "failing mock tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(&self, _arguments: serde_json::Value) -> Result<String, ToolError> {
        Err(ToolError::Execution("mock failure".into()))
    }
}

/// Baseline: a turn that performs one tool call and then replies must emit the
/// lifecycle events in the documented order — branch start, LLM request,
/// tool call/result, agent completion, branch completion. This freezes the
/// turn protocol the refactor (TurnRunner) must keep emitting.
#[tokio::test]
async fn turn_emits_lifecycle_events_in_order() {
    let script = vec![
        ChatResponse {
            stop_reason: None,
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "calc_1".into(),
                name: "mock_tool".into(),
                arguments: r#"{"x":1}"#.into(),
            }],
            usage: Usage::default(),
        },
        ChatResponse {
            stop_reason: None,
            content: Some("final".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        },
    ];
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool {
        name: "mock_tool",
        result: "tool done".into(),
    }));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    ));
    let (bridge, handle) = echo_agent::create_bridge();
    agent.attach(Arc::new(handle));

    // 该用例冻结的是含可见回执分支的完整生命周期 → 取并行多会话模式
    //（默认单会话不发射 ReplyBranch*，生命周期少两个事件）。
    agent
        .apply_capabilities(&echo_agent::config::TeamMember {
            enabled_plugins: vec![echo_agent::plugins::PARALLEL_LOOP_PLUGIN_ID.into()],
            ..Default::default()
        })
        .await;

    let key = SessionKey::parse("qq:dm::123").unwrap();
    let session = agent.trunk.get_or_create(&key, "u".into(), None);
    agent.process_message(&session, "计算").await.unwrap();

    let mut events = Vec::new();
    {
        let mut rx = bridge.event_rx.lock().await;
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
    }

    // Filter to the lifecycle-relevant variants, preserving order.
    let lifecycle: Vec<&str> = events
        .iter()
        .map(|ev| match ev {
            BackendEvent::ReplyBranchStarted { .. } => "branch_started",
            BackendEvent::AgentThinking { .. } => "thinking",
            BackendEvent::LlmRequest { .. } => "llm_request",
            BackendEvent::LlmResponse { .. } => "llm_response",
            BackendEvent::ToolCall { .. } => "tool_call",
            BackendEvent::ToolResult { .. } => "tool_result",
            BackendEvent::AgentCompleted { .. } => "agent_completed",
            BackendEvent::ReplyBranchCompleted { .. } => "branch_completed",
            _ => "other",
        })
        .filter(|kind| *kind != "other")
        .collect();

    assert_eq!(
        lifecycle,
        vec![
            "branch_started",
            "thinking",
            "llm_request",
            "llm_response",
            "tool_call",
            "tool_result",
            "llm_request",
            "llm_response",
            "agent_completed",
            "branch_completed",
        ],
        "turn lifecycle order frozen: {lifecycle:?}"
    );
}

/// Baseline: a tool that fails must produce a tool_result whose text starts
/// with "error:", and the turn must still complete (not hang).
#[tokio::test]
async fn failing_tool_result_marks_error_and_turn_completes() {
    let script = vec![
        ChatResponse {
            stop_reason: None,
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "f_1".into(),
                name: "failing_tool".into(),
                arguments: "{}".into(),
            }],
            usage: Usage::default(),
        },
        ChatResponse {
            stop_reason: None,
            content: Some("after failure".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        },
    ];
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(FailingTool {
        name: "failing_tool",
    }));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    ));
    let (bridge, handle) = echo_agent::create_bridge();
    agent.attach(Arc::new(handle));

    let key = SessionKey::parse("qq:dm::123").unwrap();
    let session = agent.trunk.get_or_create(&key, "u".into(), None);
    let reply = agent.process_message(&session, "run").await.unwrap();
    assert_eq!(reply, "after failure");

    let mut events = Vec::new();
    {
        let mut rx = bridge.event_rx.lock().await;
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
    }
    let failed_result = events.iter().find_map(|ev| match ev {
        BackendEvent::ToolResult {
            tool_name, result, ..
        } if tool_name == "failing_tool" => Some(result.as_str()),
        _ => None,
    });
    assert!(
        failed_result.is_some_and(|result| result.starts_with("error:")),
        "tool failure must surface as error: result, got {failed_result:?}"
    );
    assert!(events
        .iter()
        .any(|ev| matches!(ev, BackendEvent::AgentCompleted { .. })));
}

#[tokio::test]
async fn non_triggering_message_uses_cached_prompt() {
    let (agent, provider) = test_agent("ok");
    agent.skills.lock().await.register(echo_agent::Skill {
        metadata: echo_agent::skill::SkillMetadata {
            name: "calc".into(),
            description: "算术计算".into(),
            keywords: vec!["计算".into()],
            always: false,
            system: false,
            enabled: true,
            category: String::new(),
            package: None,
        },
        instructions: "use calculator".into(),
    });

    // First message has no trigger → prompt cached.
    agent
        .apply_command(BackendCommand::SendMessage {
            session_id: "qq:dm::123".into(),
            content: "你好".into(),
            images: vec![],
            team_id: Some("t".into()),
        })
        .await;
    let first = provider.last_system_prompt().await;
    assert!(!first.contains("Triggered skill"), "no trigger: {first}");

    // Second non-triggering message must reuse the same prompt (no
    // re-computation, no drift).
    agent
        .apply_command(BackendCommand::SendMessage {
            session_id: "qq:dm::123".into(),
            content: "再见".into(),
            images: vec![],
            team_id: Some("t".into()),
        })
        .await;
    assert_eq!(provider.last_system_prompt().await, first);
}

/// Phase 3 invariant: after a session with tool calls is persisted and the
/// store is reloaded, the model context rebuilt from the event log still
/// contains the tool-call structure (the pre-event-sourced format dropped
/// it). The event log is the source of truth; trunk_history is its
/// projection.
#[tokio::test]
async fn event_log_persists_tool_structure_across_reload() {
    let path =
        std::env::temp_dir().join(format!("echo-session-events-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);

    // Build an agent, run a turn that produces a tool call and a reply.
    let script = vec![
        ChatResponse {
            stop_reason: None,
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "calc_1".into(),
                name: "mock_tool".into(),
                arguments: r#"{"expr":"1+1"}"#.into(),
            }],
            usage: Usage::default(),
        },
        ChatResponse {
            stop_reason: None,
            content: Some("结果是 2".into()),
            reasoning_content: None,
            tool_calls: vec![],
            usage: Usage::default(),
        },
    ];
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool {
        name: "mock_tool",
        result: "2".into(),
    }));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    ));
    let key = SessionKey::parse("qq:dm::123").unwrap();
    let session = agent.trunk.get_or_create(&key, "u".into(), None);
    agent.trunk.set_persist_path(path.clone());
    agent.process_message(&session, "计算 1+1").await.unwrap();
    agent.trunk.save_now().await;

    // Reload into a fresh store: the event log must rebuild the full context.
    let store2 = echo_agent::TrunkStore::new(1000);
    store2.set_persist_path(path.clone());
    let restored = store2.load_from_file().await;
    assert_eq!(restored, 1, "identity restored");

    let events = store2.event_log();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, echo_session::SessionEvent::ToolCall(_))),
        "tool call event persisted"
    );
    // The rebuilt session history (from the event log) contains the tool result.
    let history = store2
        .snapshot_for(&SessionKey::parse("qq:dm::123").unwrap().to_session_id())
        .await;
    assert!(
        history
            .iter()
            .any(|message| matches!(message.role, echo_agent::llm::ChatRole::Tool)),
        "tool message rebuilt from log: {history:?}"
    );
    let _ = std::fs::remove_file(&path);
}

// ── Persona 级门控热更新（插件/工具/技能勾选即时生效、可恢复、全局优先）──

use echo_agent::plugins::{CHECKLIST_PLUGIN_ID, TOOLS_BUILTIN_PLUGIN_ID};

/// 构造带"门控工具"的 agent：checklist 属 echo-agent.checklist 包，
/// bash 属 echo-agent.tools.builtin 包。
fn agent_with_gated_caps() -> Arc<Agent> {
    let provider = Arc::new(StaticProvider {
        reply: "ok",
        calls: AtomicUsize::new(0),
        last_system_prompt: tokio::sync::Mutex::new(None),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool {
        name: "checklist",
        result: "ok".into(),
    }));
    tools.register(Arc::new(MockTool {
        name: "bash",
        result: "ok".into(),
    }));
    tools.set_package("checklist", CHECKLIST_PLUGIN_ID);
    tools.set_package("bash", TOOLS_BUILTIN_PLUGIN_ID);
    let agent = Arc::new(Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(AdapterRegistry::new()),
    ));
    agent.set_team_id(Some("t".into()));
    agent
}

fn persona_with_plugins(enabled: &[&str]) -> echo_agent::config::TeamMember {
    echo_agent::config::TeamMember {
        name: "t".into(),
        enabled_plugins: enabled.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

/// 插件勾选往返：取消勾选立即禁用、重新勾选立即恢复、其他包不受影响。
#[tokio::test]
async fn persona_plugin_checkbox_round_trip_applies_immediately() {
    let agent = agent_with_gated_caps();

    agent
        .apply_capabilities(&persona_with_plugins(&[
            TOOLS_BUILTIN_PLUGIN_ID,
            CHECKLIST_PLUGIN_ID,
        ]))
        .await;
    assert!(!agent.tools.is_disabled("checklist").await);
    assert!(!agent.tools.is_disabled("bash").await);

    // 取消勾选 checklist → 立即禁用（无需重启）
    agent
        .apply_capabilities(&persona_with_plugins(&[TOOLS_BUILTIN_PLUGIN_ID]))
        .await;
    assert!(
        agent.tools.is_disabled("checklist").await,
        "取消勾选应立即禁用该包工具"
    );
    assert!(!agent.tools.is_disabled("bash").await, "其他包不受影响");

    // 重新勾选 → 立即恢复（旧实现只能重启恢复）
    agent
        .apply_capabilities(&persona_with_plugins(&[
            TOOLS_BUILTIN_PLUGIN_ID,
            CHECKLIST_PLUGIN_ID,
        ]))
        .await;
    assert!(
        !agent.tools.is_disabled("checklist").await,
        "重新勾选应立即恢复"
    );
}

/// 全局插件 mount/unmount（TogglePlugin）逐 persona 重评估：
/// mount 不放开"名单外"的 persona；unmount 对全员生效；重新 mount 恢复。
#[tokio::test]
async fn global_plugin_gating_respects_persona_allowlist() {
    let agent = agent_with_gated_caps();
    agent
        .apply_capabilities(&persona_with_plugins(&[TOOLS_BUILTIN_PLUGIN_ID]))
        .await;
    assert!(agent.tools.is_disabled("checklist").await);

    // 全局 mount（启用）不应把"名单外"的 persona 放开
    agent.reapply_plugin_gating(CHECKLIST_PLUGIN_ID, true);
    assert!(
        agent.tools.is_disabled("checklist").await,
        "名单外 persona 不该被全局 mount 放开"
    );

    // 勾选后（名单允许）全局 mount 恢复
    agent
        .apply_capabilities(&persona_with_plugins(&[
            TOOLS_BUILTIN_PLUGIN_ID,
            CHECKLIST_PLUGIN_ID,
        ]))
        .await;
    assert!(!agent.tools.is_disabled("checklist").await);

    // 全局 unmount（禁用）对全员生效（即使名单允许）
    agent.reapply_plugin_gating(CHECKLIST_PLUGIN_ID, false);
    assert!(
        agent.tools.is_disabled("checklist").await,
        "全局禁用应优先于 persona 名单"
    );

    // 全局重新启用 + 名单允许 → 恢复
    agent.reapply_plugin_gating(CHECKLIST_PLUGIN_ID, true);
    assert!(!agent.tools.is_disabled("checklist").await);
}

/// 全局工具启停逐 persona 重评估：persona 黑名单不被全局启用覆盖；
/// 全局禁用全员生效；全局重新启用可恢复。
#[tokio::test]
async fn global_tool_toggle_respects_persona_denylist() {
    let agent = agent_with_gated_caps();
    agent
        .apply_capabilities(&echo_agent::config::TeamMember {
            name: "t".into(),
            disabled_tools: vec!["checklist".into()],
            ..Default::default()
        })
        .await;
    assert!(agent.tools.is_disabled("checklist").await);

    assert!(agent.reapply_tool_gating("checklist", true).await);
    assert!(
        agent.tools.is_disabled("checklist").await,
        "persona 黑名单不被全局启用覆盖"
    );

    assert!(agent.reapply_tool_gating("bash", false).await);
    assert!(agent.tools.is_disabled("bash").await, "全局禁用全员生效");

    assert!(agent.reapply_tool_gating("bash", true).await);
    assert!(!agent.tools.is_disabled("bash").await, "全局重新启用可恢复");
}

/// 技能勾选往返：enabled_skills 白名单收紧，重新勾选恢复。
#[tokio::test]
async fn persona_skill_checkbox_round_trip() {
    let (agent, _provider) = test_agent("ok");
    for name in ["alpha", "beta"] {
        agent.skills.lock().await.register(echo_agent::Skill {
            metadata: echo_agent::skill::SkillMetadata {
                name: name.into(),
                description: "d".into(),
                keywords: vec![],
                always: false,
                system: false,
                enabled: true,
                category: String::new(),
                package: None,
            },
            instructions: "use it".into(),
        });
    }

    agent
        .apply_capabilities(&echo_agent::config::TeamMember {
            name: "t".into(),
            enabled_skills: vec!["alpha".into()],
            ..Default::default()
        })
        .await;
    let skills = agent.skills.lock().await;
    assert!(skills.get("alpha").unwrap().metadata.enabled);
    assert!(
        !skills.get("beta").unwrap().metadata.enabled,
        "白名单外的技能应禁用"
    );
    drop(skills);

    agent
        .apply_capabilities(&echo_agent::config::TeamMember {
            name: "t".into(),
            enabled_skills: vec!["alpha".into(), "beta".into()],
            ..Default::default()
        })
        .await;
    let skills = agent.skills.lock().await;
    assert!(
        skills.get("beta").unwrap().metadata.enabled,
        "重新勾选应立即恢复"
    );
}

/// Package 标签横跨工具与技能：一次包级门控同时作用于两个注册表；
/// 启用方向受 persona 名单收紧；包外成员不受影响。
#[tokio::test]
async fn package_gating_spans_tools_and_skills() {
    use echo_agent::plugins::{ADAPTER_QQ_PLUGIN_ID, TOOLS_BUILTIN_PLUGIN_ID};

    let provider = Arc::new(StaticProvider {
        reply: "ok",
        calls: AtomicUsize::new(0),
        last_system_prompt: tokio::sync::Mutex::new(None),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool {
        name: "send_private_msg",
        result: "ok".into(),
    }));
    tools.register(Arc::new(MockTool {
        name: "bash",
        result: "ok".into(),
    }));
    tools.set_package("send_private_msg", ADAPTER_QQ_PLUGIN_ID);
    tools.set_package("bash", TOOLS_BUILTIN_PLUGIN_ID);

    let mut skills = SkillRegistry::new();
    for (name, pkg) in [
        ("qq-management", Some(ADAPTER_QQ_PLUGIN_ID)),
        ("qq-transport", Some(ADAPTER_QQ_PLUGIN_ID)),
        ("calculator", None),
    ] {
        skills.register(echo_agent::Skill {
            metadata: echo_agent::skill::SkillMetadata {
                name: name.into(),
                description: "d".into(),
                keywords: vec![],
                always: false,
                system: false,
                enabled: true,
                category: String::new(),
                package: pkg.map(str::to_string),
            },
            instructions: "use it".into(),
        });
    }

    let agent = Arc::new(Agent::new(
        provider,
        AgentConfig::default(),
        skills,
        tools,
        Arc::new(AdapterRegistry::new()),
    ));

    // 包级禁用：QQ 工具 + QQ 技能一起关闭，包外成员不受影响。
    agent.apply_plugin_gating(ADAPTER_QQ_PLUGIN_ID, false);
    assert!(agent.tools.is_disabled("send_private_msg").await);
    {
        let s = agent.skills.lock().await;
        assert!(!s.get("qq-management").unwrap().metadata.enabled);
        assert!(!s.get("qq-transport").unwrap().metadata.enabled);
        assert!(
            s.get("calculator").unwrap().metadata.enabled,
            "包外技能不受影响"
        );
    }
    assert!(!agent.tools.is_disabled("bash").await, "包外工具不受影响");

    // 包级恢复：两者一起恢复。
    agent.apply_plugin_gating(ADAPTER_QQ_PLUGIN_ID, true);
    assert!(!agent.tools.is_disabled("send_private_msg").await);
    {
        let s = agent.skills.lock().await;
        assert!(s.get("qq-management").unwrap().metadata.enabled);
        assert!(s.get("qq-transport").unwrap().metadata.enabled);
    }

    // 启用方向受 persona 名单收紧：黑名单中的包内技能保持禁用。
    agent
        .apply_capabilities(&echo_agent::config::TeamMember {
            name: "t".into(),
            disabled_skills: vec!["qq-management".into()],
            ..Default::default()
        })
        .await;
    agent.apply_plugin_gating(ADAPTER_QQ_PLUGIN_ID, false);
    agent.apply_plugin_gating(ADAPTER_QQ_PLUGIN_ID, true);
    {
        let s = agent.skills.lock().await;
        assert!(
            !s.get("qq-management").unwrap().metadata.enabled,
            "persona 黑名单技能不随包恢复"
        );
        assert!(s.get("qq-transport").unwrap().metadata.enabled);
    }
}
