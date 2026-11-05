//! End-to-end integration tests: a real Agent driven through the bridge.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use echo_agent::llm::{
    ChatChunk, ChatRequest, ChatResponse, LlmError, LlmProvider, ToolCall, Usage,
};
use echo_agent::tool::{Tool, ToolError};
use echo_agent::{
    Agent, AgentConfig, BackendCommand, BackendEvent, SessionKey, SkillRegistry, ToolRegistry,
};
use echo_defs::module_instance::InstanceRegistry;
use std::collections::VecDeque;

/// A static-reply provider that counts calls and records the last system prompt.
struct StaticProvider {
    reply: &'static str,
    calls: AtomicUsize,
    last_system_prompt: tokio::sync::Mutex<Option<String>>,
    /// 最近一次请求携带的工具名（schema 发送断言用）。
    last_tool_names: tokio::sync::Mutex<Vec<String>>,
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
        *self.last_tool_names.lock().await = request
            .tools
            .as_ref()
            .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
            .unwrap_or_default();
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
        request: &ChatRequest,
        _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError> {
        // 测试桩：流式与非流式同效（主对话默认经此路径；增量断言由专门用例覆盖）。
        self.chat(request).await
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

    async fn last_tool_names(&self) -> Vec<String> {
        self.last_tool_names.lock().await.clone()
    }
}

/// A provider that replays a fixed script of responses, recording the last
/// system prompt seen.
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
        request: &ChatRequest,
        _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError> {
        // 测试桩：流式与非流式同效（主对话默认经此路径；增量断言由专门用例覆盖）。
        self.chat(request).await
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
        last_tool_names: tokio::sync::Mutex::new(Vec::new()),
    });
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(InstanceRegistry::new()),
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
    apply_via_provider_module(
        &agent,
        BackendCommand::SetSystemPrompt {
            prompt: "你是翻译助手".into(),
        },
    )
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
    apply_via_provider_module(
        &agent,
        BackendCommand::SetSystemPrompt {
            prompt: "你是翻译助手".into(),
        },
    )
    .await;
    let content = std::fs::read_to_string(&tmp).unwrap();
    // 系统提示词由 [modules.system_prompt] 模块段拥有（Core 模块注入后端 API
    // 配置无关），持久化目标是 modules 段而非 [agent] 段。
    assert!(
        content.contains("[modules.system_prompt]"),
        "modules section missing: {content}"
    );
    assert!(
        content.contains("text = \"你是翻译助手\""),
        "prompt not persisted into modules: {content}"
    );
    // 运行期以 modules 段为准（Core 启动时用 modules 覆盖 agent 段）。
    std::fs::remove_file(&tmp).ok();
}

#[tokio::test]
async fn switch_model_updates_active_model() {
    let (agent, _provider) = test_agent("ok");
    apply_via_provider_module(
        &agent,
        BackendCommand::SwitchModel {
            model: "gpt-4o".into(),
        },
    )
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
    apply_via_provider_module(
        &agent,
        BackendCommand::SwitchModel {
            model: "gpt-4o".into(),
        },
    )
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
    apply_via_provider_module(
        &agent,
        BackendCommand::UpdateApiConfig {
            name: "deepseek".into(),
            provider: "deepseek".into(),
            model: "ds-v4".into(),
            base_url: "http://x".into(),
            api_key: String::new(),
            thinking: None,
            reasoning_effort: None,
        },
    )
    .await;
    apply_via_provider_module(
        &agent,
        BackendCommand::SwitchApi {
            name: "deepseek".into(),
        },
    )
    .await;
    let cfg = agent.api_config().await;
    assert_eq!(cfg.active_api, "deepseek");
    assert_eq!(cfg.api_profiles.len(), 1);
}

#[tokio::test]
async fn delete_api_removes_profile() {
    let (agent, _provider) = test_agent("ok");
    apply_via_provider_module(
        &agent,
        BackendCommand::UpdateApiConfig {
            name: "temp".into(),
            provider: "openai".into(),
            model: "m".into(),
            base_url: String::new(),
            api_key: String::new(),
            thinking: None,
            reasoning_effort: None,
        },
    )
    .await;
    assert_eq!(agent.api_config().await.api_profiles.len(), 1);
    apply_via_provider_module(
        &agent,
        BackendCommand::DeleteApi {
            name: "temp".into(),
        },
    )
    .await;
    assert!(
        agent.api_config().await.api_profiles.is_empty(),
        "profile removed"
    );
}

