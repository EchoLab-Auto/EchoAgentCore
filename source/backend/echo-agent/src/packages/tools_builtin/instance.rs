//! PluginInstance lifecycle tools — start/stop/restart/status.
//! These are LLM-callable tools for managing platform instances (QQ etc.).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::tool::{Tool, ToolError, ToolRegistry};
use echo_defs::plugin_instance::InstanceRegistry;

// ── InstanceStatusTool ──

pub struct InstanceStatusTool {
    instances: Arc<InstanceRegistry>,
}

impl InstanceStatusTool {
    pub fn new(instances: Arc<InstanceRegistry>) -> Self {
        Self { instances }
    }
}

#[async_trait]
impl Tool for InstanceStatusTool {
    fn name(&self) -> &str {
        "adapter_status"
    }
    fn description(&self) -> &str {
        "Check the status of all platform adapters. Returns which adapters are running, connected, or stopped."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let infos = self.instances.list_info();
        if infos.is_empty() {
            return Ok(
                "No adapters configured. Enable one under [adapters.<platform>] in the config."
                    .into(),
            );
        }
        let lines: Vec<String> = infos
            .iter()
            .map(|info| {
                let status = match info.status {
                    echo_defs::plugin_instance::InstanceConnectionState::Connected => "connected",
                    echo_defs::plugin_instance::InstanceConnectionState::Disconnected => {
                        "running (no client)"
                    }
                    echo_defs::plugin_instance::InstanceConnectionState::Stopped => "stopped",
                    echo_defs::plugin_instance::InstanceConnectionState::Starting => "starting",
                    echo_defs::plugin_instance::InstanceConnectionState::Stopping => "stopping",
                    echo_defs::plugin_instance::InstanceConnectionState::Error => "error",
                };
                let id = info.self_id.as_deref().unwrap_or("-");
                format!("{}: {} (self: {})", info.display_name, status, id)
            })
            .collect();
        Ok(lines.join("\n"))
    }
}

// ── InstanceStartTool ──

pub struct InstanceStartTool {
    instances: Arc<InstanceRegistry>,
}

impl InstanceStartTool {
    pub fn new(instances: Arc<InstanceRegistry>) -> Self {
        Self { instances }
    }
}

#[async_trait]
impl Tool for InstanceStartTool {
    fn name(&self) -> &str {
        "adapter_start"
    }
    fn description(&self) -> &str {
        "Start a platform adapter. Use 'all' to start all configured adapters."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "PluginInstance instance name, or 'all'"}
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = args["name"].as_str().unwrap_or("all").to_lowercase();
        if name == "all" {
            let results = self.instances.start_all().await;
            let lines: Vec<String> = results
                .iter()
                .map(|(n, r)| match r {
                    Ok(()) => format!("{}: started", n),
                    Err(e) => format!("{}: {}", n, e),
                })
                .collect();
            Ok(if lines.is_empty() {
                "No adapters to start.".into()
            } else {
                lines.join("\n")
            })
        } else {
            match self.instances.get(&name) {
                Some(instance) => match instance.start().await {
                    Ok(()) => Ok(format!("{} adapter started.", instance.display_name())),
                    Err(e) => Err(ToolError::Execution(e.to_string())),
                },
                None => Err(ToolError::Execution(format!(
                    "PluginInstance '{name}' not found."
                ))),
            }
        }
    }
}

// ── InstanceStopTool ──

pub struct InstanceStopTool {
    instances: Arc<InstanceRegistry>,
}

impl InstanceStopTool {
    pub fn new(instances: Arc<InstanceRegistry>) -> Self {
        Self { instances }
    }
}

#[async_trait]
impl Tool for InstanceStopTool {
    fn name(&self) -> &str {
        "adapter_stop"
    }
    fn description(&self) -> &str {
        "Stop a running platform adapter. Use 'all' to stop all running adapters."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "PluginInstance instance name, or 'all'"}
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = args["name"].as_str().unwrap_or("all").to_lowercase();
        if name == "all" {
            self.instances.stop_all().await;
            Ok("All adapters stopped.".into())
        } else {
            match self.instances.get(&name) {
                Some(instance) => match instance.stop().await {
                    Ok(()) => Ok(format!("{} adapter stopped.", instance.display_name())),
                    Err(e) => Err(ToolError::Execution(e.to_string())),
                },
                None => Err(ToolError::Execution(format!(
                    "PluginInstance '{name}' not found."
                ))),
            }
        }
    }
}

// ── InstanceRestartTool ──

pub struct InstanceRestartTool {
    instances: Arc<InstanceRegistry>,
}

impl InstanceRestartTool {
    pub fn new(instances: Arc<InstanceRegistry>) -> Self {
        Self { instances }
    }
}

