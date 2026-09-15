//! Adapter lifecycle tools — start/stop/restart/status.
//! These are LLM-callable tools for managing platform adapters (QQ etc.).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::tool::{Tool, ToolError, ToolRegistry};
use echo_adapter::AdapterRegistry;

// ── AdapterStatusTool ──

pub struct AdapterStatusTool {
    adapters: Arc<AdapterRegistry>,
}

impl AdapterStatusTool {
    pub fn new(adapters: Arc<AdapterRegistry>) -> Self {
        Self { adapters }
    }
}

#[async_trait]
impl Tool for AdapterStatusTool {
    fn name(&self) -> &str {
        "adapter_status"
    }
    fn description(&self) -> &str {
        "Check the status of all platform adapters (QQ etc.). Returns which adapters are running, connected, or stopped."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let infos = self.adapters.list_info();
        if infos.is_empty() {
            return Ok(
                "No adapters configured. Use [adapters.qq] enabled = true in config to enable QQ."
                    .into(),
            );
        }
        let lines: Vec<String> = infos
            .iter()
            .map(|info| {
                let status = match info.status {
                    echo_adapter::AdapterConnectionState::Connected => "connected",
                    echo_adapter::AdapterConnectionState::Disconnected => "running (no client)",
                    echo_adapter::AdapterConnectionState::Stopped => "stopped",
                    echo_adapter::AdapterConnectionState::Starting => "starting",
                    echo_adapter::AdapterConnectionState::Stopping => "stopping",
                    echo_adapter::AdapterConnectionState::Error => "error",
                };
                let id = info.self_id.as_deref().unwrap_or("-");
                format!("{}: {} (self: {})", info.display_name, status, id)
            })
            .collect();
        Ok(lines.join("\n"))
    }
}

// ── AdapterStartTool ──

pub struct AdapterStartTool {
    adapters: Arc<AdapterRegistry>,
}

impl AdapterStartTool {
    pub fn new(adapters: Arc<AdapterRegistry>) -> Self {
        Self { adapters }
    }
}

#[async_trait]
impl Tool for AdapterStartTool {
    fn name(&self) -> &str {
        "adapter_start"
    }
    fn description(&self) -> &str {
        "Start a platform adapter (e.g. QQ). Use 'all' to start all configured adapters."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Adapter name: 'qq' or 'all'"}
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = args["name"].as_str().unwrap_or("all").to_lowercase();
        if name == "all" {
            let results = self.adapters.start_all().await;
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
            match self.adapters.get(&name) {
                Some(adapter) => match adapter.start().await {
                    Ok(()) => Ok(format!("{} adapter started.", adapter.display_name())),
                    Err(e) => Err(ToolError::Execution(e.to_string())),
                },
                None => Err(ToolError::Execution(format!("Adapter '{name}' not found."))),
            }
        }
    }
}

// ── AdapterStopTool ──

pub struct AdapterStopTool {
    adapters: Arc<AdapterRegistry>,
}

impl AdapterStopTool {
    pub fn new(adapters: Arc<AdapterRegistry>) -> Self {
        Self { adapters }
    }
}

#[async_trait]
impl Tool for AdapterStopTool {
    fn name(&self) -> &str {
        "adapter_stop"
    }
    fn description(&self) -> &str {
        "Stop a running platform adapter (e.g. QQ). Use 'all' to stop all running adapters."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Adapter name: 'qq' or 'all'"}
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = args["name"].as_str().unwrap_or("all").to_lowercase();
        if name == "all" {
            self.adapters.stop_all().await;
            Ok("All adapters stopped.".into())
        } else {
            match self.adapters.get(&name) {
                Some(adapter) => match adapter.stop().await {
                    Ok(()) => Ok(format!("{} adapter stopped.", adapter.display_name())),
                    Err(e) => Err(ToolError::Execution(e.to_string())),
                },
                None => Err(ToolError::Execution(format!("Adapter '{name}' not found."))),
            }
        }
    }
}

// ── AdapterRestartTool ──

pub struct AdapterRestartTool {
    adapters: Arc<AdapterRegistry>,
}

impl AdapterRestartTool {
    pub fn new(adapters: Arc<AdapterRegistry>) -> Self {
        Self { adapters }
    }
}

