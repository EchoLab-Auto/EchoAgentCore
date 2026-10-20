//! 工具管理面命令域：`RequestToolsList` + `ToggleTool`。
//!
//! ## 为什么这两条归本插件
//!
//! 工具清单与启停状态是**本插件拥有**的东西（`BuiltinToolProvider` 装进注册
//! 表，包标签也由它打）。宿主侧的 `emit_tools_list` 与 `ToggleTool` 却各留
//! 一份"怎么投影、怎么持久化"的知识——与技能域同一个形状的问题。
//!
//! ## 宿主接缝 = 三项能力
//!
//! | 方法 | 为什么在宿主 |
//! |---|---|
//! | [`ToolsCommandHost::registry`] | 注册表是 per-persona 的（`Agent` 持有） |
//! | [`ToolsCommandHost::set_enabled`] | 启停要持久化 `disabled_tools` + 全局策略 + 逐人格重算 |
//! | [`ToolsCommandHost::emit`] | 面板事件出口 |
//!
//! 清单投影（读 `full_definitions` → `ToolInfo`）在插件里——那是"注册表长
//! 什么样"的知识，与工具装配同源。

use std::sync::Arc;

use echo_protocol::{BackendCommand, BackendEvent, ToolInfo};
use echo_tool::ToolRegistry;

/// 本插件处理的命令名。
pub const COMMANDS: [&str; 2] = ["RequestToolsList", "ToggleTool"];

/// 宿主接缝：工具管理面需要的三件事。
pub trait ToolsCommandHost: Send + Sync {
    /// 本 persona 的工具注册表。
    fn registry(&self) -> Arc<ToolRegistry>;
    /// 启停一个工具（改注册表 + 持久化 + 全局策略 + 逐人格重算）。
    /// 返回 `false` = 工具不存在。
    fn set_enabled(
        &self,
        name: &str,
        enabled: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>>;
    /// 事件出口。
    fn emit(&self, event: BackendEvent);
}

/// 命令处理器。
pub struct ToolsCommandHandler {
    host: Arc<dyn ToolsCommandHost>,
}

impl ToolsCommandHandler {
    pub fn new(host: Arc<dyn ToolsCommandHost>) -> Self {
        Self { host }
    }

    /// 处理一条命令；`None` = 不是本域。
    pub async fn handle(
        &self,
        name: &str,
        payload: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        if !COMMANDS.contains(&name) {
            return None;
        }
        let tagged = serde_json::json!({ name: payload });
        let cmd: BackendCommand = match serde_json::from_value(tagged) {
            Ok(cmd) => cmd,
            Err(error) => return Some(Err(format!("tools 命令载荷解析失败（{name}）: {error}"))),
        };
        Some(self.apply(cmd).await.map(|()| serde_json::Value::Null))
    }

    async fn apply(&self, cmd: BackendCommand) -> Result<(), String> {
        match cmd {
            BackendCommand::RequestToolsList => {
                self.emit_list().await;
                Ok(())
            }
            BackendCommand::ToggleTool { name, enabled } => {
                let ok = self.host.set_enabled(&name, enabled).await;
                if ok {
                    self.emit_list().await;
                }
                self.notice(if ok {
                    format!(
                        "tool {name} {}",
                        if enabled { "enabled" } else { "disabled" }
                    )
                } else {
                    format!("tool {name} not found")
                });
                Ok(())
            }
            other => Err(format!(
                "tools 命令域声明了 {} 但处理器没有对应分支",
                other.kind_name()
            )),
        }
    }

    /// 广播工具清单（含停用项——面板要能重新勾上）。
    ///
    /// 清单直接来自注册表（模型可见工具的**唯一真源**；`spawn_subagent` 由
    /// 组合根注册、按包标签随 subagent 插件门控）。
    async fn emit_list(&self) {
        let definitions = self.host.registry().full_definitions().await;
        let mut tools: Vec<ToolInfo> = definitions
            .into_iter()
            .map(|(definition, category, enabled, package)| ToolInfo {
                name: definition.name,
                description: definition.description,
                parameters: definition.parameters.unwrap_or(serde_json::Value::Null),
                category,
                enabled,
                package,
            })
            .collect();
        tools.sort_by_key(|tool| tool.name.clone());
        self.host.emit(BackendEvent::ToolsList { tools });
    }

    fn notice(&self, message: impl Into<String>) {
        self.host.emit(BackendEvent::Error {
            session_id: None,
            message: message.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeHost {
        registry: Arc<ToolRegistry>,
        events: Mutex<Vec<BackendEvent>>,
        toggles: Mutex<Vec<(String, bool)>>,
    }

    impl ToolsCommandHost for FakeHost {
        fn registry(&self) -> Arc<ToolRegistry> {
            Arc::clone(&self.registry)
        }
        fn set_enabled(
            &self,
            name: &str,
            enabled: bool,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            self.toggles
                .lock()
                .unwrap()
                .push((name.to_string(), enabled));
            let found = name != "ghost";
            Box::pin(async move { found })
        }
        fn emit(&self, event: BackendEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn handler() -> (ToolsCommandHandler, Arc<FakeHost>) {
        let host = Arc::new(FakeHost {
            registry: Arc::new(ToolRegistry::new()),
            events: Mutex::new(Vec::new()),
            toggles: Mutex::new(Vec::new()),
        });
        (ToolsCommandHandler::new(host.clone()), host)
    }

    /// 空注册表也回**空清单**（面板显示"没有工具"，不是报错）。
    #[tokio::test]
    async fn list_is_emitted_even_when_empty() {
        let (h, host) = handler();
        h.handle("RequestToolsList", serde_json::Value::Null)
            .await
            .expect("本域命令")
            .expect("空清单不是错误");
        assert!(matches!(
            host.events.lock().unwrap().as_slice(),
            [BackendEvent::ToolsList { tools }] if tools.is_empty()
        ));
    }

    /// 切换成功 → 广播清单；失败 → 只回 not found（不广播）。
    #[tokio::test]
    async fn toggle_reports_and_broadcasts_only_on_success() {
        let (h, host) = handler();
        h.handle("ToggleTool", serde_json::json!({"name": "bash", "enabled": false}))
            .await
            .expect("本域命令")
            .expect("ok");
        h.handle("ToggleTool", serde_json::json!({"name": "ghost", "enabled": true}))
            .await
            .expect("本域命令")
            .expect("ok");
        let events = host.events.lock().unwrap();
        let lists = events
            .iter()
            .filter(|e| matches!(e, BackendEvent::ToolsList { .. }))
            .count();
        assert_eq!(lists, 1, "只有成功那次广播清单");
        assert!(events.iter().any(|e| matches!(
            e,
            BackendEvent::Error { message, .. } if message.contains("ghost not found")
        )));
    }

    /// 外部命令不认领。
    #[tokio::test]
    async fn foreign_commands_are_not_claimed() {
        let (h, _host) = handler();
        assert!(h
            .handle("RequestSkillsList", serde_json::json!({}))
            .await
            .is_none());
    }
}
