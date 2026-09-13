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
        let mut first_enabled = None;
        for (id, profile) in profiles {
            if !profile.enabled || raw.disabled_teams.iter().any(|disabled| disabled == &id) {
                continue;
            }
            if first_enabled.is_none() {
                first_enabled = Some(id.clone());
            }
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
            personas.insert(
                id.clone(),
                Persona {
                    id,
                    profile,
                    agent,
                    bridge: None,
                },
            );
        }
        // default_id 必须是排序后的第一个 profile（BTreeMap 语义），
        // 而不是 HashMap 的随机迭代首项——否则默认人格会漂移。
        let default_id = first_enabled.unwrap_or_else(|| "default".into());
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

    /// ⚠️ 仅为兼容保留（去主智能体后不再有"默认人格"语义）。
    /// 新代码请按显式 id 解析；本方法不再参与任何路由。
    #[deprecated(note = "there is no default persona any more; resolve by explicit id")]
    #[allow(dead_code)]
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

    /// Resolve target persona。
    ///
    /// 去主智能体（2026-09）：没有"默认人格"回退——未知 id 时回退到**任一**
    /// 已存在人格（仅用于事件/日志上下文，不用于会话路由；路由由
    /// `Agent::apply_command` 的显式 team_id 校验负责）。
    pub fn resolve(&self, agent_id: Option<&str>) -> Persona {
        let personas = self.personas.lock().unwrap();
        if let Some(id) = agent_id {
            if let Some(p) = personas.get(id) {
                return Persona {
                    id: p.id.clone(),
                    profile: p.profile.clone(),
                    agent: Arc::clone(&p.agent),
                    bridge: None,
                };
            }
        }
        let p = personas
            .values()
            .next()
            .expect("at least one persona exists");
        Persona {
            id: p.id.clone(),
            profile: p.profile.clone(),
            agent: Arc::clone(&p.agent),
            bridge: None,
        }
    }

    /// 按 id 解析（未知返回 None）——路由用，绝无兜底。
    pub fn get_exact(&self, id: &str) -> Option<Persona> {
        self.personas.lock().unwrap().get(id).map(|p| Persona {
            id: p.id.clone(),
            profile: p.profile.clone(),
            agent: Arc::clone(&p.agent),
            bridge: None,
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
                bridge: None,
            })
            .collect()
    }
}
