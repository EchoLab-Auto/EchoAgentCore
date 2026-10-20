//! AgentManager — 多 agent 人格实例管理与命令路由。
//!
//! Core 启动时根据 `[agent.profiles]` 为每个启用的人格实例化一个独立
//! `Agent`（独立 trunk / 会话日志 / 系统提示词）。前端 `SendMessage` 带
//! `agent_id` 时路由到对应实例；缺省走 default。`SessionInfo.agent_id`
//! 由各 Agent 在自身 `info()` 中填充（无需在此标注事件）。

use std::collections::HashMap;
use std::sync::Arc;

use echo_protocol::TeamInfo;

use crate::agent::Agent;
use crate::config::{AgentConfig, TeamMember};

/// A running persona: profile + its agent instance.
///
/// # 构造是封住的（2026-10-20 事故的根治）
///
/// 字段可读（调用方大量读取 `id` / `profile` / `agent`），但**不能从外部
/// 直接构造**——`_sealed` 是私有字段，因此模块外写不出 `RunningAgent { .. }`。
/// 唯一入口是 [`RunningAgent::bound`]，它在构造时**强制**把 manager 弱引用
/// 绑到 agent 上。
///
/// 为什么值得这样一个技巧：绑定漏了不会 panic、不会报错，只会让
/// `TeamsList` / 命令路由**静默降级为空**——2026-10-20 的两次漏绑
/// （`__core`、运行期新建人格）都是这样藏了很久。把"必须绑定"变成类型
/// 约束，比在调用点补 `set_manager` 靠得住：以后新增创建路径时，
/// 不经过 `bound` 就编译不过。
#[derive(Clone)]
pub struct RunningAgent {
    pub id: String,
    pub profile: TeamMember,
    pub agent: Arc<Agent>,
    /// 私有标记：封住结构体字面量构造（见类型文档）。恒为 `()`。
    _sealed: (),
}

impl RunningAgent {
    /// **唯一构造入口**：构造即绑定 manager。
    ///
    /// 内部做两件事，顺序不可换：先把 manager 弱引用写给 agent，再返回结构体。
    /// 调用方拿到的一定是"已绑定"的运行实例。
    fn bound(
        manager: &std::sync::Arc<AgentManager>,
        id: String,
        profile: TeamMember,
        agent: Arc<Agent>,
    ) -> Self {
        agent.set_manager(manager);
        Self {
            id,
            profile,
            agent,
            _sealed: (),
        }
    }
}

impl std::fmt::Debug for RunningAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningAgent")
            .field("id", &self.id)
            .field("name", &self.profile.name)
            .finish()
    }
}

/// Owns all persona agents and routes frontend commands to the right one.
pub struct AgentManager {
    default_id: String,
    /// All team member definitions (including disabled ones), for rebuild.
    teams: parking_lot::RwLock<HashMap<String, TeamMember>>,
    /// Running instances (disabled ones are absent).
    agents: parking_lot::RwLock<HashMap<String, RunningAgent>>,
    disabled: parking_lot::Mutex<Vec<String>>,
    /// Persistence hook: writes the profiles map back to TOML (or any store).
    config_writer: parking_lot::Mutex<Option<ConfigWriter>>,
    /// 本节点身份（NodeId + 运行区域名）：`infos()` 把它写进每条 `TeamInfo`，
    /// 供多节点聚合客户端区分同名人格。组合根装配期注入（P3 终态：参数，
    /// 非 kernel cell）。
    node_identity: parking_lot::RwLock<crate::NodeIdentity>,
    /// 人格重建工厂（组合根在 manager 构造完成后注入）：运行期启用被禁用
    /// 的人格时用它重建实例，与启动期同一装配闭包。
    ///
    /// 2026-10 技术债清理：此前是 `echo_bootstrap` 进程级槽
    /// （`set_agent_factory` / `agent_factory_for_toggle`）。它天然属于
    /// **某个** manager——现在直接挂在它自己身上，两套 manager 各持各的。
    /// None = 未注入（toggle 返回明确错误，而不是 panic）。
    factory: parking_lot::RwLock<Option<AgentFactoryInner>>,
}

/// 工厂闭包的内部类型别名（对外仍用 [`AgentFactory`] 包装）。
type AgentFactoryInner = std::sync::Arc<dyn Fn(String, TeamMember) -> Arc<Agent> + Send + Sync>;

