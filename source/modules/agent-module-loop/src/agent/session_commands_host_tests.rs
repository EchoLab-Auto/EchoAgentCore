use super::*;
use crate::agent::registry::AgentRegistry;
use crate::agent::tests::MockProvider;
use agent_module_management::protocol::{BackendCommand, TeamInfo};
use echo_session::{SessionEvent, UserMessage};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    RwLock,
};

struct ChangingRegistry {
    current: RwLock<Arc<Agent>>,
    calls: AtomicUsize,
}
impl AgentRegistry for ChangingRegistry {
    fn resolve(&self, id: &str) -> Option<Arc<Agent>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let agent = self.current.read().unwrap().clone();
        (agent.team_id().as_deref() == Some(id)).then_some(agent)
    }
    fn all(&self) -> Vec<crate::agent_manager::RunningAgent> {
        vec![]
    }
    fn infos(&self) -> Vec<TeamInfo> {
        vec![]
    }
}
fn agent(id: &str) -> Arc<Agent> {
    let a = Arc::new(Agent::new(
        Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "summary".into(),
        }),
        crate::config::AgentConfig::default(),
        crate::SkillRegistry::new(),
        crate::tool::ToolRegistry::new(),
        Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
    ));
    a.set_team_id(Some(id.into()));
    a
}
fn feed(agent: &Agent, sid: &str) {
    agent
        .trunk
        .append_event(SessionEvent::UserMessage(UserMessage {
            content: "synthetic-history".into(),
            timestamp: 1,
            message_sequence: None,
            source: None,
            images: vec![],
            files: vec![],
            session: Some(sid.into()),
        }));
}
fn cache(a: &Agent, sid: &str) {
    let provider = Arc::new(MockProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "ok".into(),
    });
    a.model
        .session_providers
        .insert(sid.into(), ("override".into(), provider));
    a.model.session_api.set(sid, "override");
}

#[tokio::test]
async fn bound_command_keeps_one_generation_across_registry_replacement() {
    let representative = agent("a");
    let old = agent("b");
    let new = agent("b");
    let registry = Arc::new(ChangingRegistry {
        current: RwLock::new(old.clone()),
        calls: AtomicUsize::new(0),
    });
    let dyn_registry: Arc<dyn AgentRegistry> = registry.clone();
    representative.set_registry(&dyn_registry);
    let key = crate::session::SessionKey::local_tui();
    let old_session = old.trunk.get_or_create(&key, "old".into(), None);
    let new_session = new.trunk.get_or_create(&key, "new".into(), None);
    let sid = &old_session.id;
    feed(&old, sid);
    feed(&new, sid);
    cache(&old, sid);
    cache(&new, sid);
    cache(&representative, sid);
    let old_turn = old.try_register_inbound_turn(sid, 1).unwrap();
    let new_turn = new.try_register_inbound_turn(sid, 2).unwrap();
    let host = Arc::new(AgentSessionCommandHost::new(representative.clone()));
    let bound = host.bind_target(Some("b")).unwrap();
    *registry.current.write().unwrap() = new.clone();
    let guard = bound.quiesce_session(Some("b"), sid).await.unwrap();
    assert!(old_turn.cancel.is_cancelled());
    assert!(!new_turn.cancel.is_cancelled());
    // The bound trunk shares the old history Arc, not the replacement's.
    let trunk = bound.trunk();
    assert!(Arc::ptr_eq(
        &trunk.history_for(sid).unwrap(),
        &old_session.history
    ));
    assert!(!Arc::ptr_eq(
        &trunk.history_for(sid).unwrap(),
        &new_session.history
    ));
    trunk.clear_session(sid).await.unwrap();
    bound.forget_session_provider(sid);
    assert!(old.trunk.events_for_session(sid).is_empty());
    assert!(!new.trunk.events_for_session(sid).is_empty());
    assert!(!old.model.session_providers.contains_key(sid));
    assert!(old.model.session_api.get(sid).is_none());
    assert!(new.model.session_providers.contains_key(sid));
    assert!(representative.model.session_providers.contains_key(sid));
    // Snapshot and compression on the same handle must not re-resolve either.
    bound.emit_context_snapshot(sid).await;
    assert!(bound.compact(Some("b".into()), sid, 40).await.is_err());
    assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
    drop(guard);
    old.finish_inbound_turn(&old_turn.id);
    new.finish_inbound_turn(&new_turn.id);
}

