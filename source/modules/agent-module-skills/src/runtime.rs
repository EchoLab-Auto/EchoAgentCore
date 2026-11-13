//! 技能注册表与命令/提示词生命周期，全部归 skills 模块。

use crate::{commands::SkillsCommandHost, SkillRegistry};
use std::sync::Arc;

pub struct SkillsRuntime {
    pub registry: Arc<tokio::sync::Mutex<SkillRegistry>>,
    host: Arc<dyn SkillsCommandHost>,
    pub prompt: Arc<crate::prompt::SkillPromptRefresher>,
}

impl SkillsRuntime {
    pub fn new(
        host: Arc<dyn SkillsCommandHost>,
        store: echo_defs::prompt::SharedPromptStore,
    ) -> Arc<Self> {
        Self::with_registry(
            host,
            store,
            Arc::new(tokio::sync::Mutex::new(SkillRegistry::new())),
        )
    }

    pub fn with_registry(
        host: Arc<dyn SkillsCommandHost>,
        store: echo_defs::prompt::SharedPromptStore,
        registry: Arc<tokio::sync::Mutex<SkillRegistry>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            prompt: Arc::new(crate::prompt::SkillPromptRefresher {
                registry: registry.clone(),
                store,
            }),
            registry,
            host,
        })
    }

    /// 重载：文件层重新发现 + 模块贡献层重新拉取。
    ///
    /// 返回 `true` = 注册表确实变了（调用方据此决定是否广播列表刷新）。
    pub async fn reload(&self) -> Result<bool, String> {
        // 贡献来源与文件目录**互相独立**：只有贡献、没有目录的宿主（或反过
        // 来）都是合法配置，因此不能因为"目录不存在"就早退掉贡献同步。
        let contributions = self.host.contributed_skills();
        let write_dir = self.host.skills_dir();
        let mut dirs = self.host.skill_dirs();
        dirs.push(write_dir.clone());
        let has_dirs = dirs
            .iter()
            .any(|d| !d.is_empty() && std::path::Path::new(d).exists());
        let mut reloaded = if has_dirs {
            tokio::task::spawn_blocking(move || SkillRegistry::discover_many(&dirs))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?
        } else {
            SkillRegistry::new()
        };
        let mut current = self.registry.lock().await;
        reloaded.inherit_enabled_from(&current);
        // 本轮没再出现的 owner = 来源已经没了（模块卸载 / 被禁 / 启动失败），
        // 其技能一并撤销。这是「模块不在了技能就该消失」的唯一落地点。
        let reported: std::collections::BTreeSet<&str> =
            contributions.iter().map(|(owner, _)| owner.as_str()).collect();
        for owner in current.contributed_owners() {
            if !reported.contains(owner.as_str()) {
                reloaded.remove_contributed(&owner);
            }
        }
        for (owner, skills) in contributions {
            reloaded.set_contributed(&owner, skills);
        }
        if *current == reloaded {
            return Ok(false);
        }
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

    pub async fn apply_capabilities(
        &self,
        enabled: Vec<String>,
        disabled: Vec<String>,
        global: Vec<String>,
    ) {
        let mut skills = self.registry.lock().await;
        for name in skills.names() {
            let allowed = (enabled.is_empty() || enabled.contains(&name))
                && !disabled.contains(&name)
                && !global.contains(&name);
            skills.set_enabled(&name, allowed);
        }
        drop(skills);
        self.prompt.store.invalidate();
    }
}