#[async_trait]
impl Tool for InstanceRestartTool {
    fn name(&self) -> &str {
        "adapter_restart"
    }
    fn description(&self) -> &str {
        "Restart a platform adapter (stop then start). Useful when the adapter is not responding."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Adapter instance name (omit when only one adapter exists)"}
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = resolve_instance_name(&self.instances, &args)?;
        match self.instances.get(&name) {
            Some(instance) => {
                // Stop then start.
                if instance.stop().await.is_err() {
                    // Not running — just start.
                }
                match instance.start().await {
                    Ok(()) => Ok(format!("{} adapter restarted.", instance.display_name())),
                    Err(e) => Err(ToolError::Execution(e.to_string())),
                }
            }
            None => Err(ToolError::Execution(format!(
                "PluginInstance '{name}' not found."
            ))),
        }
    }
}

/// Resolve the target instance name for a tool call.
///
/// `name` 参数缺省时**不猜平台名**：只有一个插件实例就用它，多个则报错列出
/// 可选项（此前缺省写死 `"qq"`，等于假设部署里一定有 QQ）。
fn resolve_instance_name(instances: &InstanceRegistry, args: &Value) -> Result<String, ToolError> {
    if let Some(name) = args["name"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(name.to_lowercase());
    }
    let names = instances.names();
    match names.len() {
        1 => Ok(names[0].to_lowercase()),
        0 => Err(ToolError::Execution("No adapters configured.".to_string())),
        _ => Err(ToolError::Execution(format!(
            "`name` is required when multiple adapters exist (available: {}).",
            names.join(", ")
        ))),
    }
}

// ── Registration ──

pub fn register_instance_tools(registry: &mut ToolRegistry, instances: Arc<InstanceRegistry>) {
    registry.register(Arc::new(InstanceStatusTool::new(instances.clone())));
    registry.register(Arc::new(InstanceStartTool::new(instances.clone())));
    registry.register(Arc::new(InstanceStopTool::new(instances.clone())));
    registry.register(Arc::new(InstanceRestartTool::new(instances.clone())));
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_test_utils::MockInstance;

    fn registry_with(name: &str) -> Arc<InstanceRegistry> {
        let reg = InstanceRegistry::new();
        reg.register(Arc::new(MockInstance::new(name, true)));
        Arc::new(reg)
    }

    #[tokio::test]
    async fn status_reports_empty_registry() {
        let tool = InstanceStatusTool::new(Arc::new(InstanceRegistry::new()));
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.contains("No adapters"));
    }

    #[tokio::test]
    async fn status_lists_instance_states() {
        let tool = InstanceStatusTool::new(registry_with("qq"));
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.contains("qq"));
        assert!(out.contains("stopped"));
    }

    #[tokio::test]
    async fn start_single_instance_by_name() {
        let reg = registry_with("qq");
        let tool = InstanceStartTool::new(reg.clone());
        let out = tool.execute(json!({"name": "qq"})).await.unwrap();
        assert!(out.contains("started"));
        let instance = reg.get("qq").unwrap();
        assert!(
            instance.status_info().status
                == echo_defs::plugin_instance::InstanceConnectionState::Connected
        );
    }

    #[tokio::test]
    async fn start_unknown_instance_errors() {
        let tool = InstanceStartTool::new(registry_with("qq"));
        let err = tool.execute(json!({"name": "telegram"})).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn start_all_starts_every_instance() {
        let reg = InstanceRegistry::new();
        reg.register(Arc::new(MockInstance::new("qq", true)));
        reg.register(Arc::new(MockInstance::new("tg", true)));
        let reg = Arc::new(reg);
        let tool = InstanceStartTool::new(reg.clone());
        let out = tool.execute(json!({"name": "all"})).await.unwrap();
        assert!(out.contains("qq: started"));
        assert!(out.contains("tg: started"));
    }

    #[tokio::test]
    async fn stop_single_instance() {
        let reg = registry_with("qq");
        let start_tool = InstanceStartTool::new(reg.clone());
        start_tool.execute(json!({"name": "qq"})).await.unwrap();
        let stop_tool = InstanceStopTool::new(reg.clone());
        let out = stop_tool.execute(json!({"name": "qq"})).await.unwrap();
        assert!(out.contains("stopped"));
        let instance = reg.get("qq").unwrap();
        assert!(
            instance.status_info().status
                == echo_defs::plugin_instance::InstanceConnectionState::Stopped
        );
    }

    #[tokio::test]
    async fn stop_all_is_always_ok() {
        let tool = InstanceStopTool::new(registry_with("qq"));
        let out = tool.execute(json!({"name": "all"})).await.unwrap();
        assert!(out.contains("All adapters stopped"));
    }

    #[tokio::test]
    async fn restart_stops_then_starts() {
        let reg = registry_with("qq");
        let tool = InstanceRestartTool::new(reg.clone());
        let out = tool.execute(json!({"name": "qq"})).await.unwrap();
        assert!(out.contains("restarted"));
        let instance = reg.get("qq").unwrap();
        assert!(
            instance.status_info().status
                == echo_defs::plugin_instance::InstanceConnectionState::Connected
        );
    }

    #[tokio::test]
    async fn restart_unknown_instance_errors() {
        let tool = InstanceRestartTool::new(registry_with("qq"));
        let err = tool.execute(json!({"name": "nope"})).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