#[tokio::test]
async fn clear_command_clears_only_target_cached_and_persisted_override() {
    let representative = agent("a");
    let target = agent("b");
    let registry = Arc::new(ChangingRegistry {
        current: RwLock::new(target.clone()),
        calls: AtomicUsize::new(0),
    });
    let dyn_registry: Arc<dyn AgentRegistry> = registry.clone();
    representative.set_registry(&dyn_registry);
    let dir = std::env::temp_dir().join(format!(
        "echo-bound-override-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("overrides.json");
    target.model.session_api.attach(&path);
    let session = target.trunk.get_or_create(
        &crate::session::SessionKey::local_tui(),
        "target".into(),
        None,
    );
    let sid = &session.id;
    feed(&target, sid);
    feed(&representative, sid);
    cache(&target, sid);
    cache(&representative, sid);
    let host = Arc::new(AgentSessionCommandHost::new(representative.clone()));
    agent_module_session::commands::SessionCommandHandler::new(host)
        .apply_command(BackendCommand::ClearHistory {
            team_id: Some("b".into()),
            session_id: Some(sid.clone()),
        })
        .await
        .unwrap();
    assert!(target.trunk.events_for_session(sid).is_empty());
    assert!(!representative.trunk.events_for_session(sid).is_empty());
    assert!(!target.model.session_providers.contains_key(sid));
    assert!(target.model.session_api.get(sid).is_none());
    assert!(
        crate::agent::session_api_store::SessionApiStore::open(&path)
            .get(sid)
            .is_none()
    );
    assert!(representative.model.session_providers.contains_key(sid));
    assert!(representative.model.session_api.get(sid).is_some());
    assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

/// 注册表第一次返回旧实例后立即替换；覆盖实际命令路径，而非只测宿主方法。
#[tokio::test]
async fn clear_pipeline_does_not_rebind_after_target_resolution() {
    struct SwapOnResolve {
        old: Arc<Agent>,
        new: Arc<Agent>,
        calls: AtomicUsize,
    }
    impl AgentRegistry for SwapOnResolve {
        fn resolve(&self, id: &str) -> Option<Arc<Agent>> {
            if id != "b" {
                return None;
            }
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Some(if n == 0 {
                self.old.clone()
            } else {
                self.new.clone()
            })
        }
        fn all(&self) -> Vec<crate::agent_manager::RunningAgent> {
            vec![]
        }
        fn infos(&self) -> Vec<TeamInfo> {
            vec![]
        }
    }
    let representative = agent("a");
    let old = agent("b");
    let new = agent("b");
    let registry = Arc::new(SwapOnResolve {
        old: old.clone(),
        new: new.clone(),
        calls: AtomicUsize::new(0),
    });
    let dyn_registry: Arc<dyn AgentRegistry> = registry.clone();
    representative.set_registry(&dyn_registry);
    let key = crate::session::SessionKey::local_tui();
    let sid = old.trunk.get_or_create(&key, "old".into(), None).id;
    new.trunk.get_or_create(&key, "new".into(), None);
    feed(&old, &sid);
    feed(&new, &sid);
    cache(&old, &sid);
    cache(&new, &sid);
    let old_turn = old.try_register_inbound_turn(&sid, 1).unwrap();
    let new_turn = new.try_register_inbound_turn(&sid, 2).unwrap();
    let host = Arc::new(AgentSessionCommandHost::new(representative));
    agent_module_session::commands::SessionCommandHandler::new(host)
        .apply_command(BackendCommand::ClearHistory {
            team_id: Some("b".into()),
            session_id: Some(sid.clone()),
        })
        .await
        .unwrap();
    assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
    assert!(old_turn.cancel.is_cancelled());
    assert!(!new_turn.cancel.is_cancelled());
    assert!(old.trunk.events_for_session(&sid).is_empty());
    assert!(!new.trunk.events_for_session(&sid).is_empty());
    assert!(!old.model.session_providers.contains_key(&sid));
    assert!(new.model.session_providers.contains_key(&sid));
    old.finish_inbound_turn(&old_turn.id);
    new.finish_inbound_turn(&new_turn.id);
}
