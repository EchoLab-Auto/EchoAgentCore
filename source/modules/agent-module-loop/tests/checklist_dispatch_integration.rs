//! Checklist crosses the actual Agent tool-dispatch boundary, not just store methods.
use std::sync::{Arc, Mutex};

use agent_module_loop::{
    agent::{builder::AgentBuilder, Agent},
    config::AgentConfig,
    SkillRegistry,
};
use agent_module_management::protocol::BackendEvent;
use agent_module_checklist::{all_state_event_bindings, ChecklistStore, ChecklistTool};
use agent_module_tools::ToolRegistry;
use async_trait::async_trait;
use echo_defs::{
    llm::{LlmError, LlmProvider},
    message::{ChatChunk, ChatRequest, ChatResponse, ToolCall},
    sanitize::Redactor,
};
use echo_test_utils::FakeRedactor;
use serde_json::{json, Value};

struct NeverCalled;
#[async_trait]
impl LlmProvider for NeverCalled {
    fn name(&self) -> &str {
        "checklist-test"
    }
    fn default_model(&self) -> &str {
        "none"
    }
    async fn chat(&self, _: &ChatRequest) -> Result<ChatResponse, LlmError> {
        panic!("tool-dispatch test must not call a provider")
    }
    async fn chat_stream(
        &self,
        _: &ChatRequest,
        _: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError> {
        panic!("tool-dispatch test must not call a provider")
    }
}

fn agent(
    store: Arc<ChecklistStore>,
    redactor: Arc<dyn Redactor>,
    events: Arc<Mutex<Vec<BackendEvent>>>,
) -> Arc<Agent> {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ChecklistTool::with_store(store)));
    tools.set_redactor(Some(redactor.clone()));
    let agent = AgentBuilder::new(
        Arc::new(NeverCalled),
        AgentConfig::default(),
        SkillRegistry::new(),
        tools,
        Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
    )
    .module_host(Arc::new(agent_module_loop::modules::ModuleHost::new()))
    .event_sink(Arc::new(move |event| events.lock().unwrap().push(event)))
    .team_id(Some("alice".into()))
    .redactor(redactor)
    .build();
    for (key, builder) in all_state_event_bindings() {
        agent.register_tool_state_event(key, builder);
    }
    agent
}