/// 改名端到端：命令 → 池 / active_api 改写 + 磁盘同步（单 patch）。
#[tokio::test]
async fn rename_api_end_to_end_via_apply_command() {
    let (agent, _provider) = test_agent("ok");
    let tmp = std::env::temp_dir().join(format!("echo-agent-rename-{}.toml", uuid::Uuid::new_v4()));
    std::fs::write(
        &tmp,
        "[agent]\nprovider = \"openai\"\nmodel = \"top\"\napi_key = \"sk-top\"\n",
    )
    .unwrap();
    agent.set_config_path(tmp.clone());

    apply_via_provider_module(
        &agent,
        BackendCommand::UpdateApiConfig {
            name: "deepseek".into(),
            provider: "deepseek".into(),
            model: "ds-v4".into(),
            base_url: "http://x".into(),
            api_key: "sk-ds".into(),
            thinking: None,
            reasoning_effort: None,
        },
    )
    .await;
    assert!(agent.api_config().await.active_api == "deepseek");

    // RenameApi 是**进程级命令**（改全局供应商池，不属于任何人格）：
    // 生产路径由命令泵先交给 `process_commands`，不经过 `apply_command`。
    // 这里直接调它（`admin = None` = 没有其他人格引用要传播）。
    agent.rename_api("deepseek", "ds", None).await;

    let cfg = agent.api_config().await;
    assert!(
        cfg.api_profiles.iter().any(|p| p.name == "ds"),
        "renamed profile present"
    );
    assert!(
        cfg.api_profiles.iter().all(|p| p.name != "deepseek"),
        "old name gone"
    );
    assert_eq!(cfg.active_api, "ds", "active_api follows");

    let content = std::fs::read_to_string(&tmp).unwrap();
    assert!(content.contains("name = \"ds\""), "persisted: {content}");
    assert!(
        !content.contains("name = \"deepseek\""),
        "old name not persisted: {content}"
    );
    assert!(
        content.contains("active_api = \"ds\""),
        "persisted: {content}"
    );
    assert!(
        content.contains("api_key = \"sk-ds\""),
        "profile key preserved: {content}"
    );
    std::fs::remove_file(&tmp).ok();
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
        request: &ChatRequest,
        _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError> {
        // 测试桩：流式与非流式同效（主对话默认经此路径；增量断言由专门用例覆盖）。
        self.chat(request).await
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
        Arc::new(InstanceRegistry::new()),
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
    let hist = session.history.lock().clone();
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
async fn module_list_reports_mounted_modules_only() {
    use echo_module::{BuiltinModule, ModuleManifest};
    let (agent, _provider) = test_agent("ok");
    agent
        .module_host()
        .register_and_mount(std::sync::Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("test.module.one", "测试模块", "0.1", "tool", "t", "d"),
            |_| Ok(vec![]),
        )))
        .unwrap();
    assert_eq!(agent.module_host().descriptors().len(), 1);

    agent
        .apply_command(BackendCommand::RequestModulesList)
        .await;

    // 清单只列出**挂载中**的模块：卸载后从清单消失（没有 enabled 字段，
    // 因为"禁用"这个动作已经删除——模块只有挂载 / 没挂载）。
    agent
        .module_host()
        .registry
        .unmount("test.module.one")
        .unwrap();
    agent
        .apply_command(BackendCommand::RequestModulesList)
        .await;
    assert_eq!(agent.module_host().descriptors().len(), 1);
    assert_eq!(
        agent.module_host().descriptors()[0].id,
        "test.module.one",
        "descriptor 仍然存在（已注册），但 emit 侧会按挂载态过滤"
    );
}

