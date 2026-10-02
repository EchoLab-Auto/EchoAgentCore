//! 调用调度器（federation Phase 2）：call_id 路由表 + 执行端裁决。
//!
//! 两个方向共用一张 `DashMap<call_id, PendingCall>`：
//!
//! **大脑侧（出站）**：`RemoteTool` 经 [`InvokeRouter::invoke`] 注册等待句柄，
//! 路由泵（组合根消费 `LinkEvent::Frame` 并调用 [`InvokeRouter::dispatch_frame`]）
//! 把 `InvokeAccepted/InvokeOutput/InvokeResult` 派发给等待者。
//!
//! **执行端（入站）**：[`InvokeRouter::handle_invoke`] 收对端 `Invoke` →
//! per-peer 白名单裁决（[`ExecutorPolicy`]）→ 本机 `ToolRegistry::execute`
//! → 回传 `InvokeAccepted + InvokeResult`。路径校验等安全边界全部复用本机
//! 工具实现（执行端工作区根内 canonical 检查），本层不另做。

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot};

use echo_federation::{
    new_call_id, FedFrame, InvokeRequest, InvokeResult, InvokeVerdict, NodeCaps,
};

use crate::tool::ToolRegistry;

/// 等待中的出站调用：中间输出聚合 + 终态通知。
struct PendingCall {
    peer: String,
    #[allow(dead_code)] // 观测/审计预留（v1 仅日志未消费）
    tool: String,
    started: Instant,
    chunks: mpsc::Sender<String>,
    done: oneshot::Sender<InvokeResult>,
}

/// 出站调用的接收端（`RemoteTool` 持有）。
pub struct PendingInvoke {
    pub call_id: String,
    chunks: mpsc::Receiver<String>,
    done: oneshot::Receiver<InvokeResult>,
}

impl PendingInvoke {
    /// 等终态（调用方应包一层超时——工具守卫超时的 ×1.5 网络余量）。
    /// 聚合的中间输出（bash/shell 流式）追加在终态 output 之前。
    pub async fn wait(mut self) -> Result<InvokeResult, String> {
        match self.done.await {
            Ok(mut result) => {
                let mut buffered = String::new();
                while let Ok(chunk) = self.chunks.try_recv() {
                    buffered.push_str(&chunk);
                }
                if !buffered.is_empty() {
                    result.output = format!("{buffered}{}", result.output);
                }
                Ok(result)
            }
            Err(_) => Err("链路在对端回复前断开".into()),
        }
    }
}

/// 执行端 per-peer 裁决策略（`[federation.peers.*]` 配置物化）。
#[derive(Debug, Clone, Default)]
pub struct ExecutorPolicy {
    /// 允许对端调用的本机工具（`*` = 全部）。
    pub allow_tools: Vec<String>,
    /// 命中列表的调用拒绝并提示需人工确认（v1 简化为拒绝；
    /// 确认通道（Panel 弹层）后续迭代）。
    pub require_confirm: Vec<String>,
    /// 允许对端的只读查询种类（Phase 5）：`node_status` 恒允许
    /// （无害遥测），其余（`session_snapshot`/`workspace_files`）需
    /// 显式列出——会话内容与文件系统敏感。
    pub allow_queries: Vec<String>,
}

impl ExecutorPolicy {
    /// 查询授权：`node_status` 恒允许；其余需显式列出（`*` 全放行）。
    pub fn query_allowed(&self, kind: echo_federation::QueryKind) -> bool {
        use echo_federation::QueryKind;
        if matches!(kind, QueryKind::NodeStatus) {
            return true;
        }
        let key = match kind {
            QueryKind::SessionSnapshot => "session_snapshot",
            QueryKind::WorkspaceFiles => "workspace_files",
            QueryKind::NodeStatus => unreachable!(),
        };
        self.allow_queries.iter().any(|q| q == "*" || q == key)
    }
}

impl ExecutorPolicy {
    fn verdict_for(&self, tool: &str) -> InvokeVerdict {
        if self.require_confirm.iter().any(|t| t == tool) {
            return InvokeVerdict::Rejected;
        }
        let allowed = self.allow_tools.iter().any(|t| t == "*" || t == tool);
        if allowed {
            InvokeVerdict::Accepted
        } else {
            InvokeVerdict::Rejected
        }
    }
}