/// Closure that persists the profiles map (composition root injects the
/// ConfigStore patch writer).
pub type ConfigWriter = Box<
    dyn Fn(&std::collections::BTreeMap<String, TeamMember>) -> Result<(), String> + Send + Sync,
>;

impl std::fmt::Debug for AgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManager")
            .field("default_id", &self.default_id)
            .field("agents", &self.agent_ids())
            .finish()
    }
}

impl AgentManager {
    /// Build the manager from config profiles + an agent factory.
    ///
    /// `factory(id, profile)` constructs the Agent for a persona (the
    /// composition root wires provider/config/session paths there).
    pub fn build(raw: &AgentConfig, factory: impl Fn(String, TeamMember) -> Arc<Agent>) -> Self {
        let mut profiles: HashMap<String, TeamMember> = HashMap::new();
        for (id, profile) in &raw.teams {
            let mut p = profile.clone();
            if p.name.is_empty() {
                p.name = id.clone();
            }
            profiles.insert(id.clone(), p);
        }
        // 未配置人格 = 零人格（2026-10 用户请求）：此前 `teams` 为空会合成
        // 一个 legacy "default" 人格——每台新节点/新装 Core 都会凭空多出
        // 一个"默认" agent（`agents=["default"]`），与"去主智能体"语义
        // （无默认兜底、命令须显式 team_id）相悖。现在空配置就是没有智能体，
        // 由 Panel「设置 → 智能体 → 新建智能体」（或手写 `[agent.teams.<id>]`）
        // 按需创建。
        let teams = parking_lot::RwLock::new(profiles);
        let disabled: Vec<String> = raw.disabled_teams.clone();
        let mut agents = HashMap::new();
        for (id, profile) in teams.read().iter() {
            if !profile.enabled || disabled.contains(id) {
                continue;
            }
            let p = profile.clone();
            // `build` 时 manager 尚未构造完成（`Self` 正在建），无法给出
            // `&Arc<Self>`。这里先构造、由 `AgentManager::new` 之后统一
            // `bind_all` 兜底——该函数是唯一能把"未绑定"变成"已绑定"的
            // 地方，且 `_sealed` 保证外部无法绕过。
            let agent = factory(id.clone(), p.clone());
            // 这里**不能**走 `RunningAgent::bound`：`Self` 正在构造中，
            // 拿不到 `&Arc<Self>`。改由 `AgentManager::bind_all` 在 Arc
            // 建立之后统一补绑（唯一能把"未绑定"变"已绑定"的地方）。
            agents.insert(
                id.clone(),
                RunningAgent {
                    id: id.clone(),
                    profile: p,
                    agent,
                    _sealed: (),
                },
            );
        }
        let default_id = agents
            .keys()
            .min()
            .cloned()
            .unwrap_or_else(|| "default".into());
        Self {
            default_id,
            teams,
            agents: parking_lot::RwLock::new(agents),
            disabled: parking_lot::Mutex::new(disabled),
            config_writer: parking_lot::Mutex::new(None),
            node_identity: parking_lot::RwLock::new(crate::NodeIdentity::default()),
            factory: parking_lot::RwLock::new(None),
        }
    }

    /// 注入本节点身份（组合根装配期，一次）。`infos()` 的每条 `TeamInfo`
    /// 都会带上它。
    pub fn set_node_identity(&self, identity: crate::NodeIdentity) {
        *self.node_identity.write() = identity;
    }

    pub fn default_id(&self) -> String {
        self.default_id.clone()
    }

    pub fn agent_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.agents.read().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// All known profile ids (running + disabled).
    pub fn team_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.teams.read().keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn team(&self, id: &str) -> Option<TeamMember> {
        self.teams.read().get(id).cloned()
    }

    /// Resolve the target agent for a command.
    ///
    /// 去主智能体（2026-09）：`None` 不再回退"默认人格"——返回 `None`，
    /// 由调用方给出明确错误；未知 id 同样返回 `None`（不再静默兜底）。
    pub fn resolve(&self, agent_id: Option<&str>) -> Option<Arc<Agent>> {
        let id = agent_id?;
        self.agents.read().get(id).map(|r| Arc::clone(&r.agent))
    }