#[async_trait]
impl Tool for AdapterRestartTool {
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
                "name": {"type": "string", "description": "Adapter name, e.g. 'qq'"}
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = args["name"].as_str().unwrap_or("qq").to_lowercase();
        match self.adapters.get(&name) {
            Some(adapter) => {
                // Stop then start.
                if adapter.stop().await.is_err() {
                    // Not running — just start.
                }
                match adapter.start().await {
                    Ok(()) => Ok(format!("{} adapter restarted.", adapter.display_name())),
                    Err(e) => Err(ToolError::Execution(e.to_string())),
                }
            }
            None => Err(ToolError::Execution(format!("Adapter '{name}' not found."))),
        }
    }
}

// ── Registration ──

pub fn register_adapter_tools(registry: &mut ToolRegistry, adapters: Arc<AdapterRegistry>) {
    registry.register(Arc::new(AdapterStatusTool::new(adapters.clone())));
    registry.register(Arc::new(AdapterStartTool::new(adapters.clone())));
    registry.register(Arc::new(AdapterStopTool::new(adapters.clone())));
    registry.register(Arc::new(AdapterRestartTool::new(adapters.clone())));
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_test_utils::MockAdapter;

    fn registry_with(name: &str) -> Arc<AdapterRegistry> {
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(MockAdapter::new(name, true)));
        Arc::new(reg)
    }

    #[tokio::test]
    async fn status_reports_empty_registry() {
        let tool = AdapterStatusTool::new(Arc::new(AdapterRegistry::new()));
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.contains("No adapters"));
    }

    #[tokio::test]
    async fn status_lists_adapter_states() {
        let tool = AdapterStatusTool::new(registry_with("qq"));
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.contains("qq"));
        assert!(out.contains("stopped"));
    }

    #[tokio::test]
    async fn start_single_adapter_by_name() {
        let reg = registry_with("qq");
        let tool = AdapterStartTool::new(reg.clone());
        let out = tool.execute(json!({"name": "qq"})).await.unwrap();
        assert!(out.contains("started"));
        let adapter = reg.get("qq").unwrap();
        assert!(adapter.status_info().status == echo_adapter::AdapterConnectionState::Connected);
    }

    #[tokio::test]
    async fn start_unknown_adapter_errors() {
        let tool = AdapterStartTool::new(registry_with("qq"));
        let err = tool.execute(json!({"name": "telegram"})).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn start_all_starts_every_adapter() {
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(MockAdapter::new("qq", true)));
        reg.register(Arc::new(MockAdapter::new("tg", true)));
        let reg = Arc::new(reg);
        let tool = AdapterStartTool::new(reg.clone());
        let out = tool.execute(json!({"name": "all"})).await.unwrap();
        assert!(out.contains("qq: started"));
        assert!(out.contains("tg: started"));
    }

    #[tokio::test]
    async fn stop_single_adapter() {
        let reg = registry_with("qq");
        let start_tool = AdapterStartTool::new(reg.clone());
        start_tool.execute(json!({"name": "qq"})).await.unwrap();
        let stop_tool = AdapterStopTool::new(reg.clone());
        let out = stop_tool.execute(json!({"name": "qq"})).await.unwrap();
        assert!(out.contains("stopped"));
        let adapter = reg.get("qq").unwrap();
        assert!(adapter.status_info().status == echo_adapter::AdapterConnectionState::Stopped);
    }

    #[tokio::test]
    async fn stop_all_is_always_ok() {
        let tool = AdapterStopTool::new(registry_with("qq"));
        let out = tool.execute(json!({"name": "all"})).await.unwrap();
        assert!(out.contains("All adapters stopped"));
    }

    #[tokio::test]
    async fn restart_stops_then_starts() {
        let reg = registry_with("qq");
        let tool = AdapterRestartTool::new(reg.clone());
        let out = tool.execute(json!({"name": "qq"})).await.unwrap();
        assert!(out.contains("restarted"));
        let adapter = reg.get("qq").unwrap();
        assert!(adapter.status_info().status == echo_adapter::AdapterConnectionState::Connected);
    }

    #[tokio::test]
    async fn restart_unknown_adapter_errors() {
        let tool = AdapterRestartTool::new(registry_with("qq"));
        let err = tool.execute(json!({"name": "nope"})).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
