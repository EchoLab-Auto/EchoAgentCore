//! AgentSupervisor — 多 persona agent 的装配与命令路由。
//!
//! 每个 `[agent.profiles]` 条目 = 一个独立 `Agent`（独立 trunk / 会话文件 /
//! 系统提示词）。管理面仍是**一个** WS 连接：非默认 persona 的事件经
//! 镜像任务转发进默认 persona 的 BackendBridge；发送命令时按
//! `SendMessage.agent_id` 路由到对应 persona 的后端通道。

use std::collections::HashMap;
use std::sync::Arc;

use echo_agent::bridge::BackendBridge;
use echo_agent::{Agent, AgentConfig, AgentProfile};

/// A persona instance with its own backend channel.
pub struct Persona {
    pub id: String,
    #[allow(dead_code)] // 供 Panel agent 概览 / 后续事件镜像
    pub profile: AgentProfile,
    pub agent: Arc<Agent>,
    /// Per-persona backend channel (event mirror target). Created per
    /// persona at build time; the default persona's bridge is consumed by
    /// the management server.
    #[allow(dead_code)]
    bridge: Option<BackendBridge>,
}

/// Owns all personas; wires events + routes commands.
pub struct AgentSupervisor {
    default_id: String,
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
                    disabled_plugins: raw.disabled_plugins.clone(),
                    disabled_tools: raw.disabled_tools.clone(),
                    disabled_skills: raw.disabled_skills.clone(),
                    enabled_plugins: Vec::new(),
                    enabled_tools: Vec::new(),
                    enabled_skills: Vec::new(),
                },
            ));
        }
        profiles.sort_by(|a, b| a.0.cmp(&b.0));
        let profiles_sorted_first = profiles.first().map(|(id, _)| id.clone());
        for (id, profile) in profiles {
            let agent = make_agent(id.clone(), profile.clone());
            personas.insert(
                id.clone(),
                Persona {
                    id: id.clone(),
                    profile,
                    agent,
                    bridge: None,
                },
            );
        }
        // default_id 必须是排序后的第一个 profile（BTreeMap 语义），
        // 而不是 HashMap 的随机迭代首项——否则默认人格会漂移。
        let default_id = profiles_sorted_first
            .clone()
            .unwrap_or_else(|| "default".into());
        Self {
            default_id,
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
                bridge: None,
            },
        );
        agent
    }

    /// Remove a persona (drops the strong ref; memory unloads when idle).
    #[allow(dead_code)] // 预留：DeleteAgent 运行时卸载
    pub fn remove(&self, id: &str) -> bool {
        self.personas.lock().unwrap().remove(id).is_some()
    }

    pub fn default_id(&self) -> String {
        self.default_id.clone()
    }

    pub fn get(&self, id: &str) -> Option<Persona> {
        self.personas.lock().unwrap().get(id).map(|p| Persona {
            id: p.id.clone(),
            profile: p.profile.clone(),
            agent: Arc::clone(&p.agent),
            bridge: None,
        })
    }

    /// Resolve target persona (None agent_id -> default; unknown -> default).
    pub fn resolve(&self, agent_id: Option<&str>) -> Persona {
        let id = agent_id.unwrap_or(&self.default_id);
        let personas = self.personas.lock().unwrap();
        let p = personas
            .get(id)
            .or_else(|| personas.get(&self.default_id))
            .expect("at least the default persona exists");
        Persona {
            id: p.id.clone(),
            profile: p.profile.clone(),
            agent: Arc::clone(&p.agent),
            bridge: None,
        }
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
                bridge: None,
            })
            .collect()
    }
}
