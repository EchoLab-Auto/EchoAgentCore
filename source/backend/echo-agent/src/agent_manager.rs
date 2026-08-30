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
#[derive(Clone)]
pub struct RunningAgent {
    pub id: String,
    pub profile: TeamMember,
    pub agent: Arc<Agent>,
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
    teams: std::sync::RwLock<HashMap<String, TeamMember>>,
    /// Running instances (disabled ones are absent).
    agents: std::sync::RwLock<HashMap<String, RunningAgent>>,
    disabled: std::sync::Mutex<Vec<String>>,
    /// Persistence hook: writes the profiles map back to TOML (or any store).
    config_writer: std::sync::Mutex<Option<ConfigWriter>>,
}

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
        // No profiles configured -> legacy single default agent.
        if profiles.is_empty() {
            profiles.insert(
                "default".into(),
                TeamMember {
                    name: "默认".into(),
                    description: "默认助手（配置文件未定义人格）".into(),
                    system_prompt: raw.system_prompt.clone(),
                    enabled: true,
                    disabled_plugins: raw.disabled_plugins.clone(),
                    disabled_tools: raw.disabled_tools.clone(),
                    disabled_skills: raw.disabled_skills.clone(),
                    enabled_plugins: Vec::new(),
                    enabled_tools: Vec::new(),
                    enabled_skills: Vec::new(),
                    memory_limit_tokens: None,
                    context_window_tokens: None,
                },
            );
        }
        let teams = std::sync::RwLock::new(profiles);
        let disabled: Vec<String> = raw.disabled_teams.clone();
        let mut agents = HashMap::new();
        for (id, profile) in teams.read().unwrap().iter() {
            if disabled.contains(id) {
                continue;
            }
            let p = profile.clone();
            agents.insert(
                id.clone(),
                RunningAgent {
                    id: id.clone(),
                    profile: p.clone(),
                    agent: factory(id.clone(), p),
                },
            );
        }
        let default_id = raw
            .teams
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "default".into());
        Self {
            default_id,
            teams,
            agents: std::sync::RwLock::new(agents),
            disabled: std::sync::Mutex::new(disabled),
            config_writer: std::sync::Mutex::new(None),
        }
    }

    pub fn default_id(&self) -> String {
        self.default_id.clone()
    }

    pub fn agent_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.agents.read().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// All known profile ids (running + disabled).
    pub fn team_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.teams.read().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn team(&self, id: &str) -> Option<TeamMember> {
        self.teams.read().unwrap().get(id).cloned()
    }

    /// Resolve the target agent for a command (None agent_id -> default).
    pub fn resolve(&self, agent_id: Option<&str>) -> Option<Arc<Agent>> {
        let id = agent_id.unwrap_or(&self.default_id);
        self.agents
            .read()
            .unwrap()
            .get(id)
            .map(|r| Arc::clone(&r.agent))
    }

    pub fn get(&self, id: &str) -> Option<RunningAgent> {
        self.agents.read().unwrap().get(id).cloned()
    }

    pub fn all(&self) -> Vec<RunningAgent> {
        let mut list: Vec<RunningAgent> = self.agents.read().unwrap().values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// Runtime toggle. Disabling drops the manager's strong ref (agent memory
    /// unloads once last ref drops); enabling re-instantiates via factory.
    pub fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
        factory: impl Fn(String, TeamMember) -> Arc<Agent>,
    ) -> Result<(), String> {
        if !self.teams.read().unwrap().contains_key(id) {
            return Err(format!("agent not found: {id}"));
        }
        let mut agents = self.agents.write().unwrap();
        let mut disabled = self.disabled.lock().unwrap();
        if enabled {
            if agents.contains_key(id) {
                return Ok(());
            }
            let profile = self.teams.read().unwrap().get(id).cloned().unwrap();
            agents.insert(
                id.to_string(),
                RunningAgent {
                    id: id.to_string(),
                    profile: profile.clone(),
                    agent: factory(id.to_string(), profile),
                },
            );
            disabled.retain(|d| d != id);
        } else {
            if agents.remove(id).is_none() {
                return Ok(());
            }
            if !disabled.contains(&id.to_string()) {
                disabled.push(id.to_string());
            }
        }
        drop(disabled);
        self.persist_profiles()?;
        Ok(())
    }

    /// Inject the persistence hook (writes profiles to TOML).
    pub fn set_config_writer(&self, writer: ConfigWriter) {
        *self.config_writer.lock().unwrap() = Some(writer);
    }

    /// Create or update a profile. When enabled and not running, instantiates
    /// via the factory; persists through the config writer.
    pub fn save_profile(&self, id: &str, profile: TeamMember, enabled: bool) -> Result<(), String> {
        // 合并语义：传入 profile 的 per-agent 预算为 None 且旧 profile 已有值时
        // 保留旧值，避免前端（未编辑该字段）保存时擦除手工写入 TOML 的预算。
        let mut profile = profile;
        if let Some(old) = self.teams.read().unwrap().get(id) {
            if profile.memory_limit_tokens.is_none() {
                profile.memory_limit_tokens = old.memory_limit_tokens;
            }
            if profile.context_window_tokens.is_none() {
                profile.context_window_tokens = old.context_window_tokens;
            }
        }
        self.teams
            .write()
            .unwrap()
            .insert(id.to_string(), profile.clone());
        {
            let mut disabled = self.disabled.lock().unwrap();
            if enabled {
                disabled.retain(|d| d != id);
            } else if !disabled.contains(&id.to_string()) {
                disabled.push(id.to_string());
            }
        }
        let running = self.agents.read().unwrap().contains_key(id);
        if enabled && !running {
            let agent = agent_factory_for_toggle().call(id.to_string(), profile.clone());
            self.agents.write().unwrap().insert(
                id.to_string(),
                RunningAgent {
                    id: id.to_string(),
                    profile,
                    agent,
                },
            );
        } else if !enabled && running {
            self.agents.write().unwrap().remove(id);
        }
        self.persist_profiles()?;
        Ok(())
    }

    /// Delete a profile; the default/main agent is protected.
    pub fn delete_profile(&self, id: &str) -> Result<(), String> {
        if id == self.default_id {
            return Err("不能删除默认/主 agent".into());
        }
        self.teams.write().unwrap().remove(id);
        self.agents.write().unwrap().remove(id);
        let mut disabled = self.disabled.lock().unwrap();
        disabled.retain(|d| d != id);
        drop(disabled);
        self.persist_profiles()?;
        Ok(())
    }

    fn persist_profiles(&self) -> Result<(), String> {
        let writer = self.config_writer.lock().unwrap();
        match writer.as_ref() {
            Some(w) => {
                let map: std::collections::BTreeMap<String, TeamMember> =
                    self.teams.read().unwrap().clone().into_iter().collect();
                w(&map)
            }
            None => Ok(()),
        }
    }

    /// Persisted disable list (runtime toggles, survives restarts).
    pub fn disabled_list(&self) -> Vec<String> {
        let mut v = self.disabled.lock().unwrap().clone();
        v.sort();
        v
    }

    /// Snapshot for the Panel (AgentsList event) — all known profiles with
    /// running state and session counts.
    pub fn infos(&self) -> Vec<TeamInfo> {
        let agents = self.agents.read().unwrap();
        let teams = self.teams.read().unwrap();
        let mut list: Vec<TeamInfo> = teams
            .iter()
            .map(|(id, p)| TeamInfo {
                id: id.clone(),
                name: p.name.clone(),
                is_default: id == &self.default_id,
                description: p.description.clone(),
                enabled: agents.contains_key(id),
                sessions: agents.get(id).map(|r| r.agent.session_count()).unwrap_or(0),
                // 回执分支能力开关：与 Agent::allows_reply_branches 同语义
                //（enabled_plugins 为空=全部启用；非空需含该 id；黑名单优先）。
                reply_branches_enabled: {
                    let has_branch = p.enabled_plugins.is_empty()
                        || p.enabled_plugins
                            .iter()
                            .any(|x| x == crate::agent::REPLY_BRANCH_PLUGIN_ID);
                    has_branch
                        && !p.disabled_plugins
                            .iter()
                            .any(|x| x == crate::agent::REPLY_BRANCH_PLUGIN_ID)
                },
                // 全局会话视图开关：与 Agent::allows_global_session 同语义。
                global_session_enabled: {
                    let has_global = p.enabled_plugins.is_empty()
                        || p.enabled_plugins
                            .iter()
                            .any(|x| x == crate::agent::GLOBAL_SESSION_PLUGIN_ID);
                    has_global
                        && !p.disabled_plugins
                            .iter()
                            .any(|x| x == crate::agent::GLOBAL_SESSION_PLUGIN_ID)
                },
                system_prompt: p.system_prompt.clone(),
                disabled_plugins: p.disabled_plugins.clone(),
                disabled_tools: p.disabled_tools.clone(),
                disabled_skills: p.disabled_skills.clone(),
                enabled_plugins: p.enabled_plugins.clone(),
                enabled_tools: p.enabled_tools.clone(),
                enabled_skills: p.enabled_skills.clone(),
                memory_limit_tokens: p.memory_limit_tokens,
                context_window_tokens: p.context_window_tokens,
            })
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }
}