/// 联邦调用调度器（组合根持有一份）。
pub struct InvokeRouter {
    /// 出站等待表：call_id → pending。
    pending: DashMap<String, PendingCall>,
    /// 执行端策略表：peer node_id → 白名单。
    policies: DashMap<String, ExecutorPolicy>,
    /// 本机 node_id（回环帧拒绝）。
    local_node: String,
}

impl InvokeRouter {
    pub fn new(local_node: String) -> Arc<Self> {
        Arc::new(Self {
            pending: DashMap::new(),
            policies: DashMap::new(),
            local_node,
        })
    }

    /// 注册/更新某 peer 的执行端策略（握手 Up 时按 peer 配置写入；
    /// 无配置 peer = 全拒）。
    pub fn set_policy(&self, peer_node: &str, policy: ExecutorPolicy) {
        self.policies.insert(peer_node.to_string(), policy);
    }

    pub fn drop_peer(&self, peer_node: &str) {
        self.policies.remove(peer_node);
        // 断链清理：该 peer 的出站等待一律以失败终态唤醒。
        let doomed: Vec<String> = self
            .pending
            .iter()
            .filter(|e| e.value().peer == peer_node)
            .map(|e| e.key().clone())
            .collect();
        for call_id in doomed {
            if let Some((_, pending)) = self.pending.remove(&call_id) {
                let _ = pending.done.send(InvokeResult {
                    call_id: call_id.clone(),
                    success: false,
                    output: format!("链路断开（peer {peer_node}）"),
                    elapsed_ms: pending.started.elapsed().as_millis() as u64,
                });
            }
        }
    }

    /// 出站：注册调用并返回 `(call_id, 等待句柄)`——`RemoteTool` 随后经
    /// `Federation::send_to` 发 `Invoke` 帧。
    pub fn invoke(
        self: &Arc<Self>,
        peer_node: &str,
        tool: &str,
        args: serde_json::Value,
        timeout: Option<Duration>,
    ) -> (InvokeRequest, PendingInvoke) {
        let call_id = new_call_id(&self.local_node);
        let (chunk_tx, chunk_rx) = mpsc::channel(64);
        let (done_tx, done_rx) = oneshot::channel();
        self.pending.insert(
            call_id.clone(),
            PendingCall {
                peer: peer_node.to_string(),
                tool: tool.to_string(),
                started: Instant::now(),
                chunks: chunk_tx,
                done: done_tx,
            },
        );
        (
            InvokeRequest {
                call_id: call_id.clone(),
                tool: tool.to_string(),
                args,
                workdir: None,
                timeout_secs: timeout.map(|d| d.as_secs()),
            },
            PendingInvoke {
                call_id,
                chunks: chunk_rx,
                done: done_rx,
            },
        )
    }

    /// 某 peer 的执行端策略快照（查询授权用）。
    pub fn policy_of(&self, peer_node: &str) -> Option<ExecutorPolicy> {
        self.policies.get(peer_node).map(|p| p.clone())
    }

    /// 出站只读查询（Phase 5）：复用 invoke 的等待机制，返回
    /// `(QueryRequest, PendingInvoke)`——调用方经 `Federation::send_to`
    /// 发 `Query` 帧，结果经 `QueryResult` 回流传入 `PendingInvoke`。
    pub fn query(
        self: &Arc<Self>,
        peer_node: &str,
        kind: echo_federation::QueryKind,
        subject: String,
        since_seq: u64,
        limit: u32,
    ) -> (echo_federation::QueryRequest, PendingInvoke) {
        let call_id = new_call_id(&self.local_node);
        let (chunk_tx, chunk_rx) = mpsc::channel(64);
        let (done_tx, done_rx) = oneshot::channel();
        self.pending.insert(
            call_id.clone(),
            PendingCall {
                peer: peer_node.to_string(),
                tool: format!("query:{kind:?}"),
                started: Instant::now(),
                chunks: chunk_tx,
                done: done_tx,
            },
        );
        (
            echo_federation::QueryRequest {
                call_id: call_id.clone(),
                kind,
                subject,
                since_seq,
                limit,
            },
            PendingInvoke {
                call_id,
                chunks: chunk_rx,
                done: done_rx,
            },
        )
    }

