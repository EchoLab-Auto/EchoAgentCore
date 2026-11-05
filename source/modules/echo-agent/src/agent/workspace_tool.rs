//! `workspace` 工具的**宿主侧代理**。
//!
//! 工具实现已在模块仓（`echo-module-workspace` 的 `tool` 模块）；宿主这边只留
//! 一个转发壳。为什么不能省掉壳：
//!
//! 1. 工具清单是宿主的注册表（`ToolRegistry`），模型看到的是那里的条目；
//! 2. 工具调用带 [`ToolContext`]（`team_id` / `session_id`），而模块侧拿到的
//!    只有命令载荷——**上下文得由宿主补进载荷**，模块才能按 persona 取 store。
//!
//! 描述与参数 schema 直接取自模块 crate（`tool::description` /
//! `tool::parameters`），所以模型看到的工具面与模块实现只可能有一个真值。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use echo_defs::tool::{Tool, ToolContext, ToolError, ToolResult};

use crate::modules::ModuleHost;

/// 转发到工作区模块 `workspace` 工具的宿主代理。
pub struct WorkspaceTool {
    host: Arc<ModuleHost>,
}

impl WorkspaceTool {
    /// 用模块宿主构造（调用时按当前 persona 取 `team_id`）。
    pub fn new(host: Arc<ModuleHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl Tool for WorkspaceTool {
    fn name(&self) -> &str {
        echo_module_workspace::tool::TOOL_NAME
    }

    fn description(&self) -> &str {
        echo_module_workspace::tool::description()
    }

    fn category(&self) -> &'static str {
        // 与模块贡献的 category 对齐（面板里归入「模块」分组）。
        "module"
    }

    fn parameters(&self) -> Value {
        echo_module_workspace::tool::parameters()
    }

    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        Ok(self.execute_rich(arguments).await?.text)
    }

    async fn execute_rich(&self, arguments: Value) -> Result<ToolResult, ToolError> {
        // 无上下文的直接调用（测试 / 进程级）：team_id 缺失由模块回明确错误。
        self.execute_in(&ToolContext::default(), arguments).await
    }

    async fn execute_in(
        &self,
        context: &ToolContext,
        arguments: Value,
    ) -> Result<ToolResult, ToolError> {
        // 上下文随载荷走：模块是唯一的 store 所有者，只能自己按 team_id 定位。
        let mut payload = arguments;
        if let (Some(team_id), Some(object)) = (context.team_id.as_deref(), payload.as_object_mut())
        {
            object.insert("team_id".into(), Value::String(team_id.to_string()));
        }
        let outcome = self
            .host
            .dispatch_raw(echo_module_workspace::commands::TOOL_CALL, payload)
            .await;
        match outcome {
            // 模块未挂载 / 未声明该入口：与迁移前"工具不可用"一致。
            None => Err(ToolError::Execution(
                "workspace 模块未启用（该智能体未挂载工作区会话管理）".into(),
            )),
            Some(Ok(value)) => Ok(ToolResult::text(
                value
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    // 模块没给 content = 契约破坏；把原始 JSON 交出去好过吞掉。
                    .unwrap_or_else(|| value.to_string()),
            )),
            Some(Err(message)) => Err(ToolError::Execution(message)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 工具面（名称 / 描述 / 参数）直接取自模块，宿主没有第二份真值。
    #[test]
    fn facade_reuses_module_metadata() {
        let tool = WorkspaceTool::new(Arc::new(ModuleHost::new()));
        assert_eq!(tool.name(), echo_module_workspace::tool::TOOL_NAME);
        assert_eq!(
            tool.description(),
            echo_module_workspace::tool::description()
        );
        assert_eq!(tool.parameters(), echo_module_workspace::tool::parameters());
        assert_eq!(tool.category(), "module");
    }

    /// 模块未挂载：明确错误（不是 panic，也不是空成功）。
    #[tokio::test]
    async fn missing_module_is_an_explicit_error() {
        let tool = WorkspaceTool::new(Arc::new(ModuleHost::new()));
        let error = tool
            .execute_in(
                &ToolContext {
                    team_id: Some("assistant".into()),
                    ..Default::default()
                },
                serde_json::json!({ "operation": "list" }),
            )
            .await
            .expect_err("未挂载即失败");
        assert!(matches!(error, ToolError::Execution(_)), "{error:?}");
    }
}
