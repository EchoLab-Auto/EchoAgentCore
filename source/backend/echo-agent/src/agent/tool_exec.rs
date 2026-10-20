//! 工具执行的宿主侧遗留：只剩**超时口径**。
//!
//! 参数预检、执行、事件配对、出口脱敏全部搬进机制层
//! [`echo_tool::execute`]（2026-11 全插件化 S2）。这里留着的 `tool_timeout`
//! 是**宿主策略**：配置里的 `tool_timeout_secs`（用户可调）+ 工具自声明的
//! `timeout_hint`（工具知道自己的长跑上限）取大，硬上限 600s。机制层不知道
//! "用户的配置默认值是多少"，所以这条留在宿主。
//!
//! 内置循环与 echo-loop 两条驱动路径共用同一口径——否则同一请求换驱动后
//! 行为不同（2026-09-26 前 echo-loop 路径完全没有守卫，挂死的工具会永久
//! 拖住 turn）。

use crate::llm::ToolCall;
use crate::tool::ToolRegistry;

/// 工具超时计算：尊重工具自声明的 `timeout_hint`，外圈守卫长过 hint + 15s，
/// 硬上限 600s；用户配置的 base 不受此限。
pub(crate) async fn tool_timeout(
    tools: &ToolRegistry,
    base: std::time::Duration,
    call: &ToolCall,
) -> std::time::Duration {
    let hinted = match serde_json::from_str::<serde_json::Value>(&call.arguments) {
        Ok(args) => tools
            .timeout_hint(&call.name, &args)
            .await
            .map(|h| h + std::time::Duration::from_secs(15)),
        Err(_) => None,
    };
    hinted
        .map(|h| base.max(h.min(std::time::Duration::from_secs(600))))
        .unwrap_or(base)
}

impl crate::agent::Agent {
    /// 该工具成功调用是否即产生平台可见输出（工具自己声明）。
    pub(crate) async fn tools_produce_visible_output(&self, name: &str) -> bool {
        self.tools.produces_visible_output(name).await
    }

    /// 工具外圈超时守卫：配置 base + 工具自声明 `timeout_hint`。
    pub(crate) async fn tool_guard_timeout(&self, call: &ToolCall) -> std::time::Duration {
        let base = self.model.config.read().await.effective_tool_timeout();
        tool_timeout(&self.tools, base, call).await
    }
}

#[cfg(test)]
mod tests {
    use super::tool_timeout;
    use crate::llm::ToolCall;
    use crate::tool::ToolRegistry;
    use std::time::Duration;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// 无 hint 的工具：用户配置即上限。
    #[tokio::test]
    async fn configured_base_applies_without_a_hint() {
        let registry = ToolRegistry::new();
        let base = Duration::from_secs(30);
        let guard = tool_timeout(&registry, base, &call("unknown", "{}")).await;
        assert_eq!(guard, base);
    }

    /// 非法 JSON 不 panic（参数是模型产的，可能坏）。
    #[tokio::test]
    async fn malformed_arguments_fall_back_to_the_base() {
        let registry = ToolRegistry::new();
        let base = Duration::from_secs(30);
        let guard = tool_timeout(&registry, base, &call("unknown", "{oops")).await;
        assert_eq!(guard, base);
    }
}