    pub fn get(&self, id: &str) -> Option<RunningAgent> {
        self.agents.read().get(id).cloned()
    }

    pub fn all(&self) -> Vec<RunningAgent> {
        let mut list: Vec<RunningAgent> = self.agents.read().values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// 引用 `api_profile == name` 的全部 team id（含未运行/禁用；升序）。
    /// API profile 改名时用于计算落盘补丁的改写范围。
    pub fn api_ref_owners(&self, name: &str) -> Vec<String> {
        let teams = self.teams.read();
        let mut ids: Vec<String> = teams
            .iter()
            .filter(|(_, t)| t.api_profile.as_deref() == Some(name))
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// API profile 改名传播（内存侧）：把 teams 定义与运行中 persona 快照的
    /// `api_profile` 引用从 `from` 改为 `to`，返回 teams 改写数。
    ///
    /// 只做内存改写——持久化由调用方在同一次 ConfigStore patch 内完成；
    /// 不触发启停（区别于 [`Self::save_profile`] 的建销副作用）。
    pub fn replace_api_refs(&self, from: &str, to: &str) -> usize {
        let mut changed = 0usize;
        {
            let mut teams = self.teams.write();
            for t in teams.values_mut() {
                if t.api_profile.as_deref() == Some(from) {
                    t.api_profile = Some(to.to_string());
                    changed += 1;
                }
            }
        }
        {
            let mut agents = self.agents.write();
            for running in agents.values_mut() {
                if running.profile.api_profile.as_deref() == Some(from) {
                    running.profile.api_profile = Some(to.to_string());
                }
            }
        }
        changed
    }

    /// Runtime toggle. Disabling drops the manager's strong ref (agent memory
    /// unloads once last ref drops); enabling re-instantiates via factory.
    pub fn set_enabled(
        self: &std::sync::Arc<Self>,
        id: &str,
        enabled: bool,
        factory: impl Fn(String, TeamMember) -> Arc<Agent>,
    ) -> Result<(), String> {
        if !self.teams.read().contains_key(id) {
            return Err(format!("agent not found: {id}"));
        }
        let mut agents = self.agents.write();
        let mut disabled = self.disabled.lock();
        if enabled {
            if !agents.contains_key(id) {
                let profile = self.teams.read().get(id).cloned().unwrap();
                let agent = factory(id.to_string(), profile.clone());
                // 走唯一入口：构造即绑定 manager。
                let running = RunningAgent::bound(self, id.to_string(), profile, agent);
                agents.insert(id.to_string(), running);
            }
            disabled.retain(|d| d != id);
        } else {
            agents.remove(id);
            if !disabled.contains(&id.to_string()) {
                disabled.push(id.to_string());
            }
        }
        // Keep the profile's persisted flag in lockstep with the runtime
        // registry. Otherwise a ToggleTeam(false) would appear disabled only
        // until the next rebuild, then restart as enabled.
        if let Some(profile) = self.teams.write().get_mut(id) {
            profile.enabled = enabled;
        }
        drop(disabled);
        self.persist_profiles()?;
        Ok(())
    }

    /// 给注册表内**全部**运行实例补绑 manager 弱引用。
    ///
    /// 只在 `Arc<AgentManager>` 建立之后调用一次（`build` 阶段拿不到 Arc）。
    /// 这是"未绑定 → 已绑定"的唯一通道；`RunningAgent::bound` 覆盖其余
    /// 创建路径（`set_enabled` / `save_profile`）。
    pub fn bind_all(&self, manager: &std::sync::Arc<Self>) {
        for running in self.agents.read().values() {
            running.agent.set_manager(manager);
        }
    }

    /// 注入人格重建工厂（组合根调用一次；见字段文档）。
    pub fn set_factory(
        &self,
        factory: impl Fn(String, TeamMember) -> Arc<Agent> + Send + Sync + 'static,
    ) {
        *self.factory.write() = Some(std::sync::Arc::new(factory));
    }

    /// 取重建工厂（未注入 = None）。
    pub fn factory_ref(&self) -> Option<AgentFactory> {
        self.factory.read().as_ref().map(|inner| AgentFactory {
            inner: std::sync::Arc::clone(inner),
        })
    }

    pub fn set_config_writer(&self, writer: ConfigWriter) {
        *self.config_writer.lock() = Some(writer);
    }

    /// Create or update a profile. When enabled and not running, instantiates
    /// via the factory; persists through the config writer.
    pub fn save_profile(
        self: &std::sync::Arc<Self>,
        id: &str,
        profile: TeamMember,
        enabled: bool,
    ) -> Result<(), String> {
        let mut profile = profile;
        // 合并语义：传入 profile 的 per-agent 预算为 None 且旧 profile 已有值时
        // 保留旧值，避免前端（未编辑该字段）保存时擦除手工写入 TOML 的预算。
        if let Some(old) = self.teams.read().get(id) {
            if profile.memory_limit_tokens.is_none() {
                profile.memory_limit_tokens = old.memory_limit_tokens;
            }
            if profile.context_window_tokens.is_none() {
                profile.context_window_tokens = old.context_window_tokens;
            }
        }
        self.teams.write().insert(id.to_string(), profile.clone());
        {
            let mut disabled = self.disabled.lock();
            if enabled {
                disabled.retain(|d| d != id);
            } else if !disabled.contains(&id.to_string()) {
                disabled.push(id.to_string());
            }
        }
        let running = self.agents.read().contains_key(id);
        if enabled && !running {
            let agent = self
                .factory_ref()
                .ok_or_else(|| {
                    format!("agent factory not injected by composition root; cannot rebuild {id}")
                })?
                .call(id.to_string(), profile.clone());
            // 走唯一入口：构造即绑定 manager（漏绑会让 TeamsList 静默为空）。
            let running = RunningAgent::bound(self, id.to_string(), profile, agent);
            self.agents.write().insert(id.to_string(), running);
        } else if !enabled && running {
            self.agents.write().remove(id);
        } else if enabled && running {
            // 运行中保存：同步 RunningAgent.profile（2026-10 修复）。
            // SaveTeam 已把新能力应用到运行中 agent，但这里此前不同步——
            // 后续 ToggleTeam(id, true)（already running 分支跳过重建）
            // 会用**旧 profile** 重应用能力，把新能力回滚掉（profile 漂移）。
            if let Some(entry) = self.agents.write().get_mut(id) {
                entry.profile = profile;
            }
        }
        self.persist_profiles()?;
        Ok(())
    }

    /// Delete a profile.
    ///
    /// 去主智能体（2026-09）：不再有受保护的"主 agent"；唯一约束是
    /// **至少保留一个智能体**（否则前端/路由将无目标可选）。
    pub fn delete_profile(&self, id: &str) -> Result<(), String> {
        {
            let teams = self.teams.read();
            if teams.len() <= 1 {
                return Err("至少保留一个智能体".into());
            }
            if !teams.contains_key(id) {
                return Err(format!("智能体 {id} 不存在"));
            }
        }
        self.teams.write().remove(id);
        self.agents.write().remove(id);
        let mut disabled = self.disabled.lock();
        disabled.retain(|d| d != id);
        drop(disabled);
        self.persist_profiles()?;
        Ok(())
    }

    fn persist_profiles(&self) -> Result<(), String> {
        let writer = self.config_writer.lock();
        match writer.as_ref() {
            Some(w) => {
                let map: std::collections::BTreeMap<String, TeamMember> =
                    self.teams.read().clone().into_iter().collect();
                w(&map)
            }
            None => Ok(()),
        }
    }

    /// Persisted disable list (runtime toggles, survives restarts).
    pub fn disabled_list(&self) -> Vec<String> {
        let mut v = self.disabled.lock().clone();
        v.sort();
        v
    }

    /// Snapshot for the Panel (AgentsList event) — all known profiles with
    /// running state and session counts.
    pub fn infos(&self) -> Vec<TeamInfo> {
        let identity = self.node_identity.read();
        let agents = self.agents.read();
        let teams = self.teams.read();
        let mut list: Vec<TeamInfo> = teams
            .iter()
            .map(|(id, p)| TeamInfo {
                id: id.clone(),
                node_id: identity.node_id_field(),
                region_name: identity.region_name_field(),
                name: p.name.clone(),
                description: p.description.clone(),
                enabled: agents.contains_key(id),
                system_skills: p.system_skills.clone(),
                sessions: agents.get(id).map(|r| r.agent.session_count()).unwrap_or(0),
                // 循环模式：互斥循环插件推导（single 为兜底，也是默认），
                // 单一来源 TeamMember::loop_mode；parallel 才展示会话管理 UI
                // 并发射 ReplyBranch* 可见性事件。
                loop_mode: p.loop_mode(),
                system_prompt: p.system_prompt.clone(),
                disabled_tools: p.disabled_tools.clone(),
                disabled_skills: p.disabled_skills.clone(),
                enabled_plugins: p.enabled_plugins.clone(),
                enabled_tools: p.enabled_tools.clone(),
                enabled_skills: p.enabled_skills.clone(),
                memory_limit_tokens: p.memory_limit_tokens,
                context_window_tokens: p.context_window_tokens,
                api_profile: p.api_profile.clone(),
            })
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }
}

/// Process-wide manager & factory slots.
///
#[derive(Clone)]
pub struct AgentFactory {
    inner: AgentFactoryInner,
}

impl AgentFactory {
    pub fn call(&self, id: String, profile: TeamMember) -> Arc<Agent> {
        (self.inner)(id, profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member_with_plugins(enabled: &[&str]) -> TeamMember {
        TeamMember {
            name: "t".into(),
            enabled_plugins: enabled.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// 构造 manager：测试成员全部放进 disabled_teams，build 不调 factory
    ///（factory 缺失会 panic——确保测试不触碰实例化路径）。
    fn manager_with(raw: AgentConfig) -> AgentManager {
        AgentManager::build(&raw, |_id, _p| {
            panic!("factory must not be called in tests")
        })
    }

    #[test]
    fn infos_reports_loop_mode_per_member() {
        use crate::plugins::{PARALLEL_LOOP_PLUGIN_ID, SINGLE_LOOP_PLUGIN_ID};
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "bot".into(),
            member_with_plugins(&[PARALLEL_LOOP_PLUGIN_ID]),
        );
        raw.teams.insert(
            "coder".into(),
            member_with_plugins(&[SINGLE_LOOP_PLUGIN_ID]),
        );
        raw.teams.insert(
            "bare".into(),
            member_with_plugins(&["echo-agent.tools.builtin"]),
        );
        raw.disabled_teams = vec!["bot".into(), "coder".into(), "bare".into()];
        let mgr = manager_with(raw);
        let infos = mgr.infos();
        let mode_of = |id: &str| {
            let info = infos.iter().find(|t| t.id == id).unwrap();
            info.loop_mode
        };
        assert_eq!(mode_of("bot"), echo_defs::LoopMode::Parallel);
        assert_eq!(mode_of("coder"), echo_defs::LoopMode::Single);
        // 非空白名单无模式 id → Single（兜底）
        assert_eq!(mode_of("bare"), echo_defs::LoopMode::Single);
    }

    /// 节点身份作为**参数**下发（P3 终态，不再经 kernel cell）：
    /// `set_node_identity` 之后每条 `TeamInfo` 都带上它。
    #[test]
    fn infos_carry_injected_node_identity() {
        let mut raw = AgentConfig::default();
        raw.teams.insert("a".into(), member_with_plugins(&[]));
        raw.disabled_teams = vec!["a".into()];
        let mgr = manager_with(raw);
        // 未注入 → 字段不下发（嵌入式/测试用法）。
        let info = &mgr.infos()[0];
        assert_eq!(info.node_id, None);
        assert_eq!(info.region_name, None);

        mgr.set_node_identity(crate::NodeIdentity::new("node-01abc", "办公机"));
        let info = &mgr.infos()[0];
        assert_eq!(info.node_id.as_deref(), Some("node-01abc"));
        assert_eq!(info.region_name.as_deref(), Some("办公机"));

        // 区域名为空 = 只下发 node_id（前端回退用它展示）。
        mgr.set_node_identity(crate::NodeIdentity::new("node-01abc", ""));
        let info = &mgr.infos()[0];
        assert_eq!(info.node_id.as_deref(), Some("node-01abc"));
        assert_eq!(info.region_name, None);
    }

    #[test]
    fn disabled_profile_is_not_instantiated() {
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "disabled".into(),
            TeamMember {
                enabled: false,
                ..Default::default()
            },
        );
        let manager = AgentManager::build(&raw, |_id, _profile| {
            panic!("disabled profiles must not instantiate an Agent")
        });
        assert!(manager.agent_ids().is_empty());
        assert_eq!(manager.team_ids(), vec!["disabled"]);
        assert!(!manager.infos()[0].enabled);
    }

    /// 2026-10 用户请求回归：**未配置任何 `[agent.teams.*]` 时不得合成
    /// "default" 人格**——此前每台新节点/新装 Core 都会凭空多出一个
    /// "默认" agent（`agents=["default"]`）；空配置现在就是零人格，
    /// 由 Panel 按需新建。
    #[test]
    fn empty_teams_create_no_personas() {
        let raw = AgentConfig::default();
        let manager = AgentManager::build(&raw, |_id, _profile| {
            panic!("empty teams must not instantiate any Agent")
        });
        assert!(
            manager.agent_ids().is_empty(),
            "unconfigured Core must start with zero personas: {:?}",
            manager.agent_ids()
        );
        assert!(manager.team_ids().is_empty());
        assert!(manager.infos().is_empty());
        // 无兜底解析：None / "default" 都必须落空（去主智能体语义）。
        assert!(manager.resolve(None).is_none(), "no default fallback");
        assert!(manager.resolve(Some("default")).is_none());
    }

    // ── API profile 改名传播（2026-10）──

    struct StubProvider;

    #[async_trait::async_trait]
    impl crate::llm::LlmProvider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }
        fn default_model(&self) -> &str {
            "stub"
        }
        async fn chat(
            &self,
            _request: &crate::llm::ChatRequest,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Config("stub".into()))
        }
        async fn chat_stream(
            &self,
            _request: &crate::llm::ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<crate::llm::ChatChunk>,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Config("stub".into()))
        }
    }

