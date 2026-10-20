//! instance_ops 测试设施（`#[cfg(test)]`）。
//!
//! 2026-11 全插件化 S3：命令域（429 行）与它的 mock 已搬进
//! `echo-plugin-tools::instance_ops`（插件侧自带测试）。这里留下的
//! `MockInstance` 只服务 **`bridge` 的测试**——它复刻插件实例的管理面行为
//! （真实 QQ 实现在独立插件进程，由插件仓契约测试守护）。

use std::sync::Mutex;

/// 测试插件实例：实现 `PluginInstance` + `InstanceManagement`（mock 复刻插件管理面
/// 行为；生产实现由 `echo-instance-qq` 的契约测试守护，见 W3.2 测试）。
pub(crate) struct MockInstance {
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

impl MockInstance {
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
impl echo_defs::plugin_instance::PluginInstance for MockInstance {
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
    fn status_info(&self) -> echo_defs::plugin_instance::InstanceInfo {
        echo_defs::plugin_instance::InstanceInfo {
            name: self.name.clone(),
            display_name: self.name.clone(),
            status: echo_defs::plugin_instance::InstanceConnectionState::Connected,
            self_id: Some("123".into()),
            started_at: Some(1),
            platform: self.platform.clone(),
            configured: true,
        }
    }
    async fn start(&self) -> Result<(), echo_defs::plugin_instance::InstanceError> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), echo_defs::plugin_instance::InstanceError> {
        Ok(())
    }
    fn management(&self) -> Option<&dyn echo_defs::plugin_instance::InstanceManagement> {
        Some(self)
    }
    fn gating_capabilities(&self) -> bool {
        self.gating
    }
}

#[async_trait::async_trait]
impl echo_defs::plugin_instance::InstanceManagement for MockInstance {
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
    ) -> Result<serde_json::Value, echo_defs::plugin_instance::InstanceError> {
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
            // echo-instance-qq 契约测试守护）。
            "get_state" => self.cached_state().ok_or_else(|| {
                echo_defs::plugin_instance::InstanceError::Internal(
                    "no cached state available".into(),
                )
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
            other => Err(echo_defs::plugin_instance::InstanceError::Internal(
                format!("unknown instance method: {other}"),
            )),
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
