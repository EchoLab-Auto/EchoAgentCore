//! 远程代理工具（federation Phase 2）：`Tool` trait 实现，把调用路由到
//! 对端节点执行。
//!
//! 大脑侧视角：`<peer>:<tool>`（如 `gpu-box:bash`）就是一个普通注册工具，
//! `execute()` 内发 `FedFrame::Invoke` 并等终态；模型显式选择目标节点，
//! 同名本机工具不受影响（RFC §4.1「不做隐式路由」）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use echo_federation::{FedFrame, Federation};

use crate::federation::router::InvokeRouter;
use crate::tool::{Tool, ToolError};

/// 一个远程工具代理（大脑侧注册名为 `<peer_name>:<tool>`）。
pub struct RemoteTool {
    /// 注册名（`<peer_name>:<tool>`）。
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
            registration_name: format!("{peer_name}:{remote_name}"),
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
        let (request, pending) =
            self.router
                .invoke(&self.peer_node, &self.remote_name, arguments, None);

        // 2. 经链路发 Invoke；链路断开 → 立刻失败（pending 已由 drop_peer
        //    或本分支的显式失败覆盖）。
        self.federation
            .send_to(&self.peer_node, FedFrame::Invoke(request))
            .await
            .map_err(|_| ToolError::Execution(format!("联邦链路不可用（{}）", self.peer_node)))?;

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