/// Process-wide manager & factory slots.
///
/// ⚠️ MUST be module-level statics: function-local `static` items in Rust are
/// **per-function instances**, so a setter and a getter declared in separate
/// functions would refer to different statics and the value would never be
/// visible. (This exact bug made SaveAgent report "agent manager unavailable".)
static GLOBAL_MANAGER: std::sync::OnceLock<std::sync::Arc<AgentManager>> =
    std::sync::OnceLock::new();
static AGENT_FACTORY: std::sync::OnceLock<AgentFactory> = std::sync::OnceLock::new();

/// Process-wide AgentManager (best-effort): set once by the composition root.
/// Lets any Agent read the persona list (e.g. RequestAgentsList) without a
/// direct dependency on the manager instance.
pub fn global_manager() -> Option<std::sync::Arc<AgentManager>> {
    GLOBAL_MANAGER.get().cloned()
}

pub fn set_global_manager(manager: std::sync::Arc<AgentManager>) {
    let _ = GLOBAL_MANAGER.set(manager);
}

/// Process-wide agent factory (set once by the composition root). Used by
/// runtime toggling: enabling a disabled agent rebuilds it from config.
pub fn set_agent_factory(
    factory: impl Fn(String, TeamMember) -> Arc<Agent> + Send + Sync + 'static,
) {
    let _ = AGENT_FACTORY.set(AgentFactory {
        inner: std::sync::Arc::new(factory),
    });
}

/// Clonable snapshot of the agent factory (for `set_enabled` calls from
/// commands). Panics only if the composition root never injected a factory
/// (should not happen in production).
#[derive(Clone)]
pub struct AgentFactory {
    inner: std::sync::Arc<dyn Fn(String, TeamMember) -> Arc<Agent> + Send + Sync>,
}

impl AgentFactory {
    pub fn call(&self, id: String, profile: TeamMember) -> Arc<Agent> {
        (self.inner)(id, profile)
    }
}

/// Retrieve the registered factory (panics with a clear message when unset).
pub fn agent_factory_for_toggle() -> AgentFactory {
    AGENT_FACTORY
        .get()
        .cloned()
        .unwrap_or_else(|| panic!("agent factory not set by composition root"))
}