    /// 入站帧派发（链路泵每帧调用一次；返回需要回传的帧）。
    ///
    /// - `Invoke` → 执行端路径：裁决 + 异步执行，返回 `InvokeAccepted`（结果
    ///   执行完后经返回的 future 产出，组合根负责发送——见
    ///   [`InvokeRouter::handle_invoke`]，本函数只收非 Invoke 帧）
    /// - `InvokeAccepted/InvokeOutput/InvokeResult` → 大脑侧：派发给等待者
    pub fn dispatch_frame(&self, from: &str, frame: &FedFrame) {
        match frame {
            FedFrame::InvokeAccepted { call_id, verdict } => {
                if let Some(pending) = self.pending.get(call_id) {
                    if *verdict == InvokeVerdict::Rejected {
                        drop(pending);
                        if let Some((_, pending)) = self.pending.remove(call_id) {
                            let _ = pending.done.send(InvokeResult {
                                call_id: call_id.clone(),
                                success: false,
                                output: "对端拒绝执行（白名单/门控）".into(),
                                elapsed_ms: pending.started.elapsed().as_millis() as u64,
                            });
                        }
                    }
                }
            }
            FedFrame::InvokeOutput {
                call_id,
                chunk,
                stream: _,
            } => {
                if let Some(pending) = self.pending.get(call_id) {
                    let _ = pending.chunks.try_send(chunk.clone());
                }
            }
            FedFrame::InvokeResult(result) => {
                if let Some((_, pending)) = self.pending.remove(&result.call_id) {
                    let _ = pending.done.send(result.clone());
                }
            }
            FedFrame::QueryResult(result) => {
                if let Some((_, pending)) = self.pending.remove(&result.call_id) {
                    let _ = pending.done.send(InvokeResult {
                        call_id: result.call_id.clone(),
                        success: result.success,
                        output: result.payload.to_string(),
                        elapsed_ms: 0,
                    });
                }
            }
            FedFrame::Cancel { call_id } => {
                // 执行端取消（v1：尽力而为——本机工具执行多数不可中断，
                // bash 有自身超时兜底；记录审计即可）。
                tracing::info!(target: "federation", from = %from, %call_id, "cancel received (best-effort)");
            }
            _ => {}
        }
    }

