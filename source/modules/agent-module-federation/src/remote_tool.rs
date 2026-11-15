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
use serde_json::Value;

use crate::{FedFrame, Federation};

use crate::router::InvokeRouter;
use echo_defs::tool::{Tool, ToolError};

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
        // 取消传播守卫（2026-11）：本端不再等（超时 / 外层 turn 取消把
        // future drop 掉）时，必须告诉执行端"别跑了"——此前 Cancel 只被
        // 对端记一条日志，调用方早退后对端的命令仍在跑（尤其 shell 会话：
        // 一条 `cargo build` 会继续占着执行端）。Drop 时尽力发送，成功
        // 终结则解除武装。
        let mut cancel_guard = CancelGuard::new(
            self.federation.clone(),
            self.router.clone(),
            self.peer_node.clone(),
            call_id.clone(),
        );
        if self
            .federation
            .send_to(&self.peer_node, FedFrame::Invoke(request))
            .await
            .is_err()
        {
            self.router.fail_pending(&call_id, "联邦链路发送失败");
            cancel_guard.disarm();
            return Err(ToolError::Execution(format!(
                "联邦链路不可用（{}）",
                self.peer_node
            )));
        }
        // 3. 等终态（硬超时兜底；正常情况下 agent-module-loop 的工具守卫先触发）。
        let outcome = match tokio::time::timeout(REMOTE_TIMEOUT, pending.wait()).await {
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
        };
        if outcome.is_err() {
            // 失败终态已在本端结清：先摘掉可能残留的等待条目（超时路径
            // 的 receiver 已被 drop，条目要等 sweep 才清；这里立即收尾），
            // 再主动发 Cancel——执行端停止等待并回终态（对端可能已完成，
            // Cancel 对已完成调用是 no-op）。
            self.router.fail_pending(&call_id, "调用方停止等待");
            self.send_cancel(&call_id).await;
        }
        cancel_guard.disarm();
        outcome
    }
}

impl RemoteTool {
    /// 尽力发送取消帧（链路不可用 = 对端已不可达，取消本身无意义）。
    async fn send_cancel(&self, call_id: &str) {
        tracing::info!(
            target: "federation",
            peer = %self.peer_node, tool = %self.registration_name, %call_id,
            "sending cancel to executor (caller stopped waiting)"
        );
        let _ = self
            .federation
            .send_to(
                &self.peer_node,
                FedFrame::Cancel {
                    call_id: call_id.to_string(),
                },
            )
            .await;
    }
}

/// Drop 即取消：调用方 future 被外层丢弃（turn 取消 / 短超时）时，
/// 尽力把 `Cancel` 送达执行端。
///
/// 为什么用 drop 而不是显式路径：工具 future 可能被任意上层（`select!`、
/// 超时守卫、turn 取消）drop，唯一一定能跑到的收尾点就是 `Drop`。
struct CancelGuard {
    federation: Arc<Federation>,
    router: Arc<InvokeRouter>,
    peer: String,
    call_id: String,
    armed: bool,
}

