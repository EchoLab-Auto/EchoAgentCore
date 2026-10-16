//! adapter_ops 测试共享设施（`#[cfg(test)]`；commands / legacy 两个测试
//! 模块共用，避免 mock 双份漂移）。
//!
//! `MockAdapter` 复刻插件适配器的管理面行为（真实 QQ 实现在独立插件进程，
//! 由插件仓契约测试守护）；`platform` / `gating` 可配——广播泛化用例需要
//! 构造同 / 异 platform 与不同 gating 能力的实例。

use std::sync::{Arc, Mutex};

use echo_adapter::registry::AdapterRegistry;

use crate::agent::Agent;
use crate::config::AgentConfig;
use crate::event::BackendEvent;
use crate::skill::SkillRegistry;
use crate::tool::ToolRegistry;

/// 无操作 provider（管理命令不经 LLM，仅为满足 Agent 构造）。
pub(crate) struct NullProvider;

#[async_trait::async_trait]
impl crate::llm::LlmProvider for NullProvider {
    fn name(&self) -> &str {
        "null"
    }
    fn default_model(&self) -> &str {
        "null"
    }
    async fn chat(
        &self,
        _request: &crate::llm::ChatRequest,
    ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
        Err(crate::llm::LlmError::Config("null provider".into()))
    }
    async fn chat_stream(
        &self,
        _request: &crate::llm::ChatRequest,
        _tx: tokio::sync::mpsc::UnboundedSender<crate::llm::ChatChunk>,
    ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
        Err(crate::llm::LlmError::Config("null provider".into()))
    }
}

/// 测试适配器：实现 `Adapter` + `AdapterManagement`（mock 复刻插件管理面
/// 行为；生产实现由 `echo-adapter-qq` 的契约测试守护，见 W3.2 测试）。
pub(crate) struct MockAdapter {
    name: String,
    platform: String,
    /// gating 能力位（广播裁剪用例：false = 每实例独立门控，不做共享段对齐）。
    gating: bool,
    mode: Mutex<String>,
    owner: Mutex<i64>,
    allow_users: Mutex<Vec<String>>,
    allow_groups: Mutex<Vec<String>>,
    deny_users: Mutex<Vec<String>>,
    deny_groups: Mutex<Vec<String>>,
}