#[tokio::test]
async fn secret_free_events_and_state_survive_agent_and_store_rebuild() {
    const SECRET: &str = "fake-checklist-secret-317a";
    let root = std::env::temp_dir().join(format!("checklist-dispatch-{}", uuid::Uuid::new_v4()));
    let path = root.join("alice.json");
    let redactor = Arc::new(FakeRedactor::new());
    redactor.register_secret(SECRET, "checklist-test");
    let store = Arc::new(ChecklistStore::open(path.clone(), Some(redactor.clone())).unwrap());
    let events = Arc::new(Mutex::new(Vec::new()));
    let first = agent(store.clone(), redactor.clone(), events.clone());
    let result = first.run_tool("session-a", "", &ToolCall {
        id: "create-a".into(), name: "checklist".into(),
        arguments: json!({"operation":"create", "name":format!("plan-{SECRET}"), "items":[format!("verify {SECRET}")], "request_id":"create-a"}).to_string(),
    }).await;
    assert!(!result.text.starts_with("error:"), "{}", result.text);
    assert!(!result.text.contains(SECRET));
    let before = store.snapshot("session-a").unwrap();
    let emitted = events.lock().unwrap();
    let state = emitted
        .iter()
        .find_map(|event| match event {
            BackendEvent::ChecklistUpdated {
                session_id,
                team_id,
                state,
            } if session_id == "session-a" => {
                assert_eq!(team_id.as_deref(), Some("alice"));
                Some(state)
            }
            _ => None,
        })
        .expect("actual dispatch must emit a checklist snapshot");
    assert_eq!(state, &before);
    assert!(!serde_json::to_string(&*emitted).unwrap().contains(SECRET));
    drop(emitted);
    assert!(!std::fs::read_to_string(&path).unwrap().contains(SECRET));
    assert!(!store.summary("session-a").unwrap().contains(SECRET));
    assert_eq!(store.snapshot("session-b").unwrap()["lists"], json!([]));

    drop(first);
    let second = agent(store.clone(), redactor.clone(), events.clone());
    let result = second
        .run_tool(
            "session-a",
            "",
            &ToolCall {
                id: "list-a".into(),
                name: "checklist".into(),
                arguments: json!({"operation":"list"}).to_string(),
            },
        )
        .await;
    assert!(!result.text.starts_with("error:"), "{}", result.text);
    assert_eq!(
        store.snapshot("session-a").unwrap(),
        before,
        "reads after agent rebuild must not mutate state"
    );
    drop(second);
    drop(store);
    let reopened = ChecklistStore::open(path, Some(redactor)).unwrap();
    let after: Value = reopened.snapshot("session-a").unwrap();
    assert_eq!(
        after, before,
        "IDs, epoch and revision must round-trip through disk"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn history_clear_preserves_checklist_and_explicit_reset_only_clears_target_session() {
    use agent_module_loop::agent::session_commands_host::AgentSessionCommandHost;
    use agent_module_management::protocol::BackendCommand;
    use agent_module_session::commands::SessionCommandHandler;
    let store = Arc::new(ChecklistStore::memory());
    let events = Arc::new(Mutex::new(Vec::new()));
    let instance = agent(store.clone(), Arc::new(FakeRedactor::new()), events);
    store
        .apply(
            "session-a",
            json!({"operation":"create", "name":"work", "items":["pending"]}),
        )
        .await
        .unwrap();
    store
        .apply(
            "session-b",
            json!({"operation":"create", "name":"work", "items":["sibling"]}),
        )
        .await
        .unwrap();
    let before = store.snapshot("session-a").unwrap();
    let sibling = store.snapshot("session-b").unwrap()["lists"].clone();
    SessionCommandHandler::new(Arc::new(AgentSessionCommandHost::new(instance)))
        .apply_command(BackendCommand::ClearHistory {
            team_id: Some("alice".into()),
            session_id: Some("session-a".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        store.snapshot("session-a").unwrap(),
        before,
        "history cleanup must not implicitly reset task state"
    );
    store.apply("session-a", json!({"operation":"reset_session", "expected_epoch":before["epoch"], "expected_revision":before["revision"]})).await.unwrap();
    assert_eq!(store.snapshot("session-a").unwrap()["lists"], json!([]));
    assert_eq!(store.snapshot("session-b").unwrap()["lists"], sibling);
}

#[tokio::test]
async fn acknowledged_request_is_not_reapplied_after_store_reopen() {
    let root = std::env::temp_dir().join(format!("checklist-retry-{}", uuid::Uuid::new_v4()));
    let path = root.join("alice.json");
    let store = ChecklistStore::open(path.clone(), None).unwrap();
    let created: Value = serde_json::from_str(
        &store
            .apply(
                "s",
                json!({"operation":"create", "name":"work", "items":["first"]}),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let list_id = created["list"]["id"].clone();
    let request = json!({"operation":"add_items", "list_id":list_id, "items":["once"], "request_id":"durable-request-1"});
    let first = store.apply("s", request.clone()).await.unwrap();
    let before = store.snapshot("s").unwrap();
    drop(store);
    let reopened = ChecklistStore::open(path, None).unwrap();
    let retried = reopened.apply("s", request).await.unwrap();
    assert_eq!(
        retried, first,
        "a durable idempotency record must replay the acknowledged result"
    );
    assert_eq!(
        reopened.snapshot("s").unwrap(),
        before,
        "a reconnect/restart retry must not append twice"
    );
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn persona_file_names_do_not_alias_when_ids_contain_suffix_or_whitespace() {
    use agent_module_checklist::ChecklistRuntime;
    let root = std::env::temp_dir().join(format!("checklist-names-{}", uuid::Uuid::new_v4()));
    let runtime = ChecklistRuntime::new(root.clone(), None);
    let a = runtime.store_for("a").unwrap();
    let b = runtime.store_for("a ").unwrap();
    assert_ne!(
        a.path(),
        b.path(),
        "distinct persona IDs must not resolve to the same file"
    );
    a.apply(
        "s",
        json!({"operation":"create", "name":"work", "items":["a"]}),
    )
    .await
    .unwrap();
    b.apply(
        "s",
        json!({"operation":"create", "name":"work", "items":["b"]}),
    )
    .await
    .unwrap();
    drop(runtime);
    drop(a);
    drop(b);
    let reopened = ChecklistRuntime::new(root.clone(), None);
    assert_eq!(
        reopened.store_for("a").unwrap().snapshot("s").unwrap()["lists"][0]["items"][0]["text"],
        "a"
    );
    assert_eq!(
        reopened.store_for("a ").unwrap().snapshot("s").unwrap()["lists"][0]["items"][0]["text"],
        "b"
    );
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn model_summary_contains_stable_ids_and_current_version() {
    let store = ChecklistStore::memory();
    store
        .apply(
            "s",
            json!({"operation":"create", "name":"work", "items":["pending"]}),
        )
        .await
        .unwrap();
    let state = store.snapshot("s").unwrap();
    let summary = store.summary("s").unwrap();
    for id in [
        &state["epoch"],
        &state["lists"][0]["id"],
        &state["lists"][0]["items"][0]["id"],
    ] {
        assert!(
            summary.contains(id.as_str().unwrap()),
            "summary must provide actionable identity: {summary}"
        );
    }
    assert!(summary.contains("revision"));
}