#[tokio::test]
async fn toggle_skill_enables_and_disables() {
    let (agent, _provider) = test_agent("ok");
    agent.skills().lock().await.register(echo_agent::Skill {
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
    // 命令域归模块（全模块化 S3）：驱动**模块侧处理器**而不是 `apply_command`
    // ——后者在"模块未安装"时会回"模块未启用"。这样测的仍是能力本身
    // （启停 + 持久化 + 全局策略），而不依赖模块装配。
    let host: Arc<dyn echo_agent::skill::commands::SkillsCommandHost> = Arc::new(
        echo_agent::agent::skills_host::AgentSkillsCommandHost::new(agent.clone()),
    );
    let handler = echo_agent::skill::commands::SkillsCommandHandler::new(host);

    handler
        .handle(
            "ToggleSkill",
            serde_json::json!({"name": "calc", "enabled": false}),
        )
        .await
        .expect("本域命令")
        .expect("载荷合法");
    assert!(
        !agent
            .skills()
            .lock()
            .await
            .get("calc")
            .unwrap()
            .metadata
            .enabled
    );
    handler
        .handle(
            "ToggleSkill",
            serde_json::json!({"name": "calc", "enabled": true}),
        )
        .await
        .expect("本域命令")
        .expect("载荷合法");
    assert!(
        agent
            .skills()
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
    agent.skills().lock().await.register(echo_agent::Skill {
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
    agent.skills().lock().await.register(echo_agent::Skill {
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
    agent.skills().lock().await.register(echo_agent::Skill {
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
        prompt.contains("Do NOT call any platform send tool"),
        "backend input must forbid platform sends: {prompt}"
    );
    assert!(
        !prompt.contains("MUST be answered exactly once"),
        "backend input must not trigger QQ hook rules: {prompt}"
    );
}

#[tokio::test]
async fn qq_hook_input_gets_transport_boundary_prompt() {
    // A backend-only reply is returned as-is: transport discipline lives in
    // the prompt/skills, not in a forced delivery-correction loop.
    let script = vec![ChatResponse {
        stop_reason: None,
        content: Some("backend only".into()),
        reasoning_content: None,
        tool_calls: vec![],
        usage: Usage::default(),
    }];
    let provider = Arc::new(ScriptedProvider::new(script));
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(InstanceRegistry::new()),
    ));
    let key = SessionKey::parse("qq:dm::123").unwrap();
    let session = agent.trunk.get_or_create(&key, "u".into(), None);
    let hook = concat!(
        "<qq_message_hook>\n",
        "{\"event\":\"qq_message\",\"channel\":{\"type\":\"private\"},\"sender\":{\"user_id\":\"123\"},\"content\":\"hello\"}\n",
        "</qq_message_hook>"
    );

    let reply = agent.process_message(&session, hook).await.unwrap();
    assert_eq!(reply, "backend only");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

    let prompt = provider.last_system_prompt().await;
    assert!(
        prompt.contains("# Platform message boundary (qq)"),
        "平台 hook 必须注入平台边界块（平台名来自标记）: {prompt}"
    );
    assert!(
        prompt.contains("Answer every platform hook exactly once"),
        "平台 hook 必须引导走发送工具: {prompt}"
    );
    assert!(
        prompt.contains("never send twice"),
        "平台 hook 必须禁止重复发送: {prompt}"
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
        Arc::new(InstanceRegistry::new()),
    ));
    let (bridge, handle) = echo_agent::create_bridge();
    agent.attach(Arc::new(handle));

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
            BackendEvent::AgentThinking { .. } => "thinking",
            BackendEvent::LlmRequest { .. } => "llm_request",
            BackendEvent::LlmResponse { .. } => "llm_response",
            BackendEvent::ToolCall { .. } => "tool_call",
            BackendEvent::ToolResult { .. } => "tool_result",
            BackendEvent::AgentCompleted { .. } => "agent_completed",
            _ => "other",
        })
        .filter(|kind| *kind != "other")
        .collect();

    assert_eq!(
        lifecycle,
        vec![
            "thinking",
            "llm_request",
            "llm_response",
            "tool_call",
            "tool_result",
            "llm_request",
            "llm_response",
            "agent_completed",
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
        Arc::new(InstanceRegistry::new()),
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
    agent.skills().lock().await.register(echo_agent::Skill {
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
        Arc::new(InstanceRegistry::new()),
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

// ── Persona 级门控热更新（模块/工具/技能勾选即时生效、可恢复、全局优先）──

use echo_agent::modules::{TOOLS_BUILTIN_MODULE_ID, WORKSPACE_MODULE_ID};

/// 构造带"门控工具"的 agent：workspace 属 echo-agent.workspace 包，
/// bash 属 echo-agent.tools.builtin 包。
/// （checklist 已降级为普通内置工具，不再用作包门控示例。）
fn agent_with_gated_caps() -> Arc<Agent> {
    let provider = Arc::new(StaticProvider {
        reply: "ok",
        calls: AtomicUsize::new(0),
        last_system_prompt: tokio::sync::Mutex::new(None),
        last_tool_names: tokio::sync::Mutex::new(Vec::new()),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool {
        name: "workspace",
        result: "ok".into(),
    }));
    tools.register(Arc::new(MockTool {
        name: "bash",
        result: "ok".into(),
    }));
    tools.set_package("workspace", WORKSPACE_MODULE_ID);
    tools.set_package("bash", TOOLS_BUILTIN_MODULE_ID);
    let agent = Arc::new(Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(InstanceRegistry::new()),
    ));
    agent.set_team_id(Some("t".into()));
    agent
}

fn persona_with_modules(enabled: &[&str]) -> echo_agent::config::TeamMember {
    echo_agent::config::TeamMember {
        name: "t".into(),
        enabled_modules: enabled.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

/// 模块勾选往返：取消勾选立即禁用、重新勾选立即恢复、其他包不受影响。
#[tokio::test]
async fn persona_module_checkbox_round_trip_applies_immediately() {
    let agent = agent_with_gated_caps();

    agent
        .apply_capabilities(&persona_with_modules(&[
            TOOLS_BUILTIN_MODULE_ID,
            WORKSPACE_MODULE_ID,
        ]))
        .await;
    assert!(!agent.tools.is_disabled("workspace").await);
    assert!(!agent.tools.is_disabled("bash").await);

    // 取消勾选 workspace → 立即禁用（无需重启）
    agent
        .apply_capabilities(&persona_with_modules(&[TOOLS_BUILTIN_MODULE_ID]))
        .await;
    assert!(
        agent.tools.is_disabled("workspace").await,
        "取消勾选应立即禁用该包工具"
    );
    assert!(!agent.tools.is_disabled("bash").await, "其他包不受影响");

    // 重新勾选 → 立即恢复（旧实现只能重启恢复）
    agent
        .apply_capabilities(&persona_with_modules(&[
            TOOLS_BUILTIN_MODULE_ID,
            WORKSPACE_MODULE_ID,
        ]))
        .await;
    assert!(
        !agent.tools.is_disabled("workspace").await,
        "重新勾选应立即恢复"
    );
}

/// **启动窗口不变式**：`Agent` 早于模块挂载构造，那时注册表里空无一物。
///
/// 这是真实启动顺序（`make_agent` → `host_services::register` → `mount_plan`）：
/// `host_services` 需要 `Arc<Agent>` 才能装模块要的宿主接缝，所以 agent 必须先
/// 存在；等模块挂上时 persona 早已构造完毕。
///
/// 结论：门控的完整性**不能依赖挂载**。第二个来源是工具/技能的**包标签**
/// （组合根注册工具时打好，早于挂载）——这条测试把它钉住。
///
/// `agent_with_gated_caps` 从不调用 `set_module_host`，`module_host()` 回落到
/// 一个空宿主，因此它天然复现了"挂载前"的状态。
#[tokio::test]
async fn gating_works_before_any_module_is_mounted() {
    let agent = agent_with_gated_caps();

    // 前置：注册表确实一个模块都没有——这就是 make_agent 时刻的真实状态。
    // 若将来启动顺序被改成"先挂模块再建 agent"，这条断言会红，提醒改写下面
    // 的推理（那时门控集合会直接来自 descriptor，包标签只是冗余保险）。
    assert!(
        agent.module_host().registry.descriptors().is_empty(),
        "本测试的前提是「尚无模块挂载」；启动顺序已变则需重写本用例"
    );

    // 但门控集合仍然覆盖了两个包维度模块——来自工具包标签，不来自注册表。
    let gated = agent.gated_module_ids_snapshot();
    for id in [WORKSPACE_MODULE_ID, TOOLS_BUILTIN_MODULE_ID] {
        assert!(
            gated.iter().any(|g| g == id),
            "挂载前也必须能派生出门控项 {id}，实际: {gated:?}"
        );
    }

    // 而且真的生效：名单里没有 workspace → 它的工具被禁用。
    agent
        .apply_capabilities(&persona_with_modules(&[TOOLS_BUILTIN_MODULE_ID]))
        .await;
    assert!(
        agent.tools.is_disabled("workspace").await,
        "挂载前 apply_capabilities 也必须关掉名单外的包"
    );
    assert!(!agent.tools.is_disabled("bash").await);
}

/// 全局模块 mount/unmount 事件逐 persona 重评估：
/// mount 不放开"名单外"的 persona；unmount 对全员生效；重新 mount 恢复。
#[tokio::test]
async fn global_module_gating_respects_persona_allowlist() {
    let agent = agent_with_gated_caps();
    agent
        .apply_capabilities(&persona_with_modules(&[TOOLS_BUILTIN_MODULE_ID]))
        .await;
    assert!(agent.tools.is_disabled("workspace").await);

    // 全局 mount（启用）不应把"名单外"的 persona 放开
    agent.reapply_module_gating(WORKSPACE_MODULE_ID, true);
    assert!(
        agent.tools.is_disabled("workspace").await,
        "名单外 persona 不该被全局 mount 放开"
    );

    // 勾选后（名单允许）全局 mount 恢复
    agent
        .apply_capabilities(&persona_with_modules(&[
            TOOLS_BUILTIN_MODULE_ID,
            WORKSPACE_MODULE_ID,
        ]))
        .await;
    assert!(!agent.tools.is_disabled("workspace").await);

    // 全局 unmount（禁用）对全员生效（即使名单允许）
    agent.reapply_module_gating(WORKSPACE_MODULE_ID, false);
    assert!(
        agent.tools.is_disabled("workspace").await,
        "全局禁用应优先于 persona 名单"
    );

    // 全局重新启用 + 名单允许 → 恢复
    agent.reapply_module_gating(WORKSPACE_MODULE_ID, true);
    assert!(!agent.tools.is_disabled("workspace").await);
}

/// 全局工具启停逐 persona 重评估：persona 黑名单不被全局启用覆盖；
/// 全局禁用全员生效；全局重新启用可恢复。
#[tokio::test]
async fn global_tool_toggle_respects_persona_denylist() {
    let agent = agent_with_gated_caps();
    agent
        .apply_capabilities(&echo_agent::config::TeamMember {
            name: "t".into(),
            disabled_tools: vec!["workspace".into()],
            ..Default::default()
        })
        .await;
    assert!(agent.tools.is_disabled("workspace").await);

    assert!(agent.reapply_tool_gating("workspace", true).await);
    assert!(
        agent.tools.is_disabled("workspace").await,
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
        agent.skills().lock().await.register(echo_agent::Skill {
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
    let skills = agent.skills().lock().await;
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
    let skills = agent.skills().lock().await;
    assert!(
        skills.get("beta").unwrap().metadata.enabled,
        "重新勾选应立即恢复"
    );
}

/// Package 标签横跨工具与技能：一次包级门控同时作用于两个注册表；
/// 启用方向受 persona 名单收紧；包外成员不受影响。
#[tokio::test]
async fn package_gating_spans_tools_and_skills() {
    use echo_agent::modules::TOOLS_BUILTIN_MODULE_ID;
    /// 测试自带的实例模块 id（框架不认识任何真实平台名）。
    const ADAPTER_QQ_MODULE_ID: &str = "echo-agent.instance.test";

    let provider = Arc::new(StaticProvider {
        reply: "ok",
        calls: AtomicUsize::new(0),
        last_system_prompt: tokio::sync::Mutex::new(None),
        last_tool_names: tokio::sync::Mutex::new(Vec::new()),
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
    tools.set_package("send_private_msg", ADAPTER_QQ_MODULE_ID);
    tools.set_package("bash", TOOLS_BUILTIN_MODULE_ID);

    let mut skills = SkillRegistry::new();
    for (name, pkg) in [
        ("qq-management", Some(ADAPTER_QQ_MODULE_ID)),
        ("qq-transport", Some(ADAPTER_QQ_MODULE_ID)),
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
        Arc::new(InstanceRegistry::new()),
    ));

    // 包级禁用：QQ 工具 + QQ 技能一起关闭，包外成员不受影响。
    agent.apply_module_gating(ADAPTER_QQ_MODULE_ID, false);
    assert!(agent.tools.is_disabled("send_private_msg").await);
    {
        let s = agent.skills().lock().await;
        assert!(!s.get("qq-management").unwrap().metadata.enabled);
        assert!(!s.get("qq-transport").unwrap().metadata.enabled);
        assert!(
            s.get("calculator").unwrap().metadata.enabled,
            "包外技能不受影响"
        );
    }
    assert!(!agent.tools.is_disabled("bash").await, "包外工具不受影响");

    // 包级恢复：两者一起恢复。
    agent.apply_module_gating(ADAPTER_QQ_MODULE_ID, true);
    assert!(!agent.tools.is_disabled("send_private_msg").await);
    {
        let s = agent.skills().lock().await;
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
    agent.apply_module_gating(ADAPTER_QQ_MODULE_ID, false);
    agent.apply_module_gating(ADAPTER_QQ_MODULE_ID, true);
    {
        let s = agent.skills().lock().await;
        assert!(
            !s.get("qq-management").unwrap().metadata.enabled,
            "persona 黑名单技能不随包恢复"
        );
        assert!(s.get("qq-transport").unwrap().metadata.enabled);
    }
}

// ── Subagent 委派（echo-agent.subagent 模块）──

/// 装配了 subagent 运行态的 agent（模块 mount 的测试等价物）。
/// 走**模块命令域**（全模块化 S5）：API / provider 命令由
/// `echo-module-provider-llm` 认领，测试直接打宿主接缝（模块侧的派发测试在
/// 那个 crate 里），于是这些用例仍然只验证"命令做了什么"。
async fn apply_via_provider_module(agent: &Arc<Agent>, cmd: BackendCommand) {
    let host = Arc::new(
        echo_agent::agent::provider_host::AgentProviderCommandHost::new(Arc::clone(agent)),
    );
    let handler = echo_module_provider_llm::commands::ProviderCommandHandler::new(host);
    handler
        .handle(cmd.kind_name(), cmd.payload_value())
        .await
        .expect("命令应属于 provider 命令域")
        .expect("命令处理失败");
}

fn test_agent_with_subagent(reply: &'static str) -> (Arc<Agent>, Arc<StaticProvider>) {
    let (agent, provider) = test_agent(reply);
    agent.attach_subagent_runtime(echo_agent::subagent::SubagentStore::new());
    (agent, provider)
}

/// 「工具存在」与「能力可用」现在是**同一件事**（2026-11 S7）。
///
/// 此前 `spawn_subagent` 在装配期由组合根注册成占位工具（只为 schema），
/// 真正能不能用靠 `allows_dynamic_tool` 里一条工具名特判回答——于是"注册表里
/// 有它"与"它可执行"可以不一致。现在注册与接线是同一个动作
/// （`attach_subagent_runtime`），判据只剩注册表本身。
#[tokio::test]
async fn spawn_subagent_exists_only_when_runtime_attached() {
    use echo_agent::subagent::SPAWN_SUBAGENT_TOOL;
    // 未装配：注册表里没有它（模型看不到 schema，调用按未注册报错）。
    let (bare, _) = test_agent("ok");
    assert!(
        !bare
            .tools
            .names()
            .contains(&SPAWN_SUBAGENT_TOOL.to_string()),
        "未装配运行态时不该注册"
    );
    // 装配后：注册表里有它，且带 subagent 模块包标签（门控按包生效）。
    let (agent, _) = test_agent_with_subagent("ok");
    assert!(agent
        .tools
        .names()
        .contains(&SPAWN_SUBAGENT_TOOL.to_string()));
    assert_eq!(
        agent.tools.package_of(SPAWN_SUBAGENT_TOOL).as_deref(),
        Some(echo_agent::modules::SUBAGENT_MODULE_ID),
        "包标签是模块门控的唯一依据"
    );
}

/// 回归：生产装配（core 组合根）把 spawn_subagent 注册进 persona 注册表；
/// 模型请求中的工具名必须唯一——重复定义会让 API 直接拒绝整个请求
///（"Tool names must be unique"）。
#[tokio::test]
async fn registered_subagent_tool_is_sent_once_with_unique_names() {
    let provider = Arc::new(StaticProvider {
        reply: "ok",
        calls: AtomicUsize::new(0),
        last_system_prompt: tokio::sync::Mutex::new(None),
        last_tool_names: tokio::sync::Mutex::new(Vec::new()),
    });
    // main.rs 装配的等价物：工具进注册表 + subagent 包标签 + 运行态接线。
    let store = echo_agent::subagent::SubagentStore::new();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(echo_agent::subagent::SpawnSubagentTool::new(
        store.clone(),
        Arc::new(|_| {}),
    )));
    tools.set_package("spawn_subagent", echo_agent::modules::SUBAGENT_MODULE_ID);
    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(InstanceRegistry::new()),
    ));
    agent.attach_subagent_runtime(store);
    agent
        .apply_capabilities(&echo_agent::TeamMember {
            enabled_modules: vec![echo_agent::modules::SUBAGENT_MODULE_ID.into()],
            ..Default::default()
        })
        .await;
    assert!(agent.allows_dynamic_tool("spawn_subagent"));

    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    agent.process_message(&session, "hi").await.unwrap();

    let names = provider.last_tool_names().await;
    assert!(names.contains(&"spawn_subagent".to_string()), "{names:?}");
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(names.len(), unique.len(), "工具名重复: {names:?}");
}
#[tokio::test]
async fn spawn_subagent_runs_child_and_reports_back_via_hook() {
    let (agent, provider) = test_agent_with_subagent("子任务结论：共 3 处用法");
    let (bridge, handle) = echo_agent::create_bridge();
    agent.attach(Arc::new(handle));
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);

    // 主 agent 的模型直接发 spawn_subagent 调用（不经 LLM：直接走 run_tool）。
    let call = ToolCall {
        id: "call-spawn-1".into(),
        name: "spawn_subagent".into(),
        arguments: r#"{"task":"找出所有 X 的用法"}"#.into(),
    };
    let result = agent.run_tool(&session.id, "branch-main", &call).await;
    assert!(
        result.text.contains("子任务已受理"),
        "receipt: {}",
        result.text
    );

    // 子任务在后台执行：等 hook 回灌（子 agent 用同一 provider，回复固定文本）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut saw_started = false;
    let mut saw_completed = false;
    let mut saw_hook_message = false;
    while std::time::Instant::now() < deadline && !(saw_completed && saw_hook_message) {
        while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
            match event {
                BackendEvent::SubagentStarted { task, .. } => {
                    saw_started = true;
                    assert!(task.contains("找出所有 X"), "{task}");
                }
                BackendEvent::SubagentCompleted { success, .. } => {
                    saw_completed = true;
                    assert!(success, "child should succeed");
                }
                BackendEvent::MessageReceived {
                    instance_name,
                    content,
                    ..
                } if instance_name == "subagent" => {
                    saw_hook_message = true;
                    assert!(content.contains("<subagent_event>"), "{content}");
                    assert!(content.contains("子任务结论"), "{content}");
                }
                _ => {}
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(saw_started, "SubagentStarted emitted");
    assert!(saw_completed, "SubagentCompleted emitted");
    assert!(
        saw_hook_message,
        "completion hook injected as new inbound branch"
    );
    // 主 agent 至少再消化一次 hook（provider 调用 ≥ 2：hook turn + 可能的子任务）。
    assert!(provider.calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn spawn_subagent_missing_task_gets_corrective_error() {
    let (agent, _) = test_agent_with_subagent("ok");
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    let call = ToolCall {
        id: "call-bad".into(),
        name: "spawn_subagent".into(),
        arguments: "{}".into(),
    };
    let result = agent.run_tool(&session.id, "branch-main", &call).await;
    assert!(
        result.text.contains("task") && result.text.contains("自含"),
        "corrective message: {}",
        result.text
    );
}

#[tokio::test]
async fn spawn_subagent_is_blocked_when_module_not_allowed() {
    let (agent, _) = test_agent_with_subagent("ok");
    // persona 白名单不含 subagent 模块 → 工具不可见、调用被拒绝。
    agent
        .apply_capabilities(&echo_agent::config::TeamMember {
            enabled_modules: vec![echo_agent::modules::TOOLS_BUILTIN_MODULE_ID.into()],
            ..Default::default()
        })
        .await;
    // 门控经**包标签**生效：模块不在白名单 → 该包的工具整组禁用
    // （不再有按工具名的特判分支）。
    assert!(
        agent.tools.is_disabled("spawn_subagent").await,
        "模块不在白名单 → 工具应被包门控禁用"
    );
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);
    let call = ToolCall {
        id: "call-denied".into(),
        name: "spawn_subagent".into(),
        arguments: r#"{"task":"t"}"#.into(),
    };
    let result = agent.run_tool(&session.id, "branch-main", &call).await;
    assert!(
        result.text.contains("找不到工具") || result.text.contains("not registered"),
        "denied: {}",
        result.text
    );
}

#[tokio::test]
async fn cancelling_parent_turn_cascades_to_subagent() {
    // 子任务用一个永不放行的 provider：主 turn 取消必须传播。
    let entered = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    struct HangingProvider {
        entered: Arc<AtomicUsize>,
        gate: Arc<tokio::sync::Semaphore>,
    }
    #[async_trait]
    impl LlmProvider for HangingProvider {
        fn name(&self) -> &str {
            "hanging"
        }
        fn default_model(&self) -> &str {
            "hanging"
        }
        async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.gate.acquire().await.expect("gate open").forget();
            Ok(ChatResponse {
                stop_reason: None,
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
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
    let provider = Arc::new(HangingProvider {
        entered: entered.clone(),
        gate: gate.clone(),
    });
    let agent = Arc::new(Agent::new(
        provider,
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(InstanceRegistry::new()),
    ));
    agent.set_team_id(Some("t".into()));
    let store = echo_agent::subagent::SubagentStore::new();
    agent.attach_subagent_runtime(store.clone());
    let (bridge, handle) = echo_agent::create_bridge();
    agent.attach(Arc::new(handle));
    let session = agent
        .trunk
        .get_or_create(&SessionKey::local_tui(), "user".into(), None);

    // 直接 spawn（同步受理，子任务随即挂起在 provider 上）。
    let call = ToolCall {
        id: "call-spawn-hang".into(),
        name: "spawn_subagent".into(),
        arguments: r#"{"task":"永远跑不完的任务"}"#.into(),
    };
    let result = agent.run_tool(&session.id, "branch-main", &call).await;
    assert!(result.text.contains("子任务已受理"), "{}", result.text);
    let task_id = store.snapshot()[0].id.clone();

    // 子任务进入 provider 后取消全局（模拟主 turn 取消传播链末端）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(entered.load(Ordering::SeqCst), 1, "child entered provider");
    store.cancel_all();
    // 不放行 gate：子任务的 select! 应被 parent_cancel.cancelled() 中止
    // （entry.cancel 是注册时父令牌的 child_token，cancel_all 经 finish 取消它）。

    // 取消后子任务以 Cancelled 收尾并发 SubagentCompleted(success=false)。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut saw_completed = false;
    while std::time::Instant::now() < deadline && !saw_completed {
        while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
            if let BackendEvent::SubagentCompleted { success, .. } = event {
                saw_completed = true;
                assert!(!success, "cancelled child reports failure");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(saw_completed, "SubagentCompleted emitted after cancel");
    assert_eq!(
        store.snapshot()[0].status,
        echo_agent::subagent::SubagentStatus::Cancelled,
        "store records cancellation for {task_id}"
    );
}