impl MockAdapter {
    /// 指定 platform 与 gating 能力位（广播泛化用例；默认档见
    /// `agent_with_instances` 的调用方——platform `mock` + gating `true`
    /// 与 QQ 插件贡献一致）。
    pub(crate) fn configured(name: &str, platform: &str, gating: bool) -> Self {
        Self {
            name: name.into(),
            platform: platform.into(),
            gating,
            mode: Mutex::new("none".into()),
            owner: Mutex::new(0),
            allow_users: Mutex::new(Vec::new()),
            allow_groups: Mutex::new(Vec::new()),
            deny_users: Mutex::new(Vec::new()),
            deny_groups: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl echo_adapter::Adapter for MockAdapter {
    fn name(&self) -> &str {
        &self.name
    }
    fn display_name(&self) -> &str {
        &self.name
    }
    fn platform(&self) -> &str {
        &self.platform
    }
    fn is_configured(&self) -> bool {
        true
    }
    fn status_info(&self) -> echo_adapter::AdapterInfo {
        echo_adapter::AdapterInfo {
            name: self.name.clone(),
            display_name: self.name.clone(),
            status: echo_adapter::AdapterConnectionState::Connected,
            self_id: Some("123".into()),
            started_at: Some(1),
            platform: self.platform.clone(),
            configured: true,
        }
    }
    async fn start(&self) -> Result<(), echo_adapter::AdapterError> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), echo_adapter::AdapterError> {
        Ok(())
    }
    async fn send_message(
        &self,
        _target: &echo_adapter::MessageTarget,
        _content: &str,
    ) -> Result<echo_adapter::SendResult, echo_adapter::AdapterError> {
        Ok(echo_adapter::SendResult {
            message_id: None,
            success: true,
            error: None,
        })
    }
    fn subscribe(&self, _tx: tokio::sync::mpsc::UnboundedSender<echo_adapter::AdapterEvent>) {}
    fn management(&self) -> Option<&dyn echo_adapter::AdapterManagement> {
        Some(self)
    }
    fn gating_capabilities(&self) -> bool {
        self.gating
    }
}

#[async_trait::async_trait]
impl echo_adapter::AdapterManagement for MockAdapter {
    fn cached_state(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "gate": {
                "mode": *self.mode.lock().unwrap(),
                "allowlist_users": self.allow_users.lock().unwrap().clone(),
                "allowlist_groups": self.allow_groups.lock().unwrap().clone(),
                "denylist_users": self.deny_users.lock().unwrap().clone(),
                "denylist_groups": self.deny_groups.lock().unwrap().clone(),
            },
            "owner": *self.owner.lock().unwrap(),
        }))
    }

    async fn invoke_custom(
        &self,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, echo_adapter::AdapterError> {
        match method {
            "set_gate_mode" => {
                let mode = payload["mode"].as_str().unwrap_or("none");
                *self.mode.lock().unwrap() = mode.to_string();
                Ok(serde_json::json!({"mode": mode}))
            }
            "get_gate_mode" => Ok(serde_json::json!({
                "mode": self.mode.lock().unwrap().clone()
            })),
            "set_owner" => {
                // W2.4：插件 `parse_id` 接受 number|string（mock 复刻）。
                let owner = payload["owner_qq"]
                    .as_i64()
                    .or_else(|| payload["owner_qq"].as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(0);
                *self.owner.lock().unwrap() = owner;
                Ok(serde_json::json!({"owner_qq": owner}))
            }
            "get_owner" => Ok(serde_json::json!({
                "owner_qq": *self.owner.lock().unwrap()
            })),
            // 01a §1.3 / 02a §7.2 C3：插件 `get_state` 返回完整
            // cached_state（mock 复刻插件行为；真实实现由
            // echo-adapter-qq 契约测试守护）。
            "get_state" => self.cached_state().ok_or_else(|| {
                echo_adapter::AdapterError::Internal("no cached state available".into())
            }),
            "update_allowlist" => {
                *self.allow_users.lock().unwrap() = parse_list(&payload, "user_ids");
                *self.allow_groups.lock().unwrap() = parse_list(&payload, "group_ids");
                Ok(serde_json::json!({}))
            }
            "update_denylist" => {
                *self.deny_users.lock().unwrap() = parse_list(&payload, "user_ids");
                *self.deny_groups.lock().unwrap() = parse_list(&payload, "group_ids");
                Ok(serde_json::json!({}))
            }
            "get_filter_config" => Ok(serde_json::json!({
                "allowlist_users": self.allow_users.lock().unwrap().clone(),
                "allowlist_groups": self.allow_groups.lock().unwrap().clone(),
                "denylist_users": self.deny_users.lock().unwrap().clone(),
                "denylist_groups": self.deny_groups.lock().unwrap().clone(),
            })),
            other => Err(echo_adapter::AdapterError::Internal(format!(
                "unknown adapter method: {other}"
            ))),
        }
    }
}

fn parse_list(payload: &serde_json::Value, key: &str) -> Vec<String> {
    payload[key]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| {
                    v.as_i64()
                        .map(|n| n.to_string())
                        .or_else(|| v.as_str().map(|s| s.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 组装一个挂指定实例（name, platform, gating）的 Agent，返回（agent, 事件缓冲）。
pub(crate) fn agent_with_instances(
    specs: &[(&str, &str, bool)],
) -> (Arc<Agent>, Arc<Mutex<Vec<BackendEvent>>>) {
    let registry = AdapterRegistry::new();
    for (name, platform, gating) in specs {
        registry.register(Arc::new(MockAdapter::configured(name, platform, *gating)));
    }
    let agent = Arc::new(Agent::new(
        Arc::new(NullProvider),
        AgentConfig::default(),
        SkillRegistry::new(),
        ToolRegistry::new(),
        Arc::new(registry),
    ));
    let events: Arc<Mutex<Vec<BackendEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    agent.attach_event_sink(Arc::new(move |event: BackendEvent| {
        sink_events.lock().expect("events poisoned").push(event);
    }));
    (agent, events)
}

/// 两实例（`mock-a` / `mock-b`，同 platform + gating）——多数用例的公共装配。
pub(crate) fn agent_with_two_instances() -> (Arc<Agent>, Arc<Mutex<Vec<BackendEvent>>>) {
    agent_with_instances(&[("mock-a", "mock", true), ("mock-b", "mock", true)])
}

/// 取某适配器管理面的 owner（mock `get_owner`）。
pub(crate) async fn owner_of(ad: &Arc<dyn echo_adapter::Adapter>) -> i64 {
    ad.management()
        .expect("management")
        .invoke_custom("get_owner", serde_json::json!({}))
        .await
        .expect("get_owner")["owner_qq"]
        .as_i64()
        .expect("owner_qq number")
}

pub(crate) fn drain_events(events: &Arc<Mutex<Vec<BackendEvent>>>) -> Vec<BackendEvent> {
    std::mem::take(&mut *events.lock().expect("events poisoned"))
}