    fn stub_agent(_id: String, _p: TeamMember) -> Arc<Agent> {
        Arc::new(Agent::new(
            Arc::new(StubProvider),
            AgentConfig::default(),
            crate::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
        ))
    }

    /// 改名传播：teams（含禁用）与运行中 persona 快照的引用一起改写；
    /// 无关引用不受影响；只做内存改写（无启停副作用）。
    ///
    /// （tokio runtime：`Agent::new` 装配期需要运行时上下文。）
    #[tokio::test]
    async fn replace_api_refs_updates_teams_and_running_profiles() {
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "alice".into(),
            TeamMember {
                name: "Alice".into(),
                api_profile: Some("alpha".into()),
                ..Default::default()
            },
        );
        raw.teams.insert(
            "bob".into(),
            TeamMember {
                name: "Bob".into(),
                api_profile: Some("beta".into()),
                ..Default::default()
            },
        );
        raw.teams.insert(
            "carol".into(),
            TeamMember {
                name: "Carol".into(),
                api_profile: Some("alpha".into()),
                ..Default::default()
            },
        );
        raw.disabled_teams = vec!["carol".into()];
        let mgr = AgentManager::build(&raw, stub_agent);

        assert_eq!(
            mgr.api_ref_owners("alpha"),
            vec!["alice".to_string(), "carol".to_string()]
        );
        assert_eq!(mgr.api_ref_owners("beta"), vec!["bob".to_string()]);
        assert!(mgr.api_ref_owners("gamma").is_empty());

        let changed = mgr.replace_api_refs("alpha", "gamma");
        assert_eq!(changed, 2, "alice + carol (disabled included)");

        assert!(mgr.api_ref_owners("alpha").is_empty());
        assert_eq!(
            mgr.api_ref_owners("gamma"),
            vec!["alice".to_string(), "carol".to_string()]
        );
        assert_eq!(
            mgr.team("alice").unwrap().api_profile.as_deref(),
            Some("gamma")
        );
        assert_eq!(
            mgr.team("bob").unwrap().api_profile.as_deref(),
            Some("beta"),
            "unrelated reference untouched"
        );
        // 运行中 persona 的快照同步更新——不重建实例。
        assert_eq!(
            mgr.get("alice").unwrap().profile.api_profile.as_deref(),
            Some("gamma")
        );
        assert_eq!(
            mgr.get("bob").unwrap().profile.api_profile.as_deref(),
            Some("beta")
        );
        // carol 未运行：无实例，但定义已更新。
        assert!(mgr.get("carol").is_none());
    }
}