#[async_trait::async_trait]
impl echo_defs::prompt::NamedCapabilityController for SkillsRuntime {
    async fn apply(
        &self,
        enabled: Vec<String>,
        disabled: Vec<String>,
        global_disabled: Vec<String>,
    ) {
        self.apply_capabilities(enabled, disabled, global_disabled)
            .await;
    }
    async fn reload(&self) -> Result<bool, String> {
        SkillsRuntime::reload(self).await
    }
    async fn set_enabled(&self, name: &str, enabled: bool) -> bool {
        SkillsRuntime::set_enabled(self, name, enabled).await
    }
    fn set_package_enabled(&self, package: &str, enabled: bool) {
        if let Ok(mut skills) = self.registry.try_lock() {
            skills.set_package_enabled(package, enabled);
        }
        self.prompt.store.invalidate();
    }
    async fn emit_list(&self) {
        let skills = self.registry.lock().await;
        let list =
            crate::commands::skills_list(&skills, std::path::Path::new(&self.host.skills_dir()));
        drop(skills);
        self.host
            .emit(agent_module_management::protocol::BackendEvent::SkillsList { skills: list });
    }
    fn packages(&self) -> Vec<String> {
        self.registry
            .try_lock()
            .map(|s| s.packages())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod contribution_tests {
    use super::*;
    use crate::commands::SkillsCommandHost;
    use crate::prompt::TestPromptStore;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// 可变贡献源的宿主替身：`contributions` 可随时改，模拟模块启停。
    struct Host {
        dir: String,
        contributions: Mutex<Vec<(String, Vec<crate::Skill>)>>,
        applies: AtomicUsize,
    }

    impl Host {
        fn new(contributions: Vec<(String, Vec<crate::Skill>)>) -> Arc<Self> {
            Arc::new(Self {
                dir: String::new(),
                contributions: Mutex::new(contributions),
                applies: AtomicUsize::new(0),
            })
        }
        fn set(mut self: Arc<Self>, contributions: Vec<(String, Vec<crate::Skill>)>) {
            *self.contributions.lock().unwrap() = contributions;
        }
    }

    impl SkillsCommandHost for Host {
        fn skills_dir(&self) -> String {
            self.dir.clone()
        }
        fn emit(&self, _: agent_module_management::protocol::BackendEvent) {}
        fn apply_capabilities(
            &self,
            _: Vec<String>,
            _: Vec<String>,
            _: Vec<String>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            Box::pin(async {
                self.applies.fetch_add(1, Ordering::SeqCst);
            })
        }
        fn persist_enabled(
            &self,
            _: String,
            _: bool,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }
        fn global_disabled(&self) -> Vec<String> {
            Vec::new()
        }
        fn attach_prompt_pipeline(
            &self,
            _: echo_defs::prompt::SharedPromptRefresher,
            _: Arc<dyn echo_defs::prompt::NamedCapabilityController>,
        ) {
        }
        fn contributed_skills(&self) -> Vec<(String, Vec<crate::Skill>)> {
            self.contributions.lock().unwrap().clone()
        }
    }

    fn skill(name: &str) -> crate::Skill {
        crate::Skill::direct(name, "desc", Vec::new(), false, "test", "正文")
    }

    fn runtime(host: Arc<Host>) -> Arc<SkillsRuntime> {
        SkillsRuntime::new(host, Arc::new(TestPromptStore))
    }

    #[tokio::test]
    async fn reload_registers_module_contributions() {
        let host = Host::new(vec![("echo-agent.adapter.qq".into(), vec![skill("qq-transport")])]);
        let rt = runtime(host);
        assert!(rt.reload().await.unwrap());
        let registry = rt.registry.lock().await;
        assert!(registry.get("qq-transport").is_some(), "模块贡献的技能必须进表");
        assert_eq!(registry.contributed_owners(), vec!["echo-agent.adapter.qq"]);
    }

    #[tokio::test]
    async fn owner_that_stops_reporting_loses_its_skills() {
        let host = Host::new(vec![("m1".into(), vec![skill("a"), skill("b")])]);
        let rt = runtime(host.clone());
        rt.reload().await.unwrap();
        assert_eq!(rt.registry.lock().await.len(), 2);

        // 模块卸载：这一轮不再上报 → 技能必须消失，而不是留着教一个已经不存在的工具
        host.clone().set(Vec::new());
        assert!(rt.reload().await.unwrap());
        let registry = rt.registry.lock().await;
        assert_eq!(registry.len(), 0, "来源消失的技能必须撤销");
        assert!(registry.contributed_owners().is_empty());
    }

    #[tokio::test]
    async fn owner_replacement_is_not_an_append() {
        let host = Host::new(vec![("m1".into(), vec![skill("a"), skill("b")])]);
        let rt = runtime(host.clone());
        rt.reload().await.unwrap();

        // 模块升级后只带一个技能：旧的另一个必须走
        host.clone().set(vec![("m1".into(), vec![skill("a")])]);
        rt.reload().await.unwrap();
        let registry = rt.registry.lock().await;
        assert!(registry.get("a").is_some());
        assert!(registry.get("b").is_none(), "set_contributed 是替换而不是追加");
    }

    #[tokio::test]
    async fn contributions_survive_a_directory_reload() {
        // 目录里没有技能（`dir` 为空）也必须有贡献：两层互相独立
        let host = Host::new(vec![("m1".into(), vec![skill("a")])]);
        let rt = runtime(host);
        rt.reload().await.unwrap();
        // 再重载一次（模拟面板点「重载技能」）：不能把贡献冲掉
        rt.reload().await.unwrap();
        assert!(rt.registry.lock().await.get("a").is_some());
    }
}
