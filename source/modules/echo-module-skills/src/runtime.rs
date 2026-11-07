//! 技能注册表与命令/提示词生命周期，全部归 skills 模块。

use std::sync::Arc;
use crate::{SkillRegistry, commands::SkillsCommandHost};

pub struct SkillsRuntime {
    pub registry: Arc<tokio::sync::Mutex<SkillRegistry>>,
    host: Arc<dyn SkillsCommandHost>,
    pub prompt: Arc<crate::prompt::SkillPromptRefresher>,
}

impl SkillsRuntime {
    pub fn new(host: Arc<dyn SkillsCommandHost>, store: echo_defs::prompt::SharedPromptStore) -> Arc<Self> {
        Self::with_registry(host, store, Arc::new(tokio::sync::Mutex::new(SkillRegistry::new())))
    }

    pub fn with_registry(host: Arc<dyn SkillsCommandHost>, store: echo_defs::prompt::SharedPromptStore,
        registry: Arc<tokio::sync::Mutex<SkillRegistry>>) -> Arc<Self> {
        Arc::new(Self {
            prompt: Arc::new(crate::prompt::SkillPromptRefresher { registry: registry.clone(), store }),
            registry, host,
        })
    }

    pub async fn reload(&self) -> Result<bool, String> {
        let write_dir = self.host.skills_dir();
        let mut dirs = self.host.skill_dirs();
        dirs.push(write_dir.clone());
        if dirs.iter().all(|d| d.is_empty() || !std::path::Path::new(d).exists()) { return Ok(false); }
        let mut reloaded = tokio::task::spawn_blocking(move || SkillRegistry::discover_many(&dirs))
            .await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        let mut current = self.registry.lock().await;
        reloaded.inherit_enabled_from(&current);
        if *current == reloaded { return Ok(false); }
        *current = reloaded;
        drop(current);
        self.prompt.store.replace("skills", Vec::new());
        self.prompt.store.invalidate();
        Ok(true)
    }

    pub async fn set_enabled(&self, name: &str, enabled: bool) -> bool {
        let ok = self.registry.lock().await.set_enabled(name, enabled);
        if ok {
            self.host.persist_enabled(name.to_string(), enabled).await;
            self.prompt.store.invalidate();
        }
        ok
    }

    pub async fn apply_capabilities(&self, enabled: Vec<String>, disabled: Vec<String>, global: Vec<String>) {
        let mut skills = self.registry.lock().await;
        for name in skills.names() {
            let allowed = (enabled.is_empty() || enabled.contains(&name))
                && !disabled.contains(&name) && !global.contains(&name);
            skills.set_enabled(&name, allowed);
        }
        drop(skills);
        self.prompt.store.invalidate();
    }
}

#[async_trait::async_trait]
impl echo_defs::prompt::NamedCapabilityController for SkillsRuntime {
    async fn apply(&self, enabled: Vec<String>, disabled: Vec<String>, global_disabled: Vec<String>) {
        self.apply_capabilities(enabled, disabled, global_disabled).await;
    }
    async fn reload(&self) -> Result<bool, String> { SkillsRuntime::reload(self).await }
    async fn set_enabled(&self, name: &str, enabled: bool) -> bool { SkillsRuntime::set_enabled(self, name, enabled).await }
    fn set_package_enabled(&self, package: &str, enabled: bool) {
        if let Ok(mut skills) = self.registry.try_lock() { skills.set_package_enabled(package, enabled); }
        self.prompt.store.invalidate();
    }
    async fn emit_list(&self) {
        let skills = self.registry.lock().await;
        let list = crate::commands::skills_list(&skills, std::path::Path::new(&self.host.skills_dir()));
        drop(skills);
        self.host.emit(echo_module_management::protocol::BackendEvent::SkillsList { skills: list });
    }
    fn packages(&self) -> Vec<String> {
        self.registry.try_lock().map(|s| s.packages()).unwrap_or_default()
    }
}