    /// 执行端入口：收 `Invoke` → 裁决 →（接受则）spawn 本机执行。
    ///
    /// 返回立即要回传的帧（Accepted/Rejected）；接受时第二个元素为执行
    /// future——完成后产出 `InvokeResult` 帧，组合根负责发回对端。
    pub fn handle_invoke(
        self: &Arc<Self>,
        from: &str,
        request: InvokeRequest,
        registry: Arc<ToolRegistry>,
        send_result: mpsc::Sender<(String, FedFrame)>,
        workspace_roots: &[std::path::PathBuf],
    ) -> FedFrame {
        let call_id = request.call_id.clone();
        // 回环帧：origin 是本机（跨机回环，恶意或误配）——拒绝。
        if echo_federation::call_origin(&call_id) == self.local_node {
            return FedFrame::Error {
                call_id: Some(call_id),
                code: echo_federation::FedError::LoopDetected,
                message: "call_id origin is local node".into(),
            };
        }
        let verdict = self
            .policies
            .get(from)
            .map(|p| p.verdict_for(&request.tool))
            .unwrap_or(InvokeVerdict::Rejected);
        tracing::info!(
            target: "federation",
            from = %from, call_id = %call_id, tool = %request.tool,
            verdict = ?verdict, "invoke verdict"
        );
        if verdict != InvokeVerdict::Accepted {
            return FedFrame::InvokeAccepted {
                call_id: request.call_id,
                verdict,
            };
        }
        // 沙箱裁决（2026-10 安全收紧）：联邦路径的文件类工具，绝对路径
        // 必须落在执行端工作区根并集内——否则获授 read_file 的 peer 可读
        // ~/.ssh 等任意路径，allow_tools 白名单形同虚设。本地多仓库工作流
        // 的"绝对路径显式意图"例外不适用于跨机远程调用。
        const FILE_TOOLS: &[&str] = &["read_file", "write_file", "edit_file"];
        if FILE_TOOLS.contains(&request.tool.as_str()) {
            if let Some(raw) = request.args.get("path").and_then(|v| v.as_str()) {
                if raw.starts_with('/')
                    && !crate::federation::path_within_roots(raw, workspace_roots)
                {
                    tracing::warn!(
                        target: "federation",
                        from = %from, call_id = %call_id, tool = %request.tool, path = %raw,
                        "federation absolute path outside workspace roots — rejected"
                    );
                    return FedFrame::Error {
                        call_id: Some(call_id),
                        code: echo_federation::FedError::Forbidden,
                        message: "absolute path outside workspace roots (federation sandbox)"
                            .into(),
                    };
                }
            }
        }
        // 接受：先回执，再 spawn 执行；结果经 send_result 通道回传组合根发送。
        let started = Instant::now();
        let from_owned = from.to_string();
        let tool = request.tool.clone();
        let args = request.args.clone();
        let result_call_id = call_id.clone();
        tokio::spawn(async move {
            let outcome = registry.execute(&tool, args).await;
            let result = match outcome {
                Ok(text) => InvokeResult {
                    call_id: result_call_id,
                    success: true,
                    output: text,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                },
                Err(e) => InvokeResult {
                    call_id: result_call_id,
                    success: false,
                    output: e.to_string(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                },
            };
            let _ = send_result
                .send((from_owned, FedFrame::InvokeResult(result)))
                .await;
        });
        FedFrame::InvokeAccepted {
            call_id,
            verdict: InvokeVerdict::Accepted,
        }
    }

    /// 大脑侧观测：某 peer 的出站调用是否仍在飞。
    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

/// 从对端 caps 推导可注册的代理工具名（交集：对端声明 ∩ 首批支持的
/// 无状态工具族——shell 三件套的远程化留待后续迭代，见 RFC §4 注记）。
pub fn remote_tool_candidates(caps: &NodeCaps) -> Vec<String> {
    const SUPPORTED: [&str; 6] = [
        "bash",
        "read_file",
        "write_file",
        "edit_file",
        "search_code",
        "list_files",
    ];
    SUPPORTED
        .iter()
        .filter(|t| caps.tools.iter().any(|c| c == *t))
        .map(|t| t.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_federation::NodeCaps;

    fn policy(tools: &[&str]) -> ExecutorPolicy {
        ExecutorPolicy {
            allow_tools: tools.iter().map(|s| s.to_string()).collect(),
            require_confirm: Vec::new(),
            allow_queries: Vec::new(),
        }
    }

    #[test]
    fn verdict_respects_whitelist_and_wildcard() {
        let p = policy(&["bash", "read_file"]);
        assert_eq!(p.verdict_for("bash"), InvokeVerdict::Accepted);
        assert_eq!(p.verdict_for("write_file"), InvokeVerdict::Rejected);
        let all = policy(&["*"]);
        assert_eq!(all.verdict_for("anything"), InvokeVerdict::Accepted);
    }

    #[test]
    fn require_confirm_rejects_even_if_allowed() {
        let p = ExecutorPolicy {
            allow_tools: vec!["*".into()],
            require_confirm: vec!["bash".into()],
            allow_queries: Vec::new(),
        };
        assert_eq!(p.verdict_for("bash"), InvokeVerdict::Rejected);
        assert_eq!(p.verdict_for("read_file"), InvokeVerdict::Accepted);
    }

    #[tokio::test]
    async fn outbound_result_routed_to_waiting_caller() {
        let router = InvokeRouter::new("node-a".into());
        let (req, pending) =
            router.invoke("node-b", "bash", serde_json::json!({"command": "ls"}), None);
        assert!(req.call_id.starts_with("node-a:"));
        let result = InvokeResult {
            call_id: req.call_id.clone(),
            success: true,
            output: "done".into(),
            elapsed_ms: 5,
        };
        router.dispatch_frame("node-b", &FedFrame::InvokeResult(result));
        let got = pending.wait().await.expect("result");
        assert!(got.success);
        assert_eq!(got.output, "done");
        assert_eq!(router.pending_count(), 0);
    }

    #[tokio::test]
    async fn rejected_verdict_fails_the_call() {
        let router = InvokeRouter::new("node-a".into());
        let (req, pending) = router.invoke("node-b", "bash", serde_json::json!({}), None);
        router.dispatch_frame(
            "node-b",
            &FedFrame::InvokeAccepted {
                call_id: req.call_id,
                verdict: InvokeVerdict::Rejected,
            },
        );
        let got = pending.wait().await.expect("terminal");
        assert!(!got.success);
        assert!(got.output.contains("拒绝"));
    }

    #[tokio::test]
    async fn output_chunks_prepend_final_output() {
        let router = InvokeRouter::new("node-a".into());
        let (req, pending) = router.invoke("node-b", "bash", serde_json::json!({}), None);
        router.dispatch_frame(
            "node-b",
            &FedFrame::InvokeOutput {
                call_id: req.call_id.clone(),
                chunk: "partial...".into(),
                stream: echo_federation::OutputStream::Stdout,
            },
        );
        router.dispatch_frame(
            "node-b",
            &FedFrame::InvokeResult(InvokeResult {
                call_id: req.call_id,
                success: true,
                output: "final".into(),
                elapsed_ms: 1,
            }),
        );
        let got = pending.wait().await.expect("terminal");
        assert_eq!(got.output, "partial...final");
    }

    #[tokio::test]
    async fn drop_peer_fails_inflight_calls() {
        let router = InvokeRouter::new("node-a".into());
        let (_req, pending) = router.invoke("node-b", "bash", serde_json::json!({}), None);
        router.drop_peer("node-b");
        let got = pending.wait().await.expect("terminal");
        assert!(!got.success);
        assert!(got.output.contains("断开"));
    }

    #[tokio::test]
    async fn loop_origin_call_is_rejected() {
        let router = InvokeRouter::new("node-a".into());
        let registry = Arc::new(ToolRegistry::new());
        let (tx, _rx) = mpsc::channel(1);
        let reply = router.handle_invoke(
            "node-b",
            InvokeRequest {
                call_id: "node-a:1-0".into(), // origin = 本机 → 回环
                tool: "bash".into(),
                args: serde_json::json!({}),
                workdir: None,
                timeout_secs: None,
            },
            registry,
            tx,
            &[],
        );
        assert!(matches!(
            reply,
            FedFrame::Error {
                code: echo_federation::FedError::LoopDetected,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn executor_rejects_tool_outside_whitelist() {
        let router = InvokeRouter::new("node-a".into());
        router.set_policy("node-b", policy(&["read_file"]));
        let registry = Arc::new(ToolRegistry::new());
        let (tx, _rx) = mpsc::channel(1);
        let reply = router.handle_invoke(
            "node-b",
            InvokeRequest {
                call_id: "node-c:1-0".into(),
                tool: "bash".into(),
                args: serde_json::json!({}),
                workdir: None,
                timeout_secs: None,
            },
            registry,
            tx,
            &[],
        );
        assert!(matches!(
            reply,
            FedFrame::InvokeAccepted {
                verdict: InvokeVerdict::Rejected,
                ..
            }
        ));
    }

    #[test]
    fn query_authorization_defaults() {
        use echo_federation::QueryKind;
        let default = ExecutorPolicy::default();
        assert!(default.query_allowed(QueryKind::NodeStatus));
        assert!(!default.query_allowed(QueryKind::SessionSnapshot));
        assert!(!default.query_allowed(QueryKind::WorkspaceFiles));
        let open = ExecutorPolicy {
            allow_tools: vec![],
            require_confirm: vec![],
            allow_queries: vec!["session_snapshot".into()],
        };
        assert!(open.query_allowed(QueryKind::SessionSnapshot));
        assert!(!open.query_allowed(QueryKind::WorkspaceFiles));
        let all = ExecutorPolicy {
            allow_tools: vec![],
            require_confirm: vec![],
            allow_queries: vec!["*".into()],
        };
        assert!(all.query_allowed(QueryKind::WorkspaceFiles));
    }

    #[tokio::test]
    async fn query_result_routed_to_waiting_caller() {
        let router = InvokeRouter::new("node-a".into());
        let (req, pending) = router.query(
            "node-b",
            echo_federation::QueryKind::NodeStatus,
            String::new(),
            0,
            0,
        );
        router.dispatch_frame(
            "node-b",
            &FedFrame::QueryResult(echo_federation::QueryResultFrame {
                call_id: req.call_id,
                success: true,
                payload: serde_json::json!({"node_id": "node-b"}),
            }),
        );
        let got = pending.wait().await.expect("terminal");
        assert!(got.success);
        assert!(got.output.contains("node-b"));
    }

    #[test]
    fn candidates_intersect_caps_with_supported() {
        let caps = NodeCaps {
            tools: vec!["bash".into(), "shell_start".into(), "read_file".into()],
            subagent: true,
            workspaces: vec![],
        };
        let names = remote_tool_candidates(&caps);
        // shell_start 不在首批支持集；交集保序
        assert_eq!(names, vec!["bash", "read_file"]);
    }
}
