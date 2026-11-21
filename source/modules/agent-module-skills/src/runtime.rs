//! 技能注册表与命令/提示词生命周期，全部归 skills 模块。
//!
//! ## 两个方向相反的来源
//!
//! | 方向 | 来源 | 谁说话 |
//! | --- | --- | --- |
//! | **拉** | `Host::contributed_skills()` | 宿主（进程外模块的 `Register` 帧） |
//! | **推** | [`echo_defs::skill::SkillRegistrar`] | 模块自己（在 `mount` 里） |
//!
//! 两者在 [`SkillsRuntime::reload`] 里合并成同一张贡献表：`owner` 是归属键，
//! 撤销语义（本轮没上报就撤）对两者一致。推的那张由本模块自己持有
//! （`registered`）——登记发生在**模块的** mount 期，那时还没有任何宿主句柄
//! 能承载它。

use crate::{commands::SkillsCommandHost, SkillRegistry};
use std::sync::Arc;

pub struct SkillsRuntime {
    pub registry: Arc<tokio::sync::Mutex<SkillRegistry>>,
    host: Arc<dyn SkillsCommandHost>,
    pub prompt: Arc<crate::prompt::SkillPromptRefresher>,
    /// 推模型登记的技能：`owner` → 已解析的技能集。
    ///
    /// 与 `registry`（合并后的真值）分开存：`reload` 每次重建 `registry`，
    /// 这张表是**输入**，不能跟着重建——否则每次 reload 都会把自己刚登记的
    /// 那批冲掉。
    registered: std::sync::RwLock<std::collections::BTreeMap<String, Vec<crate::Skill>>>,
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
            registered: std::sync::RwLock::new(std::collections::BTreeMap::new()),
        })
    }

    /// 推模型登记的当前快照（`owner` → 技能集）。
    ///
    /// 返回克隆而不是借用：`reload` 拿它当输入时要跨 `await`，借用守不住。
    fn registered_snapshot(&self) -> Vec<(String, Vec<crate::Skill>)> {
        self.registered
            .read()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// 后台重载一次，失败只记 warn（调用方是生命周期钩子，没有错误出口）。
    ///
    /// 为什么登记后必须主动重载：模块的 `mount` 与本模块的初次扫描**顺序
    /// 不固定**（前者按服务依赖拓扑排序，后者由本模块 `mount` 时 spawn）。
    /// 只把新表存起来的话，"技能要等下一次无关重载才出现"——提示词里少
    /// 一整段规则的静默故障。
    pub fn reload_now(self: &Arc<Self>, reason: &'static str) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // 没有运行时（单测 / 同步装配）：调用方会自己调 `reload`。
            return;
        };
        let runtime = self.clone();
        handle.spawn(async move {
            if let Err(error) = runtime.reload().await {
                tracing::warn!(%error, reason, "skills reload failed");
            }
        });
    }

    /// 登记并在登记后主动重载（`Arc<Self>` 才做得到：重载要跨 `await`）。
    pub fn register_and_reload(
        self: &Arc<Self>,
        owner: &str,
        skills: &[(&str, &str)],
    ) -> Result<(), String> {
        echo_defs::skill::SkillRegistrar::register_markdown(self.as_ref(), owner, skills)?;
        self.reload_now("register");
        Ok(())
    }

    /// 撤销登记后让表生效。
    pub fn unregister_and_reload(self: &Arc<Self>, owner: &str) {
        echo_defs::skill::SkillRegistrar::unregister(self.as_ref(), owner);
        self.reload_now("unregister");
    }

    /// 重载：文件层重新发现 + 模块贡献层重新拉取 + 主动登记层合并。
    ///
    /// 返回 `true` = 注册表确实变了（调用方据此决定是否广播列表刷新）。
    pub async fn reload(&self) -> Result<bool, String> {
        // 贡献来源与文件目录**互相独立**：只有贡献、没有目录的宿主（或反过
        // 来）都是合法配置，因此不能因为"目录不存在"就早退掉贡献同步。
        //
        // 两层贡献先合并：拉模型（宿主给的外部模块 `Register` 帧）与推模型
        // （模块在 mount 里登记）。同 owner 以**推**为准——推的那份更晚、
        // 更具体（它是模块自己刚说的话）。
        let mut contributions = self.host.contributed_skills();
        for (owner, skills) in self.registered_snapshot() {
            contributions.retain(|(existing, _)| existing != &owner);
            contributions.push((owner, skills));
        }
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
        let reported: std::collections::BTreeSet<&str> = contributions
            .iter()
            .map(|(owner, _)| owner.as_str())
            .collect();
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
        // 「技能到底从哪来」是排查主线（少一个技能 = 提示词里少了整段规则，
        // 而且没有任何报错）。按来源分别计数，一眼看出是文件层还是模块层空。
        let owners: Vec<String> = reloaded.contributed_owners();
        tracing::info!(
            total = reloaded.len(),
            contributed_owners = ?owners,
            skills = ?reloaded.names(),
            "skills registry reloaded"
        );
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

/// 推模型接缝的实现：模块在自己的 `mount` 里调用。
///
/// 这里**只写表不重载**（`&self` 拿不到 `Arc`）。要"登记后立刻生效"的调用方
/// 用 [`SkillsRuntime::register_and_reload`]——模块 mount 拿到的是
/// `Arc<SkillsRuntime>`，走那条路（见 `lib.rs` 的 `RegistrarHandle`）。
impl echo_defs::skill::SkillRegistrar for SkillsRuntime {
    fn register_markdown(&self, owner: &str, skills: &[(&str, &str)]) -> Result<(), String> {
        if owner.is_empty() {
            return Err("skill registrar: owner 不能为空".into());
        }
        // 解析在**本模块**做：`Skill` 的 frontmatter 规则是技能模块的领域
        // 知识，注册方只交正文（见 `SkillRegistrar` 的文档）。
        let parsed: Vec<crate::Skill> = skills
            .iter()
            .map(|(text, fallback)| crate::loader::parse_skill_text(text, fallback))
            .collect();
        if parsed.is_empty() {
            return Err(format!("skill registrar: `{owner}` 交了 0 条技能"));
        }
        let names: Vec<String> = parsed.iter().map(|s| s.metadata.name.clone()).collect();
        let mut map = self
            .registered
            .write()
            .map_err(|_| "skill registrar: 登记表被 poison".to_string())?;
        map.insert(owner.to_string(), parsed);
        drop(map);
        // 「技能从哪来」是排查主线：留一条就能区分"没登记"与"登记了没进表"。
        tracing::debug!(owner, skills = ?names, "skill module registered");
        Ok(())
    }

    fn unregister(&self, owner: &str) {
        let removed = self
            .registered
            .write()
            .map(|mut map| map.remove(owner).is_some())
            .unwrap_or(false);
        if removed {
            tracing::debug!(owner, "skill module unregistered");
        }
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
        let host = Host::new(vec![(
            "echo-agent.adapter.qq".into(),
            vec![skill("qq-transport")],
        )]);
        let rt = runtime(host);
        assert!(rt.reload().await.unwrap());
        let registry = rt.registry.lock().await;
        assert!(
            registry.get("qq-transport").is_some(),
            "模块贡献的技能必须进表"
        );
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
        assert!(
            registry.get("b").is_none(),
            "set_contributed 是替换而不是追加"
        );
    }

    /// 推模型：模块在 `mount` 里交正文 → 登记后 `reload` 就见得到。
    ///
    /// 这条守的是「技能归属跟着模块生存期走」的正向一半：登记进来的技能
    /// 与宿主拉来的贡献**同权**（同一张 `contributed` 表、同一套撤销语义）。
    #[tokio::test]
    async fn registered_markdown_reaches_the_registry() {
        let host = Host::new(Vec::new());
        let rt = runtime(host);
        echo_defs::skill::SkillRegistrar::register_markdown(
            rt.as_ref(),
            "echo-agent.subagent",
            &[(
                "---\nname: subagent-delegation\ndescription: 委派\npackage: echo-agent.subagent\n---\n正文",
                "subagent-delegation",
            )],
        )
        .expect("登记");
        rt.reload().await.unwrap();
        let registry = rt.registry.lock().await;
        let skill = registry.get("subagent-delegation").expect("技能必须进表");
        assert_eq!(
            skill.metadata.package.as_deref(),
            Some("echo-agent.subagent"),
            "package 标签必须来自 frontmatter"
        );
        assert_eq!(registry.contributed_owners(), vec!["echo-agent.subagent"]);
    }

    /// 推模型：`unregister`（模块卸载）后技能必须消失。
    ///
    /// 负向一半同样重要——这正是改造前做不到的那件事：
    /// `modules.toml` 里 `enabled = false` 的模块，技能不许留在提示词里。
    #[tokio::test]
    async fn unregister_removes_the_skills() {
        let host = Host::new(Vec::new());
        let rt = runtime(host);
        echo_defs::skill::SkillRegistrar::register_markdown(
            rt.as_ref(),
            "echo-agent.prodoc",
            &[(
                "---\nname: prodoc-format\npackage: echo-agent.prodoc\n---\n正文",
                "prodoc-format",
            )],
        )
        .expect("登记");
        rt.reload().await.unwrap();
        assert!(rt.registry.lock().await.get("prodoc-format").is_some());

        echo_defs::skill::SkillRegistrar::unregister(rt.as_ref(), "echo-agent.prodoc");
        assert!(rt.reload().await.unwrap());
        let registry = rt.registry.lock().await;
        assert!(
            registry.get("prodoc-format").is_none(),
            "模块卸载后技能必须一起消失"
        );
        assert!(registry.contributed_owners().is_empty());
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