impl CancelGuard {
    fn new(
        federation: Arc<Federation>,
        router: Arc<InvokeRouter>,
        peer: String,
        call_id: String,
    ) -> Self {
        Self {
            federation,
            router,
            peer,
            call_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let federation = self.federation.clone();
        let peer = self.peer.clone();
        let call_id = self.call_id.clone();
        // 先结清**本地**等待条目（幂等：正常收尾路径可能已经摘过）。这一步
        // 同步完成——此后本进程的任何 sweep / 观测都不会再看到这条已放弃的
        // 调用；`Cancel` 只是通知执行端，不能替代本地清理。
        self.router
            .fail_pending(&call_id, "调用方停止等待（工具 future 被丢弃）");
        tracing::info!(
            target: "federation",
            %peer, %call_id,
            "remote tool dropped before terminal state — pending cleared, sending cancel"
        );
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let _ = federation
                        .send_to(&peer, FedFrame::Cancel { call_id })
                        .await;
                });
            }
            Err(_) => {
                // 无异步运行时可挂（同步测试 / 关闭期）：放弃发送，
                // 对端的截止时间仍会兜底。
            }
        }
    }
}
/// 代理工具 schema 的**首选**来源：对端握手通告的 `tool_definitions`
/// （2026-11）。
///
/// 远端是唯一知道自己工具参数形状的一方——本地复制同名 schema 只在"两边
/// 同版本同模块"时成立；模块升/降级、按人格启用不同模块、本机压根没装同名
/// 工具时，猜出来的 schema 会把模型引向错误参数（表现为对端一直回参数错误，
/// 而模型以为是自己填错）。
///
/// **绝不猜**：`None` = 对端是旧节点（整张定义表为空）、没通告该工具，或
/// 通告的定义缺参数 schema。调用方必须**跳过**，不得回退本机同名副本，也
/// 不得伪造空 schema 造一个"看起来能调"的代理。链路兼容性不受影响：
/// `node://<peer>/…` 显式调用不依赖代理工具（见 `RemoteAwareTool`）。
pub fn advertised_schema(caps: &crate::NodeCaps, remote_name: &str) -> Option<(String, Value)> {
    let definition = caps
        .tool_definitions
        .iter()
        .find(|definition| definition.name == remote_name)?;
    // 缺参数定义 = 不完整通告：无法知道对端参数形状，视同未通告。
    let parameters = definition.parameters.clone()?;
    Some((definition.description.clone(), parameters))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// 2026-11：对端通告的定义**优先**（远端最清楚自己的参数形状）。
    #[test]
    fn advertised_schema_wins_over_local_copy() {
        let caps = crate::NodeCaps {
            tools: vec!["bash".into()],
            tool_definitions: vec![echo_defs::tool::ToolDefinition {
                name: "bash".into(),
                description: "remote bash".into(),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"],
                })),
            }],
            ..Default::default()
        };
        let (description, parameters) = advertised_schema(&caps, "bash").expect("advertised");
        assert_eq!(description, "remote bash");
        assert_eq!(
            parameters["required"],
            json!(["command"]),
            "必须用对端通告的 schema，而不是本机同名工具的 schema"
        );
        // 对端没通告该工具 → None（调用方必须跳过，不能猜本机同名 schema）。
        assert!(advertised_schema(&caps, "read_file").is_none());
    }

    /// **绝不猜**：旧节点（空定义表）/ 定义缺参数 / 非空定义里缺某个工具
    /// 一律 `None`，由调用方跳过——不伪造空 schema、不复制本机同名副本。
    #[test]
    fn missing_or_incomplete_definitions_are_never_guessed() {
        // ① 旧节点：整张定义表为空（`serde(default)`）。
        assert!(advertised_schema(&crate::NodeCaps::default(), "bash").is_none());

        // ② 定义在，但缺参数 schema（不完整通告）→ 视同未通告。
        let incomplete = crate::NodeCaps {
            tools: vec!["bash".into()],
            tool_definitions: vec![echo_defs::tool::ToolDefinition {
                name: "bash".into(),
                description: "remote bash".into(),
                parameters: None,
            }],
            ..Default::default()
        };
        assert!(
            advertised_schema(&incomplete, "bash").is_none(),
            "缺 schema 的定义不能被伪装成空 object"
        );

        // ③ 非空定义表里没有某个工具 → 该工具同样 None。
        let only_bash = crate::NodeCaps {
            tools: vec!["bash".into(), "read_file".into()],
            tool_definitions: vec![echo_defs::tool::ToolDefinition {
                name: "bash".into(),
                description: "remote bash".into(),
                parameters: Some(json!({"type": "object", "properties": {}})),
            }],
            ..Default::default()
        };
        assert!(advertised_schema(&only_bash, "bash").is_some());
        assert!(advertised_schema(&only_bash, "read_file").is_none());
    }

    /// C（2026-11 复审）：工具 future 被外层丢弃时，Drop 必须先**同步**结清
    /// 本地 pending（`fail_pending` 幂等），再尽力通知执行端 Cancel。
    #[tokio::test]
    async fn cancel_guard_drop_clears_local_pending_immediately() {
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let federation = Federation::new(
            "node-a".into(),
            None,
            crate::NodeCaps::default(),
            Vec::new(),
            None,
            event_tx,
            shutdown_tx,
        );
        let router = InvokeRouter::new("node-a".into());
        let (request, pending) = router.invoke(
            "node-b",
            "bash",
            json!({"command": "sleep 300"}),
            Some(Duration::from_secs(460)),
        );
        let call_id = request.call_id.clone();
        assert_eq!(router.pending_count(), 1);

        let guard = CancelGuard::new(federation, router.clone(), "node-b".into(), call_id.clone());
        drop(guard);
        assert_eq!(
            router.pending_count(),
            0,
            "Drop 必须立即摘除本地 pending，而不是等 sweep"
        );

        // 等待方拿到失败终态（不是链路断开/悬空）。
        let got = tokio::time::timeout(Duration::from_secs(1), pending.wait())
            .await
            .expect("必须有 deadline，不能悬空等待")
            .expect("本地终结应产生终态");
        assert!(!got.success);
        assert!(got.output.contains("停止等待"), "{}", got.output);
    }
}
