//! AgentSupervisor — 多 persona agent 的装配与解析。
//!
//! 每个 `[agent.teams.*]` 条目 = 一个独立 `Agent`（独立 trunk / 会话文件 /
//! 系统提示词）。去主智能体（2026-09-13）后没有"默认人格"：命令路由由
//! `Agent::apply_command` 按显式 `team_id` 完成，本模块只负责构建与按 id
//! 解析（`get` / `get_exact`），以及给组合根提供 `personas()` 迭代。

use std::collections::HashMap;
use std::sync::Arc;

use echo_agent::{Agent, AgentConfig, AgentProfile};

/// A persona instance: id + capability profile + the agent itself.
pub struct Persona {
    pub id: String,
    /// 能力画像（组合根用 `apply_capabilities` 重新应用门控）。
    pub profile: AgentProfile,
    pub agent: Arc<Agent>,
}

/// Owns all personas; wires events + routes commands.
pub struct AgentSupervisor {
    personas: std::sync::Mutex<HashMap<String, Persona>>,
    /// Factory to (re)build a persona instance at runtime (SaveAgent/update).
    make_agent: Box<dyn Fn(String, AgentProfile) -> Arc<Agent> + Send + Sync>,
}

impl AgentSupervisor {
    /// Build personas. `make_agent(id, profile)` creates each Agent (the
    /// caller must have set its agent_id and config path).
    pub fn build(
        raw: &AgentConfig,
        make_agent: impl Fn(String, AgentProfile) -> Arc<Agent> + Send + Sync + 'static,
    ) -> Self {
        let mut personas: HashMap<String, Persona> = HashMap::new();
        let mut profiles: Vec<(String, AgentProfile)> = raw
            .teams
            .iter()
            .map(|(id, p)| (id.clone(), p.clone()))
            .collect();
        if profiles.is_empty() {
            profiles.push((
                "default".into(),
                AgentProfile {
                    name: "默认".into(),
                    description: "默认助手（配置文件未定义人格）".into(),
                    system_prompt: raw.system_prompt.clone(),
                    enabled: true,
                    system_skills: Vec::new(),
                    disabled_tools: raw.disabled_tools.clone(),
                    disabled_skills: raw.disabled_skills.clone(),
                    enabled_plugins: Vec::new(),
                    enabled_tools: Vec::new(),
                    enabled_skills: Vec::new(),
                    memory_limit_tokens: None,
                    context_window_tokens: None,
                    api_profile: None,
                    disabled_plugins: Vec::new(),
                },
            ));
        }
        profiles.sort_by(|a, b| a.0.cmp(&b.0));
        for (id, profile) in profiles {
            if !profile.enabled || raw.disabled_teams.iter().any(|disabled| disabled == &id) {
                continue;
            }
            let agent = make_agent(id.clone(), profile.clone());
            personas.insert(
                id.clone(),
                Persona {
                    id: id.clone(),
                    profile,
                    agent,
                },
            );
        }
        // Keep a routable management persona even when configuration disables
        // every declared team. This avoids a startup panic while preserving
        // the persisted enabled state of those teams.
        if personas.is_empty() {
            let id = "default".to_string();
            let profile = AgentProfile {
                name: "默认".into(),
                description: "临时管理助手（所有配置人格均已禁用）".into(),
                system_prompt: raw.system_prompt.clone(),
                enabled: true,
                system_skills: Vec::new(),
                disabled_tools: raw.disabled_tools.clone(),
                disabled_skills: raw.disabled_skills.clone(),
                enabled_plugins: Vec::new(),
                enabled_tools: Vec::new(),
                enabled_skills: Vec::new(),
                memory_limit_tokens: None,
                context_window_tokens: None,
                api_profile: None,
                disabled_plugins: Vec::new(),
            };
            let agent = make_agent(id.clone(), profile.clone());
            personas.insert(id.clone(), Persona { id, profile, agent });
        }
        Self {
            personas: std::sync::Mutex::new(personas),
            make_agent: Box::new(make_agent),
        }
    }

    /// Create (or return existing) persona at runtime.
    pub fn create(&self, id: &str, profile: AgentProfile) -> Arc<Agent> {
        let mut personas = self.personas.lock().unwrap();
        if let Some(existing) = personas.get(id) {
            return Arc::clone(&existing.agent);
        }
        let agent = (self.make_agent)(id.to_string(), profile.clone());
        personas.insert(
            id.to_string(),
            Persona {
                id: id.to_string(),
                profile,
                agent: Arc::clone(&agent),
            },
        );
        agent
    }

    pub fn get(&self, id: &str) -> Option<Persona> {
        self.personas.lock().unwrap().get(id).map(|p| Persona {
            id: p.id.clone(),
            profile: p.profile.clone(),
            agent: Arc::clone(&p.agent),
        })
    }

    /// 按 id 解析（未知返回 None）——路由用，绝无兜底。
    pub fn get_exact(&self, id: &str) -> Option<Persona> {
        self.personas.lock().unwrap().get(id).map(|p| Persona {
            id: p.id.clone(),
            profile: p.profile.clone(),
            agent: Arc::clone(&p.agent),
        })
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.personas.lock().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn personas(&self) -> Vec<Persona> {
        self.personas
            .lock()
            .unwrap()
            .values()
            .map(|p| Persona {
                id: p.id.clone(),
                profile: p.profile.clone(),
                agent: Arc::clone(&p.agent),
            })
            .collect()
    }
}
