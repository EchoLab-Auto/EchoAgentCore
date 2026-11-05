//! Host-owned registry connecting persona-scoped tools to live session stores.
//!
//! Tools are built before `Agent`, so this registry is shared with the factory
//! and populated immediately after each agent is constructed. `TrunkStore` is
//! itself a cheap handle to shared live state, not the persisted JSON document.

use dashmap::DashMap;
use serde_json::Value;

use echo_defs::session_query::{SessionQuery, SessionQueryView};
use echo_module_session::store::query;
use echo_module_session::TrunkStore;

#[derive(Default)]
pub struct SessionQueryRegistry {
    stores: DashMap<String, TrunkStore>,
}

impl SessionQueryRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, team_id: impl Into<String>, trunk: &TrunkStore) {
        self.stores.insert(team_id.into(), trunk.clone());
    }

    fn resolve(&self, team_id: Option<&str>) -> Result<TrunkStore, String> {
        let key = team_id.filter(|id| !id.is_empty()).unwrap_or("default");
        self.stores
            .get(key)
            .map(|store| store.clone())
            .ok_or_else(|| format!("人格 {key} 的会话存储不存在"))
    }
}

impl SessionQuery for SessionQueryRegistry {
    fn list_sessions(&self, team_id: Option<&str>, limit: usize) -> Result<Value, String> {
        let trunk = self.resolve(team_id)?;
        Ok(query::list_sessions(&trunk, limit))
    }

    fn read_session(
        &self,
        team_id: Option<&str>,
        session_id: &str,
        view: SessionQueryView,
        limit: usize,
        include_tool_results: bool,
    ) -> Result<Value, String> {
        let trunk = self.resolve(team_id)?;
        query::read_session(&trunk, session_id, view, limit, include_tool_results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_persona_scoped() {
        let registry = SessionQueryRegistry::new();
        let alice = TrunkStore::new(1000);
        let bob = TrunkStore::new(1000);
        registry.register("alice", &alice);
        registry.register("bob", &bob);
        assert_eq!(
            registry.list_sessions(Some("alice"), 50).unwrap()["total"],
            0
        );
        assert!(registry
            .read_session(Some("alice"), "missing", SessionQueryView::Audit, 50, false)
            .is_err());
        assert!(registry.list_sessions(Some("bob"), 50).is_ok());
        assert!(registry.list_sessions(Some("nobody"), 50).is_err());
    }
}
