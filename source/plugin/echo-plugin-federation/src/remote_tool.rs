//! 远程代理工具（federation Phase 2）：`Tool` trait 实现，把调用路由到
//! 对端节点执行。
//!
//! 大脑侧视角：`<peer>__<tool>`（如 `gpu-box__bash`）就是一个普通注册
//! 工具，`execute()` 内发 `FedFrame::Invoke` 并等终态；模型显式选择目标
//! 节点，同名本机工具不受影响（RFC §4.1「不做隐式路由」）。
//!
//! 命名规范见 [`echo_defs::remote_tool`]（跨边界契约：注册端、模型侧、
//! 远程子代理三方必须逐字节一致，故词汇在定义层）。本模块只 re-export，
//! 不另存一份实现。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use echo_federation::{FedFrame, Federation};

use crate::router::InvokeRouter;
use echo_tool::{Tool, ToolError};

pub use echo_defs::remote_tool::{remote_tool_name, remote_tool_prefix, sanitize_peer_component};

/// 一个远程工具代理（大脑侧注册名为 `<peer_name>__<tool>`）。
pub struct RemoteTool {
    /// 注册名（`<peer_name>__<tool>`，经 [`remote_tool_name`] 构造）。
    registration_name: String,
    /// 对端本机工具名（`bash` 等，Invoke 帧携带）。
    remote_name: String,
    /// 对端节点 id（路由键）。
    peer_node: String,
    description: String,
    parameters: Value,
    federation: Arc<Federation>,
    router: Arc<InvokeRouter>,
}

impl RemoteTool {
    pub fn new(
        peer_name: &str,
        peer_node: &str,
        remote_name: &str,
        description: String,
        parameters: Value,
        federation: Arc<Federation>,
        router: Arc<InvokeRouter>,
    ) -> Self {
        Self {
            registration_name: remote_tool_name(peer_name, remote_name),
            remote_name: remote_name.to_string(),
            peer_node: peer_node.to_string(),
            description,
            parameters,
            federation,
            router,
        }
    }
}

/// 远程调用的硬超时：本地工具守卫（默认 120s）× 1.5 网络余量，封顶 460s
/// （bash 本机上限 300s + 余量）。对端按其自身上限独立收紧。
const REMOTE_TIMEOUT: Duration = Duration::from_secs(460);

#[async_trait]
impl Tool for RemoteTool {
    fn name(&self) -> &str {
        &self.registration_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn category(&self) -> &'static str {
        "federation"
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    fn timeout_hint(&self, _arguments: &Value) -> Option<Duration> {
        Some(REMOTE_TIMEOUT)
    }

    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        // 1. 注册等待句柄（先注册再发帧，杜绝结果先于注册的竞态）。
        //    显式传入硬超时，router 的 sweep 据此推导 pending 存活上限，
        //    避免误杀仍在飞行的长调用。
        let (request, pending) = self.router.invoke(
            &self.peer_node,
            &self.remote_name,
            arguments,
            Some(REMOTE_TIMEOUT),
        );

        // 2. 经链路发 Invoke；链路断开 → 显式终结 pending（否则条目滞留
        //    到下一次 sweep，等待方还会干等超时）。
        let call_id = pending.call_id.clone();
        if self
            .federation
            .send_to(&self.peer_node, FedFrame::Invoke(request))
            .await
            .is_err()
        {
            self.router.fail_pending(&call_id, "联邦链路发送失败");
            return Err(ToolError::Execution(format!(
                "联邦链路不可用（{}）",
                self.peer_node
            )));
        }
        // 3. 等终态（硬超时兜底；正常情况下 echo-loop 的工具守卫先触发）。
        match tokio::time::timeout(REMOTE_TIMEOUT, pending.wait()).await {
            Ok(Ok(result)) if result.success => Ok(result.output),
            Ok(Ok(result)) => Err(ToolError::Execution(format!(
                "远程工具 {} 执行失败：{}",
                self.registration_name, result.output
            ))),
            Ok(Err(reason)) => Err(ToolError::Execution(format!(
                "远程调用 {} 中断：{reason}",
                self.registration_name
            ))),
            Err(_) => Err(ToolError::Execution(format!(
                "远程工具 {} 超时（>{}s）",
                self.registration_name,
                REMOTE_TIMEOUT.as_secs()
            ))),
        }
    }
}

/// 代理工具的 JSON Schema 来源：对端 caps 只给工具名，参数 schema 从
/// **本机同名工具**复制（执行端就是同一套实现，schema 必然一致；本机未
/// 注册该工具时回退空 object）。
pub fn proxy_schema(
    local_definitions: &[echo_defs::tool::ToolDefinition],
    remote_name: &str,
    peer_label: &str,
) -> (String, Value) {
    let local = local_definitions.iter().find(|d| d.name == remote_name);
    let description = format!(
        "【远程·{peer_label}】{}",
        local
            .map(|d| d.description.as_str())
            .unwrap_or("（对端工具）")
    );
    let parameters = local
        .and_then(|d| d.parameters.clone())
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    (description, parameters)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注册名/前缀必须满足 LLM function name 约束（`^[a-zA-Z0-9_-]{1,64}$`）。
    fn assert_function_name_safe(name: &str) {
        assert!(!name.is_empty(), "empty name");
        assert!(name.len() <= 64, "too long: {name}");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "invalid char in: {name}"
        );
    }

    /// 注册名规范（2026-10 修复）：冒号绝不出现在对 LLM 暴露的名字里。
    #[test]
    fn remote_tool_names_are_llm_function_name_safe() {
        // 常规 peer。
        assert_eq!(remote_tool_name("A100-40g", "bash"), "A100-40g__bash");
        assert_eq!(remote_tool_prefix("A100-40g"), "A100-40g__");
        assert_function_name_safe(&remote_tool_name("A100-40g", "search_code"));

        // 非法字符（冒号/点/空格）→ `_`。
        assert_eq!(sanitize_peer_component("gpu:box"), "gpu_box");
        assert_eq!(sanitize_peer_component("a.b c"), "a_b_c");
        assert_eq!(remote_tool_name("gpu:box", "bash"), "gpu_box__bash");

        // 各类非法 peer 名：不出现冒号、满足 function name 约束。
        for peer in ["a:b", "x y", "节点-1"] {
            let name = remote_tool_name(peer, "read_file");
            assert!(!name.contains(':'), "{name}");
            assert_function_name_safe(&name);
        }

        // 不同 peer 的同一工具不互相覆盖。
        assert_ne!(
            remote_tool_name("core", "bash"),
            remote_tool_name("A100-40g", "bash")
        );
    }
}
