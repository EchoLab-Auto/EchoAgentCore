//! 调用调度器（federation Phase 2）：call_id 路由表 + 执行端裁决。
//!
//! 三张表：
//!
//! - **出站** `DashMap<call_id, PendingCall>`：`RemoteTool` 经
//!   [`InvokeRouter::invoke`] 注册等待句柄，路由泵（组合根消费
//!   `LinkEvent::Frame` 并调用 [`InvokeRouter::dispatch_frame`]）把
//!   `InvokeAccepted/InvokeOutput/InvokeResult` 派发给等待者；
//! - **入站** `DashMap<call_id, Arc<InboundCall>>`：[`InvokeRouter::handle_invoke`]
//!   收对端 `Invoke` → 统一入口校验 → per-peer 白名单裁决
//!   （[`ExecutorPolicy`]）→ 登记入站条目（来源主体 / 截止时间 / 取消信号）
//!   → 本机工具执行 → 回传 `InvokeAccepted + InvokeResult`；
//! - per-peer 策略表 + 入站并发额度。
//!
//! ## 安全边界（2026-11 收紧）
//!
//! 1. **主体来自传输层**：执行时构造 [`ToolContext::for_remote_peer`]，把
//!    对端 `node_id` 作为可信来源注入工具——工具（shell 会话族等）据此绑定
//!    资源归属。参数里的任何 `__team_id` 一类魔术键都**不是**身份。
//! 2. **统一入口校验**：call_id 形状 + origin 必须等于来源 peer（防冒用
//!    他人 call_id 触发取消/串味）、工具名形状、args 必须是对象、工具参数
//!    schema 预检（必需字段）。
//! 3. **每个入站调用一条登记**：进入时登记，终结时**先摘除再回帧**——
//!    保证一次调用只有一个终态、表里不留已终结条目。
//! 4. **截止时间 / 取消**：到点或来源方 `Cancel` 时停止等待并发终态帧。
//!    对 shell 会话族这是"超时/取消只停止等待、不杀会话"的契约（drop 的是
//!    读到哨兵为止的 future，bash 子进程与会话都保留）；一次性 `bash` 工具
//!    自身 `kill_on_drop`，drop 即整组终止（与本地工具守卫同语义）。
//! 5. **取消只能来自调用发起方**：`Cancel` 的来源 peer 必须等于登记条目
//!    的 peer，否则只记审计、不生效。
//! 6. **并发上限**：全局 + per-peer 两道信号量，超限立刻回错误帧（不排队
//!    到调用方超时）。
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures_util::FutureExt;
use tokio::sync::{mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};

use crate::{new_call_id, FedFrame, InvokeRequest, InvokeResult, InvokeVerdict, NodeCaps};

use echo_defs::registry::ToolRegistry;

/// 入站并发上限（全局 / 单 peer）。
///
/// 取值边界：单个 `bash` 最长 300s、shell 会话族可长期占着（每次 exec 一条
/// 命令），8 条并发已够「起任务 + poll」的典型用法；全局 32 覆盖多 peer。
/// 超限**立刻**回错误帧，让调用方立即看到"对端忙"，而不是干等 460s。
pub const MAX_INBOUND_INFLIGHT: usize = 32;
pub const MAX_INBOUND_PER_PEER: usize = 8;

/// 入站缺省截止（秒）：调用方没给 `timeout_secs`、工具也没给
/// [`echo_defs::tool::Tool::timeout_hint`] 时用它。
const DEFAULT_INBOUND_DEADLINE_SECS: u64 = 120;
/// 入站截止上限（秒）：与 `bash` / `bash-exec` 的 300s 上限留出余量。
const MAX_INBOUND_DEADLINE_SECS: u64 = 600;
/// 让调用方自己的超时先到一步：执行端截止 = 请求值 + 这点余量，
/// 使"调用方等的超时"与"执行端判的超时"不会互相打架（调用方先到 →
/// 它会发 `Cancel`，执行端按取消处理）。
const INBOUND_DEADLINE_GRACE_SECS: u64 = 5;

/// 入站调用登记：来源主体 / 截止时间 / 取消信号。
struct InboundCall {
    /// 来源 peer 的 `node_id`（授权主体，来自链路而非参数）。
    peer: String,
    tool: String,
    started: Instant,
    deadline: Instant,
    /// `true` = 来源方取消（只有登记条目上的 peer 能置位）。
    cancel: watch::Sender<bool>,
}

impl InboundCall {
    fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }
}

/// 执行任务的 RAII 结算器（2026-11 复审 D）。
///
/// 正常路径在回帧前显式 [`InboundSettlement::settle`]；任务 panic / abort
/// 时由 `Drop` 兜底：**先**从入站表摘除自己（`Arc::ptr_eq` 防误删重放条目），
/// **再**尽力回一个失败终态。没有它，一次工具 panic 就会把 `inbound` 条目
/// 永久留在表里（并发额度也不释放），调用方只能等到自身超时。
struct InboundSettlement {
    router: Arc<InvokeRouter>,
    call_id: String,
    entry: Arc<InboundCall>,
    peer: String,
    started: Instant,
    sender: mpsc::Sender<(String, FedFrame)>,
    settled: bool,
}

impl InboundSettlement {
    fn new(
        router: Arc<InvokeRouter>,
        call_id: String,
        entry: Arc<InboundCall>,
        peer: String,
        started: Instant,
        sender: mpsc::Sender<(String, FedFrame)>,
    ) -> Self {
        Self {
            router,
            call_id,
            entry,
            peer,
            started,
            sender,
            settled: false,
        }
    }

    /// 正常终态已摘表 / 已生成：Drop 不再补发 panic 终态。
    fn settle(mut self) {
        self.settled = true;
    }
}

impl Drop for InboundSettlement {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.router
            .inbound
            .remove_if(&self.call_id, |_, entry| Arc::ptr_eq(entry, &self.entry));
        tracing::error!(
            target: "federation",
            call_id = %self.call_id,
            peer = %self.peer,
            "inbound execution task panicked/aborted — cleaned up and replying failure terminal"
        );
        let result = InvokeResult {
            call_id: self.call_id.clone(),
            success: false,
            output: "执行端任务异常终止（panic）；请重试".into(),
            elapsed_ms: self.started.elapsed().as_millis() as u64,
        };
        let frame = (self.peer.clone(), FedFrame::InvokeResult(result));
        let sender = self.sender.clone();
        match sender.try_send(frame) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(frame)) => {
                // 通道满：panic 上下文不能 await，交给运行时补齐（尽力而为）。
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let _ = sender.send(frame).await;
                    });
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

/// 把入站 `watch` 取消信号适配成工具上下文里的
/// [`echo_defs::tool::Cancellation`]——尊重 `cx.cancel` 的工具（子代理等）
/// 与路由层的截止/取消走同一个信号。
struct InboundCancellation {
    rx: watch::Receiver<bool>,
}

impl echo_defs::tool::Cancellation for InboundCancellation {
    fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    fn when_cancelled(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let mut rx = self.rx.clone();
        Box::pin(async move {
            let _ = rx.wait_for(|cancelled| *cancelled).await;
        })
    }
}
/// 入站执行的终态（统一映射成 `InvokeResult`）。
enum Terminal {
    Ok(String),
    Err(String),
    TimedOut(u64),
    Cancelled,
    /// 工具执行 panic（被 `catch_unwind` 捕获）：回显消息，别让调用方
    /// 干等到自身超时。
    Panicked(String),
}

/// 从 panic payload 里取可读消息（`panic!("...")` 的两种常见形状）。
fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "未知 panic".to_string()
    }
}

/// 等待中的出站调用：中间输出聚合 + 终态通知。
struct PendingCall {
    peer: String,
    started: Instant,
    /// 存活上限：`sweep` 据此判定条目是否已泄漏（见 [`InvokeRouter::sweep`]）。
    ttl: Duration,
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
    /// 显式列出——会话内容与文件系统敏感。**`*` = 完全开放**：
    /// 除放行全部查询种类外，执行端同时豁免**文件系统沙箱**（文件
    /// 工具调用的工作区根裁决 + WorkspaceFiles/GitStatus 的目录
    /// canonical 校验）——与「SSH 免密」同语义（2026-10 设计演进）。
    pub allow_queries: Vec<String>,
    /// 允许对端远程委派子代理（Phase 3 spawn_subagent node=<peer>）。
    pub allow_subagent: bool,
}

impl ExecutorPolicy {
    /// 满权限策略（`*` 全部工具/查询 + 接受委派）——邀请配对提升时的
    /// **默认值**（2026-10）：邀请串本身即一次性凭证，接收方按互信对待
    /// （与「SSH 免密」同语义）；提升后落配置、可在 Panel 按需收紧。
    pub fn full() -> Self {
        Self {
            allow_tools: vec!["*".into()],
            require_confirm: Vec::new(),
            allow_queries: vec!["*".into()],
            allow_subagent: true,
        }
    }

    /// 查询授权：`node_status` 恒允许；其余需显式列出（`*` 全放行）。
    pub fn query_allowed(&self, kind: crate::QueryKind) -> bool {
        use crate::QueryKind;
        // NodeStatus 与 ApiProfiles 恒允许：前者无害遥测，后者只回脱敏
        // 供应商元数据（key 明文永不过线）——分布式供应商发现需要
        // 开箱可用（2026-10 分布式供应商共享设计）。
        if matches!(kind, QueryKind::NodeStatus | QueryKind::ApiProfiles) {
            return true;
        }
        let key = match kind {
            QueryKind::SessionSnapshot => "session_snapshot",
            QueryKind::WorkspaceFiles => "workspace_files",
            QueryKind::WorkspaceGitStatus => "workspace_git_status",
            QueryKind::BrowseDirectories => "browse_directories",
            QueryKind::NodeStatus | QueryKind::ApiProfiles => unreachable!(),
        };
        self.allow_queries.iter().any(|q| q == "*" || q == key)
    }

    /// 是否完全开放（`*` 全权）：执行端据此豁免文件系统沙箱（Invoke
    /// 文件工具的工作区根裁决 + WorkspaceFiles/GitStatus 的目录
    /// canonical 校验）——`allow_queries = ["*"]` 即「SSH 免密」语义，
    /// 对端可访问本机任意可读路径（2026-10 设计演进，用户要求）。
    pub fn is_full_access(&self) -> bool {
        self.allow_queries.iter().any(|q| q == "*")
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

/// pending 条目的最小存活时间：短于此绝不清扫；带显式超时的调用按
/// pending 条目的最小存活时间：短于此绝不清扫；带显式超时的调用按
/// `timeout + 网络余量` 延长，避免清扫误杀仍在飞行的长调用（远程 bash
/// 可跑 300s+）。
const PENDING_MIN_TTL: Duration = Duration::from_secs(120);

/// 由调用方声明的超时推导 pending 存活上限。
fn pending_ttl(timeout: Option<Duration>) -> Duration {
    PENDING_MIN_TTL.max(
        timeout
            .map(|t| t + Duration::from_secs(60))
            .unwrap_or(PENDING_MIN_TTL),
    )
}

/// 联邦调用调度器（组合根持有一份）。
pub struct InvokeRouter {
    /// 出站等待表：call_id → pending。
    pending: DashMap<String, PendingCall>,
    /// 入站登记表：call_id → 登记条目（来源主体 / 截止 / 取消）。
    inbound: DashMap<String, Arc<InboundCall>>,
    /// 执行端策略表：peer node_id → 白名单。
    policies: DashMap<String, ExecutorPolicy>,
    /// 入站并发额度（全局）。
    inbound_permits: Arc<Semaphore>,
    /// 入站并发额度（per-peer，惰性建桶）。
    peer_permits: DashMap<String, Arc<Semaphore>>,
    /// 入站截止的余量（生产 5s；测试可调小，避免为了等超时跑几秒）。
    deadline_grace: Duration,
    /// 本机 node_id（回环帧拒绝）。
    local_node: String,
}

impl InvokeRouter {
    pub fn new(local_node: String) -> Arc<Self> {
        Self::new_with_grace(local_node, Duration::from_secs(INBOUND_DEADLINE_GRACE_SECS))
    }

    /// 构造入口（`deadline_grace` 可调；生产走 [`Self::new`]）。
    fn new_with_grace(local_node: String, deadline_grace: Duration) -> Arc<Self> {
        Arc::new(Self {
            pending: DashMap::new(),
            inbound: DashMap::new(),
            policies: DashMap::new(),
            inbound_permits: Arc::new(Semaphore::new(MAX_INBOUND_INFLIGHT)),
            peer_permits: DashMap::new(),
            deadline_grace,
            local_node,
        })
    }

    /// 该 peer 的入站额度（首次使用时建桶）。
    fn peer_permits(&self, peer: &str) -> Arc<Semaphore> {
        self.peer_permits
            .entry(peer.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(MAX_INBOUND_PER_PEER)))
            .clone()
    }

    /// 注册/更新某 peer 的执行端策略（握手 Up 时按 peer 配置写入；
    /// 无配置 peer = 全拒）。
    pub fn set_policy(&self, peer_node: &str, policy: ExecutorPolicy) {
        self.policies.insert(peer_node.to_string(), policy);
    }

    pub fn drop_peer(&self, peer_node: &str) {
        self.policies.remove(peer_node);
        self.peer_permits.remove(peer_node);
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
        // 入站方向：调用方已经掉线，继续跑只是浪费执行端资源——
        // 置取消位让执行任务停止等待（会话类资源由工具自身策略保留）。
        let mut cancelled = 0usize;
        for entry in self.inbound.iter() {
            if entry.peer == peer_node && !entry.is_cancelled() {
                let _ = entry.cancel.send(true);
                cancelled += 1;
            }
        }
        if cancelled > 0 {
            tracing::info!(
                target: "federation",
                peer = %peer_node,
                calls = cancelled,
                "cancelled inbound calls after peer disconnect"
            );
        }
    }
    /// 清扫已泄漏的 pending 条目（send_to 失败未清理、等待方已放弃、
    /// 对端永不回复等）：条目超过其 ttl，或等待端 receiver 已 drop
    /// （`done.is_closed()`）。DashMap 逐桶扫描，无需额外全局锁；
    /// 在 `invoke`/`query` 入口顺手调用，O(在飞调用数)。
    pub fn sweep(&self) {
        let now = Instant::now();
        let stale: Vec<String> = self
            .pending
            .iter()
            .filter(|e| now.duration_since(e.started) > e.ttl || e.done.is_closed())
            .map(|e| e.key().clone())
            .collect();
        for call_id in stale {
            if let Some((_, pending)) = self.pending.remove(&call_id) {
                // 清理原因分级（2026-10 日志修复）：等待方已放弃（receiver
                // 被 drop——如 `fetch_live_load` 的 3s 短超时查询，属**常规**
                // 行为）记 debug；ttl 超时（对端长期无响应 / 发送后未清理，
                // 属异常）保持 warn。此前两者共用 warn——每次对端响应慢于
                // 短超时都会留一条「stale pending」告警，把常规行为渲染成
                // 故障（重启后首屏即两连 WARN 的实况）。
                if pending.done.is_closed() {
                    tracing::debug!(
                        target: "federation",
                        call_id = %call_id,
                        peer = %pending.peer,
                        age_ms = pending.started.elapsed().as_millis() as u64,
                        "sweeping abandoned pending invoke (caller gone)"
                    );
                } else {
                    tracing::warn!(
                        target: "federation",
                        call_id = %call_id,
                        peer = %pending.peer,
                        age_ms = pending.started.elapsed().as_millis() as u64,
                        "sweeping stale pending invoke (ttl exceeded)"
                    );
                }
                // drop done/chunks：若等待方仍在，wait() 以「链路断开」失败。
                drop(pending);
            }
        }
        // 入站泄漏兜底：正常路径在回帧前摘除条目；这里只清"执行任务已死但
        // 条目还在"的异常形状（超过截止 + 60s 仍挂着）。先置取消位，
        // 让残余任务下一次被唤醒时自行退出并摘除。
        let inbound_stale: Vec<Arc<InboundCall>> = self
            .inbound
            .iter()
            .filter(|e| {
                now.checked_duration_since(e.deadline)
                    .is_some_and(|late| late > Duration::from_secs(60))
            })
            .map(|e| e.value().clone())
            .collect();
        for entry in inbound_stale {
            tracing::warn!(
                target: "federation",
                peer = %entry.peer,
                tool = %entry.tool,
                age_ms = entry.started.elapsed().as_millis() as u64,
                "sweeping stale inbound invoke (deadline + 60s)"
            );
            let _ = entry.cancel.send(true);
        }
    }

    /// 取出**属于 `from`** 的出站等待条目；来源不是登记 peer 时不动它。
    ///
    /// 2026-11 复审 E：call_id 只是路由键，不能当授权凭据——任一链路对端
    /// 都能在帧里填别人的 call_id。若不做来源校验，peer B 一条伪造的
    /// `InvokeResult` / `Error` / `QueryResult` 就能结算 peer A 的条目，
    /// 等待方拿到的是 B 编的输出，A 的真结果随后被当作迟到帧丢弃。
    fn take_pending_from(&self, from: &str, call_id: &str) -> Option<(String, PendingCall)> {
        use dashmap::mapref::entry::Entry;
        match self.pending.entry(call_id.to_string()) {
            Entry::Occupied(entry) if entry.get().peer == from => Some(entry.remove_entry()),
            Entry::Occupied(entry) => {
                tracing::warn!(
                    target: "federation",
                    from = %from,
                    call_id = %call_id,
                    owner = %entry.get().peer,
                    "ignoring terminal frame for a pending call owned by another peer"
                );
                None
            }
            Entry::Vacant(_) => None,
        }
    }

    /// 出站失败快速清理（如 `send_to` 失败）：显式以失败终态唤醒等待方
    /// 并移除条目，避免等待方干等超时、条目滞留到下一次 sweep。
    /// **幂等**：条目已不在（正常收尾 / 已结算）时是 no-op。
    pub fn fail_pending(&self, call_id: &str, reason: &str) {
        if let Some((_, pending)) = self.pending.remove(call_id) {
            let _ = pending.done.send(InvokeResult {
                call_id: call_id.to_string(),
                success: false,
                output: reason.to_string(),
                elapsed_ms: pending.started.elapsed().as_millis() as u64,
            });
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
        self.sweep();
        let call_id = new_call_id(&self.local_node);
        let (chunk_tx, chunk_rx) = mpsc::channel(64);
        let (done_tx, done_rx) = oneshot::channel();
        self.pending.insert(
            call_id.clone(),
            PendingCall {
                peer: peer_node.to_string(),
                started: Instant::now(),
                ttl: pending_ttl(timeout),
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
        kind: crate::QueryKind,
        subject: String,
        team_id: String,
        since_seq: u64,
        limit: u32,
    ) -> (crate::QueryRequest, PendingInvoke) {
        self.sweep();
        let call_id = new_call_id(&self.local_node);
        let (chunk_tx, chunk_rx) = mpsc::channel(64);
        let (done_tx, done_rx) = oneshot::channel();
        self.pending.insert(
            call_id.clone(),
            PendingCall {
                peer: peer_node.to_string(),
                started: Instant::now(),
                ttl: pending_ttl(None),
                chunks: chunk_tx,
                done: done_tx,
            },
        );
        (
            crate::QueryRequest {
                call_id: call_id.clone(),
                kind,
                subject,
                team_id,
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
                // 非 Accepted 的受理回执都是**终态**：v1 没有后续确
                // 认流程，也没有"再次裁决"的帧——若在此继续等，等待方
                // 会一直挂到自身硬超时（460s），界面上只看到"远程工具
                // 超时"，真正的裁决原因（白名单/门控）完全不可见。
                // 因此 Rejected / NeedsConfirm 一律立即终结 pending。
                match verdict {
                    InvokeVerdict::Accepted => {}
                    InvokeVerdict::Rejected | InvokeVerdict::NeedsConfirm => {
                        let reason = if *verdict == InvokeVerdict::Rejected {
                            "对端拒绝执行（白名单/门控）"
                        } else {
                            "对端要求人工确认，v1 无确认通道（按拒绝处理）"
                        };
                        if let Some((_, pending)) = self.take_pending_from(from, call_id) {
                            let _ = pending.done.send(InvokeResult {
                                call_id: call_id.clone(),
                                success: false,
                                output: reason.into(),
                                elapsed_ms: pending.started.elapsed().as_millis() as u64,
                            });
                        }
                    }
                }
            }
            // 通用错误帧（2026-10 修复）：此前落到 `_ => {}` 被静默丢弃，
            // 执行端明确的"未加载任何 persona""路径越界""回环"等回执全
            // 都变成调用方的 460s 空等（实测 node-01m436f8 空 agent 时
            // list_files/bash 均挂满超时）。带 call_id 即终结对应 pending。
            FedFrame::Error {
                call_id: Some(call_id),
                code,
                message,
            } => {
                if let Some((_, pending)) = self.take_pending_from(from, call_id) {
                    tracing::warn!(
                        target: "federation",
                        from = %from, call_id = %call_id, code = ?code,
                        "remote invoke failed (error frame)"
                    );
                    let _ = pending.done.send(InvokeResult {
                        call_id: call_id.clone(),
                        success: false,
                        output: format!("对端返回错误（{code:?}）：{message}"),
                        elapsed_ms: pending.started.elapsed().as_millis() as u64,
                    });
                }
            }
            FedFrame::Error {
                call_id: None,
                code,
                message,
            } => {
                tracing::warn!(
                    target: "federation", from = %from, code = ?code, message = %message,
                    "federation error frame without call_id"
                );
            }
            FedFrame::InvokeOutput {
                call_id,
                chunk,
                stream: _,
            } => match self.pending.get(call_id) {
                Some(pending) if pending.peer == from => {
                    let _ = pending.chunks.try_send(chunk.clone());
                }
                Some(pending) => tracing::warn!(
                    target: "federation",
                    from = %from, call_id = %call_id, owner = %pending.peer,
                    "ignoring output chunk for a pending call owned by another peer"
                ),
                None => {}
            },
            FedFrame::InvokeResult(result) => {
                if let Some((_, pending)) = self.take_pending_from(from, &result.call_id) {
                    let _ = pending.done.send(result.clone());
                }
            }
            FedFrame::QueryResult(result) => {
                if let Some((_, pending)) = self.take_pending_from(from, &result.call_id) {
                    let _ = pending.done.send(InvokeResult {
                        call_id: result.call_id.clone(),
                        success: result.success,
                        output: result.payload.to_string(),
                        elapsed_ms: 0,
                    });
                }
            }
            FedFrame::Cancel { call_id } => {
                // 取消传播（2026-11 落实）：只允许**调用发起方**取消自己的
                // call——登记条目上的 peer 是唯一权威。来源不符只记审计，
                // 不触碰别人的执行（否则任一 peer 可凭猜到的 call_id 打断
                // 全节点的在飞调用）。
                match self.inbound.get(call_id) {
                    Some(entry) if entry.peer == from => {
                        let _ = entry.cancel.send(true);
                        tracing::info!(
                            target: "federation",
                            from = %from,
                            %call_id,
                            tool = %entry.tool,
                            "inbound invoke cancelled by origin"
                        );
                    }
                    Some(entry) => {
                        tracing::warn!(
                            target: "federation",
                            from = %from,
                            call_id = %call_id,
                            owner = %entry.peer,
                            "cancel ignored: caller is not the origin of this call"
                        );
                    }
                    None => {
                        // 终态已回 / 从未见过：迟到帧，属正常竞态。
                        tracing::debug!(
                            target: "federation",
                            from = %from,
                            %call_id,
                            "cancel for unknown or already-finished call"
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// 执行端入口：收 `Invoke` → 统一校验 → 裁决 → 登记 → spawn 本机执行。
    ///
    /// 返回立即要回传的帧（`InvokeAccepted` / `Error` / 预检失败的
    /// `InvokeResult`）；接受时执行任务完成后经 `send_result` 通道回传组合根
    /// 发送（终态只有一帧——任务在发送**之前**已把入站条目摘除）。
    pub fn handle_invoke(
        self: &Arc<Self>,
        from: &str,
        request: InvokeRequest,
        registry: Arc<ToolRegistry>,
        send_result: mpsc::Sender<(String, FedFrame)>,
        workspace_roots: &[std::path::PathBuf],
    ) -> FedFrame {
        let call_id = request.call_id.clone();
        if let Err(reply) = self.validate_invoke_envelope(from, &request) {
            return *reply;
        }
        // 2. per-peer 白名单裁决。
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
        // 3. 能力裁决（2026-11 S8）：本机**没声明可远程调用**的工具一律不执行。
        //
        // 这条闸门此前不存在：peer 策略为 `allow_tools = ["*"]`（配对互信
        // 的默认档）时，任何工具名都能穿透到执行端——包括 `workspace` /
        // `instance_stop` / `spawn_subagent` 这类会改宿主状态、本来就只该
        // 本地调的管理工具。准入集合现在由工具自己的声明给出。
        let Some(tool) = registry.get(&request.tool) else {
            return FedFrame::Error {
                call_id: Some(call_id),
                code: crate::FedError::UnknownCall,
                message: format!("executor has no tool named `{}`", request.tool),
            };
        };
        if !tool.remote_capable() {
            tracing::warn!(
                target: "federation",
                from = %from, call_id = %call_id, tool = %request.tool,
                "invoke rejected: tool did not declare remote_capable"
            );
            return FedFrame::Error {
                call_id: Some(call_id),
                code: crate::FedError::Forbidden,
                message: format!("tool `{}` is not remote-capable on this node", request.tool),
            };
        }
        // 4. 沙箱裁决（2026-10 安全收紧；S8 改为**问工具**）：
        // 带路径参数的工具，绝对路径必须落在执行端工作区根并集内——否则获授
        // read_file 的 peer 可读 ~/.ssh 等任意路径，allow_tools 白名单形同
        // 虚设。参数名由 [`echo_defs::tool::Tool::path_arguments`] 声明，
        // 所以模块新加的文件工具自动享有同一条保护（此前是一张写死的
        // 5 个名字的表，新工具静默裸奔）。
        //
        // **豁免（2026-10 设计演进）**：peer 策略为完全开放
        // （allow_queries = ["*"]，邀请配对提升的默认互信档）时跳过沙箱——
        // 「SSH 免密」同语义：完全互信的对端不受目录边界约束。
        let full_access = self
            .policies
            .get(from)
            .map(|p| p.is_full_access())
            .unwrap_or(false);
        if !full_access {
            for arg in tool.path_arguments() {
                let Some(raw) = request.args.get(*arg).and_then(|v| v.as_str()) else {
                    continue;
                };
                if raw.starts_with('/') && !crate::path_within_roots(raw, workspace_roots) {
                    tracing::warn!(
                        target: "federation",
                        from = %from, call_id = %call_id, tool = %request.tool,
                        argument = %arg, path = %raw,
                        "federation absolute path outside workspace roots — rejected"
                    );
                    return FedFrame::Error {
                        call_id: Some(call_id),
                        code: crate::FedError::Forbidden,
                        message: format!(
                            "absolute path in `{arg}` outside workspace roots (federation sandbox)"
                        ),
                    };
                }
            }
        }
        // 5. 参数 schema 预检（统一入口，不再把空参/缺参直接甩给工具实现）：
        //    必需字段缺失时立即回终态明文——与本地执行链
        //    （[`echo_defs::execute::invalid_tool_arguments`]）同一口径。
        let schema = tool.parameters();
        if let Some(message) = echo_defs::execute::invalid_tool_arguments(
            &request.tool,
            &serde_json::to_string(&request.args).unwrap_or_default(),
            &request.args,
            &schema,
        ) {
            tracing::warn!(
                target: "federation",
                from = %from, call_id = %call_id, tool = %request.tool,
                "invoke rejected: argument preflight failed"
            );
            return FedFrame::InvokeResult(InvokeResult {
                call_id,
                success: false,
                output: message,
                elapsed_ms: 0,
            });
        }
        // 6. 并发额度（全局 + per-peer）：超限立即回错误帧。
        let Ok(global_permit) = self.inbound_permits.clone().try_acquire_owned() else {
            tracing::warn!(target: "federation", from = %from, "inbound concurrency limit reached (global)");
            return FedFrame::Error {
                call_id: Some(call_id),
                code: crate::FedError::Internal,
                message: format!(
                    "executor busy: {MAX_INBOUND_INFLIGHT} inbound calls in flight, retry later"
                ),
            };
        };
        let Ok(peer_permit) = self.peer_permits(from).try_acquire_owned() else {
            tracing::warn!(target: "federation", from = %from, "inbound concurrency limit reached (peer)");
            return FedFrame::Error {
                call_id: Some(call_id),
                code: crate::FedError::Internal,
                message: format!(
                    "executor busy for this peer: {MAX_INBOUND_PER_PEER} calls in flight, retry later"
                ),
            };
        };
        // 7. 截止时间：请求值 > 工具自报 hint > 缺省，统一夹到上限并加一点
        //    余量（让调用方自己的超时先到一步、由它发 Cancel 收尾）。
        let requested_secs = request
            .timeout_secs
            .or_else(|| tool.timeout_hint(&request.args).map(|d| d.as_secs()))
            .unwrap_or(DEFAULT_INBOUND_DEADLINE_SECS)
            .clamp(1, MAX_INBOUND_DEADLINE_SECS);
        let started = Instant::now();
        let deadline = started + Duration::from_secs(requested_secs) + self.deadline_grace;
        // 8. 入站登记（重复 call_id → 拒绝；一条 call 只能有一个执行体）。
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let inbound = Arc::new(InboundCall {
            peer: from.to_string(),
            tool: request.tool.clone(),
            started,
            deadline,
            cancel: cancel_tx,
        });
        match self.inbound.entry(call_id.clone()) {
            dashmap::mapref::entry::Entry::Occupied(_) => {
                tracing::warn!(
                    target: "federation",
                    from = %from, call_id = %call_id,
                    "duplicate inbound call_id rejected"
                );
                return FedFrame::Error {
                    call_id: Some(call_id),
                    code: crate::FedError::Forbidden,
                    message: "duplicate call_id for an in-flight invoke".into(),
                };
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(Arc::clone(&inbound));
            }
        }
        // 9. spawn 执行：截止 / 取消 / 工具完成三选一，先摘表再回帧。
        let router = Arc::clone(self);
        let from_owned = from.to_string();
        let tool_name = request.tool.clone();
        let args = request.args.clone();
        let result_call_id = call_id.clone();
        // 兜底结算器（2026-11 复审 D）：在 spawn **之前**构造并 move 进任务。
        // 即使任务在第一次 poll 前就被 abort（future 直接 drop，任务体一行
        // 都没跑），Drop 也会摘掉入站条目并回失败终态；正常路径回帧前 settle。
        let settlement = InboundSettlement::new(
            Arc::clone(&router),
            result_call_id.clone(),
            Arc::clone(&inbound),
            from_owned.clone(),
            started,
            send_result.clone(),
        );
        tokio::spawn(async move {
            let _global_permit: OwnedSemaphorePermit = global_permit;
            let _peer_permit: OwnedSemaphorePermit = peer_permit;
            let mut cancel_rx = cancel_rx;
            let cx = echo_defs::tool::ToolContext {
                peer: Some(from_owned.clone()),
                cancel: Some(Arc::new(InboundCancellation {
                    rx: cancel_rx.clone(),
                })),
                ..Default::default()
            };
            // 工具 panic 转成显式终态（可捕获时给出 panic 消息）；超出
            // catch_unwind 的异常终止仍由上面的 RAII 结算器兜底。
            let exec =
                std::panic::AssertUnwindSafe(registry.execute_rich_in(&cx, &tool_name, args))
                    .catch_unwind();
            let terminal = tokio::select! {
                biased;
                // 来源方取消：停止等待（shell 会话保留，一次性 bash 由工具
                // 自身 kill_on_drop 收尾）。
                _ = cancel_rx.wait_for(|cancelled| *cancelled) => Terminal::Cancelled,
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    Terminal::TimedOut(requested_secs)
                }
                outcome = exec => match outcome {
                    Ok(Ok(result)) => Terminal::Ok(result.text),
                    Ok(Err(error)) => Terminal::Err(error.to_string()),
                    Err(payload) => Terminal::Panicked(panic_payload_message(payload)),
                },
            };
            let elapsed_ms = started.elapsed().as_millis() as u64;
            // 单终态：先从入站表摘除，再回帧——此后迟到的 Cancel/重放都
            // 找不到条目，不会产生第二个终态。
            router
                .inbound
                .remove_if(&result_call_id, |_, entry| Arc::ptr_eq(entry, &inbound));
            let result = match terminal {
                Terminal::Ok(text) => InvokeResult {
                    call_id: result_call_id,
                    success: true,
                    output: text,
                    elapsed_ms,
                },
                Terminal::Err(error) => InvokeResult {
                    call_id: result_call_id,
                    success: false,
                    output: error,
                    elapsed_ms,
                },
                Terminal::TimedOut(secs) => InvokeResult {
                    call_id: result_call_id,
                    success: false,
                    output: format!(
                        "执行端超时（{secs}s）：已停止等待（工具内部分配的资源按其自身语义保留）"
                    ),
                    elapsed_ms,
                },
                Terminal::Cancelled => InvokeResult {
                    call_id: result_call_id,
                    success: false,
                    output: "调用已被发起方取消".into(),
                    elapsed_ms,
                },
                Terminal::Panicked(message) => InvokeResult {
                    call_id: result_call_id,
                    success: false,
                    output: format!("执行端工具执行 panic：{message}"),
                    elapsed_ms,
                },
            };
            // 正常终态已生成：解除兜底，避免 Drop 再补一帧。
            settlement.settle();
            let _ = send_result
                .send((from_owned, FedFrame::InvokeResult(result)))
                .await;
        });
        FedFrame::InvokeAccepted {
            call_id,
            verdict: InvokeVerdict::Accepted,
        }
    }

    /// 统一入口基本校验（任何执行动作之前）。
    ///
    /// - `call_id` 形状：`<origin>:<...>`，且 origin 必须等于来源 peer——链路
    ///   是唯一可信的身份来源；不校验就能用别人的 call_id 让取消/结果串味；
    /// - 回环：origin 是本机 → `LoopDetected`（跨机回环，恶意或误配）；
    /// - 工具名：非空、长度有界、无空白/控制字符（注册表键的形状假设）；
    /// - `args`：必须是 JSON 对象或 null（工具 schema 的根形状）。
    ///
    /// `Err` 侧装箱：`FedFrame` 是线上帧的固定形状（含 `NodeHello` 等大
    /// 变体），不能为 lint 改协议类型；校验失败路径本就冷，Box 一次即可
    /// 让 `Result` 回到小尺寸。
    fn validate_invoke_envelope(
        &self,
        from: &str,
        request: &InvokeRequest,
    ) -> Result<(), Box<FedFrame>> {
        let call_id = request.call_id.as_str();
        let reject = |code, message: String| {
            Err(Box::new(FedFrame::Error {
                call_id: Some(request.call_id.clone()),
                code,
                message,
            }))
        };
        if call_id.is_empty() || call_id.len() > 256 {
            return reject(
                crate::FedError::Forbidden,
                "malformed call_id (length)".into(),
            );
        }
        let Some((origin, suffix)) = call_id.split_once(':') else {
            return reject(
                crate::FedError::Forbidden,
                "malformed call_id (no origin)".into(),
            );
        };
        if suffix.is_empty()
            || !suffix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return reject(
                crate::FedError::Forbidden,
                "malformed call_id (suffix)".into(),
            );
        }
        if origin == self.local_node {
            return reject(
                crate::FedError::LoopDetected,
                "call_id origin is local node".into(),
            );
        }
        if origin != from {
            return reject(
                crate::FedError::Forbidden,
                format!("call_id origin `{origin}` does not match link peer"),
            );
        }
        let tool = request.tool.as_str();
        if tool.is_empty() || tool.len() > 128 {
            return reject(
                crate::FedError::Forbidden,
                "malformed tool name (length)".into(),
            );
        }
        if tool.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return reject(
                crate::FedError::Forbidden,
                "malformed tool name (chars)".into(),
            );
        }
        if !(request.args.is_object() || request.args.is_null()) {
            return reject(
                crate::FedError::Forbidden,
                "invoke args must be a JSON object".into(),
            );
        }
        Ok(())
    }

    /// 大脑侧观测：某 peer 的出站调用是否仍在飞。
    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// 执行端观测：仍在飞（含取消中）的入站调用数。正常终结后必须回零。
    #[cfg(test)]
    fn inbound_count(&self) -> usize {
        self.inbound.len()
    }
}

/// 从对端 caps 推导可注册的代理工具名（2026-11 S8）。
///
/// 此前这里还有一份 `const SUPPORTED: [&str; 6]`，与本机握手宣告的清单
/// 求交集——同一批工具名写在**两个 crate** 里，任一处改名就静默少一个远程
/// 工具（对端照旧宣告、本机照旧忽略，没有任何报错）。现在唯一真源是工具
/// 自己的 [`echo_defs::tool::Tool::remote_capable`] 声明：对端宣告什么就
/// 镜像什么，本函数只负责去重与保序。
///
/// 安全性不靠这张表：执行端的准入由 peer 策略（`allow_tools`）+ 工具声明
/// 双重裁决（见 [`InvokeRouter::handle_invoke`]）。
pub fn remote_tool_candidates(caps: &NodeCaps) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    caps.tools
        .iter()
        .filter(|tool| !tool.is_empty() && seen.insert(tool.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeCaps;

    /// 最小工具：**按声明**控制远程面与路径参数。
    ///
    /// 执行端测试需要"注册表里真的有个工具"——S8 起 `handle_invoke` 先查
    /// 注册表再查声明，空注册表会得到 `UnknownCall`（此前按名字查表，
    /// 没注册也照样过沙箱裁决，那个"过"其实毫无意义）。
    struct DeclaredTool {
        name: &'static str,
        remote: bool,
        path_args: &'static [&'static str],
    }

    #[async_trait::async_trait]
    impl echo_defs::tool::Tool for DeclaredTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "test tool"
        }
        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<String, echo_defs::tool::ToolError> {
            Ok(format!("{} ran", self.name))
        }
        fn remote_capable(&self) -> bool {
            self.remote
        }
        fn path_arguments(&self) -> &'static [&'static str] {
            self.path_args
        }
    }

    /// 执行即 panic 的工具：验证 router 的 RAII 清理与 panic 终态（D）。
    struct PanicTool;

    #[async_trait::async_trait]
    impl echo_defs::tool::Tool for PanicTool {
        fn name(&self) -> &str {
            "panic_tool"
        }
        fn description(&self) -> &str {
            "always panics"
        }
        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<String, echo_defs::tool::ToolError> {
            panic!("kaboom from test tool")
        }
        fn remote_capable(&self) -> bool {
            true
        }
    }

    /// 建一个装好文件的注册表（5 个文件工具 + bash，语义同内置工具族）。
    fn file_registry() -> Arc<ToolRegistry> {
        let registry = Arc::new(ToolRegistry::new());
        for name in [
            "read_file",
            "write_file",
            "edit_file",
            "list_files",
            "search_code",
        ] {
            registry.register_shared(Arc::new(DeclaredTool {
                name,
                remote: true,
                path_args: &["path"],
            }));
        }
        registry.register_shared(Arc::new(DeclaredTool {
            name: "bash",
            remote: true,
            path_args: &[],
        }));
        registry
    }

    fn policy(tools: &[&str]) -> ExecutorPolicy {
        ExecutorPolicy {
            allow_tools: tools.iter().map(|s| s.to_string()).collect(),
            require_confirm: Vec::new(),
            allow_queries: Vec::new(),
            allow_subagent: false,
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
            allow_subagent: false,
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

    /// 执行端以 `Error` 帧回执（如「未加载任何 persona」「绝对路径越界」）时
    /// 必须**立即**终结等待者：此前该帧落到 `_ => {}` 被丢弃，调用方干等
    /// 460s 硬超时且看不到任何原因（2026-10 实测：node-01m436f8 空 agent，
    /// list_files / bash 均挂满超时）。
    #[tokio::test]
    async fn error_frame_fails_pending_fast() {
        let router = InvokeRouter::new("node-a".into());
        let (req, pending) =
            router.invoke("node-b", "bash", serde_json::json!({"command": "ls"}), None);
        router.dispatch_frame(
            "node-b",
            &FedFrame::Error {
                call_id: Some(req.call_id.clone()),
                code: crate::FedError::Forbidden,
                message: "执行端未加载任何 persona，联邦工具通道不可用".into(),
            },
        );
        let got = tokio::time::timeout(std::time::Duration::from_secs(1), pending.wait())
            .await
            .expect("应即时返回，而非等到硬超时")
            .expect("终态应为 Ok(失败结果)，而非链路断开");
        assert!(!got.success);
        assert!(got.output.contains("未加载任何 persona"), "{}", got.output);
        assert_eq!(router.pending_count(), 0);
    }

    /// 无 call_id 的错误帧（链路级）不得 panic、不得误伤其他 pending。
    #[tokio::test]
    async fn error_frame_without_call_id_is_ignored() {
        let router = InvokeRouter::new("node-a".into());
        let (_req, _pending) = router.invoke("node-b", "bash", serde_json::json!({}), None);
        router.dispatch_frame(
            "node-b",
            &FedFrame::Error {
                call_id: None,
                code: crate::FedError::Internal,
                message: "boom".into(),
            },
        );
        assert_eq!(router.pending_count(), 1);
    }

    /// E（2026-11 复审）：**只有登记 peer** 的终态帧能结算该 pending。
    /// 对端 B 冒用 A 的 call_id 发 InvokeResult 必须被忽略。
    #[tokio::test]
    async fn result_from_another_peer_cannot_settle_a_pending_call() {
        let router = InvokeRouter::new("node-a".into());
        let (req, pending) = router.invoke("node-b", "bash", serde_json::json!({}), None);
        router.dispatch_frame(
            "node-c",
            &FedFrame::InvokeResult(InvokeResult {
                call_id: req.call_id.clone(),
                success: true,
                output: "forged by node-c".into(),
                elapsed_ms: 1,
            }),
        );
        assert_eq!(
            router.pending_count(),
            1,
            "非归属 peer 的 InvokeResult 不得结算 node-b 的条目"
        );
        router.dispatch_frame(
            "node-b",
            &FedFrame::InvokeResult(InvokeResult {
                call_id: req.call_id.clone(),
                success: true,
                output: "real from node-b".into(),
                elapsed_ms: 2,
            }),
        );
        let got = tokio::time::timeout(Duration::from_secs(1), pending.wait())
            .await
            .expect("必须有 deadline")
            .expect("归属 peer 的终态应送达");
        assert_eq!(got.output, "real from node-b");
        assert_eq!(router.pending_count(), 0);
    }

    /// E：Error 帧同样按来源校验（B 不能把 A 的调用打成失败）。
    #[tokio::test]
    async fn error_from_another_peer_cannot_settle_a_pending_call() {
        let router = InvokeRouter::new("node-a".into());
        let (req, pending) = router.invoke("node-b", "bash", serde_json::json!({}), None);
        router.dispatch_frame(
            "node-c",
            &FedFrame::Error {
                call_id: Some(req.call_id.clone()),
                code: crate::FedError::Forbidden,
                message: "forged".into(),
            },
        );
        assert_eq!(router.pending_count(), 1, "非归属 peer 的 Error 不得结算");
        router.dispatch_frame(
            "node-b",
            &FedFrame::InvokeResult(InvokeResult {
                call_id: req.call_id.clone(),
                success: true,
                output: "ok".into(),
                elapsed_ms: 1,
            }),
        );
        let got = tokio::time::timeout(Duration::from_secs(1), pending.wait())
            .await
            .expect("必须有 deadline")
            .expect("归属 peer 的终态应送达");
        assert!(got.success);
        assert_eq!(router.pending_count(), 0);
    }

    /// E：QueryResult 同样按来源校验。
    #[tokio::test]
    async fn query_result_from_another_peer_cannot_settle_a_pending_call() {
        let router = InvokeRouter::new("node-a".into());
        let (req, pending) = router.query(
            "node-b",
            crate::QueryKind::NodeStatus,
            String::new(),
            String::new(),
            0,
            10,
        );
        router.dispatch_frame(
            "node-c",
            &FedFrame::QueryResult(crate::QueryResultFrame {
                call_id: req.call_id.clone(),
                success: true,
                payload: serde_json::json!({"forged": true}),
            }),
        );
        assert_eq!(
            router.pending_count(),
            1,
            "非归属 peer 的 QueryResult 不得结算"
        );
        router.dispatch_frame(
            "node-b",
            &FedFrame::QueryResult(crate::QueryResultFrame {
                call_id: req.call_id.clone(),
                success: true,
                payload: serde_json::json!({"real": true}),
            }),
        );
        let got = tokio::time::timeout(Duration::from_secs(1), pending.wait())
            .await
            .expect("必须有 deadline")
            .expect("归属 peer 的 QueryResult 应送达");
        assert!(got.success);
        assert!(got.output.contains("real"), "{}", got.output);
    }

    /// D（2026-11 复审）：执行任务 panic 后，入站条目必须被 RAII 清理，
    /// 且调用方在 deadline 内收到失败终态（不是干等自身超时）。
    #[tokio::test]
    async fn panicking_execution_cleans_inbound_and_replies_terminal() {
        let router = InvokeRouter::new("node-a".into());
        router.set_policy("node-b", policy(&["*"]));
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(PanicTool));
        let (tx, mut rx) = mpsc::channel(4);
        let reply = router.handle_invoke(
            "node-b",
            InvokeRequest {
                call_id: "node-b:panic-1".into(),
                tool: "panic_tool".into(),
                args: serde_json::json!({}),
                workdir: None,
                timeout_secs: Some(5),
            },
            registry,
            tx,
            &[],
        );
        assert!(matches!(reply, FedFrame::InvokeAccepted { .. }));
        let (peer, frame) = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("panic 终态必须在 deadline 内到达")
            .expect("send_result 通道不应关闭");
        assert_eq!(peer, "node-b");
        match frame {
            FedFrame::InvokeResult(result) => {
                assert!(!result.success, "panic 必须映射为失败终态");
                assert!(
                    result.output.contains("kaboom"),
                    "应带回 panic 消息：{}",
                    result.output
                );
            }
            other => panic!("expected InvokeResult, got {other:?}"),
        }
        assert_eq!(
            router.inbound_count(),
            0,
            "panic 后入站条目必须被清理（否则泄漏且占用并发额度）"
        );
    }

    /// `NeedsConfirm` 是终态（v1 无确认通道）：不终结就会挂到硬超时。
    #[tokio::test]
    async fn needs_confirm_fails_pending_fast() {
        let router = InvokeRouter::new("node-a".into());
        let (req, pending) = router.invoke("node-b", "bash", serde_json::json!({}), None);
        router.dispatch_frame(
            "node-b",
            &FedFrame::InvokeAccepted {
                call_id: req.call_id.clone(),
                verdict: InvokeVerdict::NeedsConfirm,
            },
        );
        let got = tokio::time::timeout(std::time::Duration::from_secs(1), pending.wait())
            .await
            .expect("应即时返回")
            .expect("终态应为 Ok(失败结果)");
        assert!(!got.success);
        assert!(got.output.contains("确认"), "{}", got.output);
        assert_eq!(router.pending_count(), 0);
    }

    /// `Accepted` 回执**不得**终结 pending（结果帧随后才到）。
    #[tokio::test]
    async fn accepted_verdict_keeps_pending() {
        let router = InvokeRouter::new("node-a".into());
        let (req, pending) = router.invoke("node-b", "bash", serde_json::json!({}), None);
        router.dispatch_frame(
            "node-b",
            &FedFrame::InvokeAccepted {
                call_id: req.call_id.clone(),
                verdict: InvokeVerdict::Accepted,
            },
        );
        assert_eq!(router.pending_count(), 1);
        router.dispatch_frame(
            "node-b",
            &FedFrame::InvokeResult(InvokeResult {
                call_id: req.call_id.clone(),
                success: true,
                output: "ok".into(),
                elapsed_ms: 1,
            }),
        );
        let got = pending.wait().await.expect("result");
        assert!(got.success);
        assert_eq!(got.output, "ok");
    }

    /// 等待方放弃（receiver 被 drop，如短超时查询）的条目：sweep 清理、
    /// 不 panic、不泄漏——且与 ttl 超时走不同日志路径（debug vs warn，
    /// 2026-10 日志分级修复的行为契约）。
    #[tokio::test]
    async fn sweep_clears_abandoned_pending() {
        let router = InvokeRouter::new("node-a".into());
        let (_req, pending) = router.invoke("node-b", "bash", serde_json::json!({}), None);
        assert_eq!(router.pending_count(), 1);
        // 模拟 3s 短超时的查询：把等待句柄丢掉（receiver closed）。
        drop(pending);
        // 下一次 invoke 入口顺手 sweep（对端尚未回复，条目为 abandoned）。
        let (_req2, pending2) = router.invoke("node-b", "bash", serde_json::json!({}), None);
        assert_eq!(
            router.pending_count(),
            1,
            "abandoned 条目应被清理，只剩新注册的一条"
        );
        drop(pending2);
        router.sweep();
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
                stream: crate::OutputStream::Stdout,
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
                code: crate::FedError::LoopDetected,
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
                call_id: "node-b:1-0".into(),
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
    /// P3-1：执行端沙箱——5 个文件工具的绝对路径落在工作区根并集外时拒绝
    /// （FedError::Forbidden），调用侧透传为 ToolError。**仅限受限策略**：
    /// 完全开放（allow_queries = ["*"]）豁免沙箱（见下方测试）。
    #[tokio::test]
    async fn executor_rejects_absolute_path_outside_roots() {
        let router = InvokeRouter::new("node-a".into());
        router.set_policy(
            "node-b",
            ExecutorPolicy {
                allow_tools: vec!["*".into()],
                require_confirm: vec![],
                allow_queries: vec!["workspace_files".into()],
                allow_subagent: false,
            },
        );
        let registry = file_registry();
        let (tx, _rx) = mpsc::channel(1);
        let root = std::env::temp_dir().join(format!("echo-fed-roots-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for tool in [
            "read_file",
            "write_file",
            "edit_file",
            "list_files",
            "search_code",
        ] {
            let reply = router.handle_invoke(
                "node-b",
                InvokeRequest {
                    call_id: format!("node-b:1-{tool}"),
                    tool: tool.into(),
                    args: serde_json::json!({"path": "/etc/shadow"}),
                    workdir: None,
                    timeout_secs: None,
                },
                registry.clone(),
                tx.clone(),
                std::slice::from_ref(&root),
            );
            assert!(
                matches!(
                    reply,
                    FedFrame::Error {
                        code: crate::FedError::Forbidden,
                        ..
                    }
                ),
                "{tool} outside roots must be Forbidden: {reply:?}"
            );
        }
        // 根内绝对路径放行（裁决通过 → InvokeAccepted）。
        let inside = root.join("ok.txt");
        std::fs::write(&inside, "x").unwrap();
        let reply = router.handle_invoke(
            "node-b",
            InvokeRequest {
                call_id: "node-b:2-0".into(),
                tool: "read_file".into(),
                args: serde_json::json!({"path": inside.to_string_lossy()}),
                workdir: None,
                timeout_secs: None,
            },
            registry,
            tx,
            std::slice::from_ref(&root),
        );
        assert!(matches!(
            reply,
            FedFrame::InvokeAccepted {
                verdict: InvokeVerdict::Accepted,
                ..
            }
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 完全开放豁免（2026-10 设计演进）：`allow_queries = ["*"]` 的 peer
    /// 调用文件工具时不做工作区根裁决——任意绝对路径直达本机工具实现
    /// （「SSH 免密」同语义）。
    #[tokio::test]
    async fn executor_full_access_skips_workspace_sandbox() {
        let router = InvokeRouter::new("node-a".into());
        router.set_policy("node-b", ExecutorPolicy::full());
        let registry = file_registry();
        let (tx, _rx) = mpsc::channel(1);
        let reply = router.handle_invoke(
            "node-b",
            InvokeRequest {
                call_id: "node-b:3-0".into(),
                tool: "read_file".into(),
                args: serde_json::json!({"path": "/etc/hostname"}),
                workdir: None,
                timeout_secs: None,
            },
            registry,
            tx,
            &[], // 工作区根为空也不拦截
        );
        assert!(
            matches!(
                reply,
                FedFrame::InvokeAccepted {
                    verdict: InvokeVerdict::Accepted,
                    ..
                }
            ),
            "full-access peer must bypass sandbox: {reply:?}"
        );
    }

    #[test]
    fn query_authorization_defaults() {
        use crate::QueryKind;
        let default = ExecutorPolicy::default();
        assert!(default.query_allowed(QueryKind::NodeStatus));
        assert!(!default.query_allowed(QueryKind::SessionSnapshot));
        assert!(!default.query_allowed(QueryKind::WorkspaceFiles));
        let open = ExecutorPolicy {
            allow_tools: vec![],
            require_confirm: vec![],
            allow_queries: vec!["session_snapshot".into()],
            allow_subagent: false,
        };
        assert!(open.query_allowed(QueryKind::SessionSnapshot));
        assert!(!open.query_allowed(QueryKind::WorkspaceFiles));
        let all = ExecutorPolicy {
            allow_tools: vec![],
            require_confirm: vec![],
            allow_queries: vec!["*".into()],
            allow_subagent: false,
        };
        assert!(all.query_allowed(QueryKind::WorkspaceFiles));
    }

    #[test]
    fn full_access_flag_follows_allow_queries_wildcard() {
        assert!(!ExecutorPolicy::default().is_full_access());
        assert!(ExecutorPolicy::full().is_full_access());
        let explicit_only = ExecutorPolicy {
            allow_tools: vec![],
            require_confirm: vec![],
            allow_queries: vec!["workspace_files".into()],
            allow_subagent: false,
        };
        assert!(!explicit_only.is_full_access());
    }

    #[tokio::test]
    async fn query_result_routed_to_waiting_caller() {
        let router = InvokeRouter::new("node-a".into());
        let (req, pending) = router.query(
            "node-b",
            crate::QueryKind::NodeStatus,
            String::new(),
            String::new(),
            0,
            0,
        );
        router.dispatch_frame(
            "node-b",
            &FedFrame::QueryResult(crate::QueryResultFrame {
                call_id: req.call_id,
                success: true,
                payload: serde_json::json!({"node_id": "node-b"}),
            }),
        );
        let got = pending.wait().await.expect("terminal");
        assert!(got.success);
        assert!(got.output.contains("node-b"));
    }

    /// 候选集**完全**由对端宣告决定（S8）：本机不再有"支持集"名单。
    ///
    /// 这里故意放一个本机并不实现的名字（`their_module_tool`）——对端能宣告
    /// 它就说明它的工具声明了 `remote_capable`，我们照镜像；重复项去重保序。
    #[test]
    fn candidates_mirror_peer_caps_exactly() {
        let caps = NodeCaps {
            panel_ready: false,
            tools: vec![
                "read_file".into(),
                "their_module_tool".into(),
                "read_file".into(),
                "".into(),
            ],
            subagent: true,
            workspaces: vec![],
            active_turns: 0,
            tool_definitions: Vec::new(),
        };
        assert_eq!(
            remote_tool_candidates(&caps),
            vec!["read_file", "their_module_tool"],
            "宣告什么镜像什么：去重、保序、忽略空名"
        );
    }

    /// 执行端准入（S8）：注册表里没有 / 没声明可远程 → 明确拒绝。
    #[tokio::test]
    async fn executor_rejects_unknown_and_non_remote_tools() {
        let router = InvokeRouter::new("node-a".into());
        router.set_policy("node-b", policy(&["*"]));
        let (tx, _rx) = mpsc::channel(1);
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(DeclaredTool {
            name: "workspace",
            remote: false, // 改宿主状态的管理工具：不声明远程面
            path_args: &[],
        }));

        let unknown = router.handle_invoke(
            "node-b",
            InvokeRequest {
                call_id: "node-b:9-0".into(),
                tool: "no_such_tool".into(),
                args: serde_json::json!({}),
                workdir: None,
                timeout_secs: None,
            },
            registry.clone(),
            tx.clone(),
            &[],
        );
        assert!(
            matches!(
                unknown,
                FedFrame::Error {
                    code: crate::FedError::UnknownCall,
                    ..
                }
            ),
            "未注册的工具必须报 UnknownCall（此前按名字过沙箱后静默失败）: {unknown:?}"
        );

        let not_remote = router.handle_invoke(
            "node-b",
            InvokeRequest {
                call_id: "node-b:9-1".into(),
                tool: "workspace".into(),
                args: serde_json::json!({}),
                workdir: None,
                timeout_secs: None,
            },
            registry,
            tx,
            &[],
        );
        assert!(
            matches!(
                not_remote,
                FedFrame::Error {
                    code: crate::FedError::Forbidden,
                    ..
                }
            ),
            "未声明 remote_capable 的工具即使 allow_tools=[\"*\"] 也不得执行: {not_remote:?}"
        );
    }

    // ── 入站执行治理（2026-11）──

    /// 慢工具：验证截止 / 取消只**停止等待**（不改变工具自身的资源语义）。
    struct SlowTool {
        name: &'static str,
        delay: Duration,
        required: &'static [&'static str],
    }

    #[async_trait::async_trait]
    impl echo_defs::tool::Tool for SlowTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "slow test tool"
        }
        fn parameters(&self) -> serde_json::Value {
            let mut properties = serde_json::Map::new();
            for field in self.required {
                properties.insert((*field).to_string(), serde_json::json!({"type": "string"}));
            }
            serde_json::json!({
                "type": "object",
                "properties": properties,
                "required": self.required,
            })
        }
        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<String, echo_defs::tool::ToolError> {
            tokio::time::sleep(self.delay).await;
            Ok("slow done".into())
        }
        fn remote_capable(&self) -> bool {
            true
        }
    }

    /// 记录 `ToolContext.peer` 的执行端工具——证明主体来自**传输层注入**。
    struct PeerEcho {
        seen: Arc<std::sync::Mutex<Vec<Option<String>>>>,
    }

    #[async_trait::async_trait]
    impl echo_defs::tool::Tool for PeerEcho {
        fn name(&self) -> &str {
            "peer_echo"
        }
        fn description(&self) -> &str {
            "records the injected peer"
        }
        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<String, echo_defs::tool::ToolError> {
            Ok("no context".into())
        }
        async fn execute_in(
            &self,
            cx: &echo_defs::tool::ToolContext,
            _arguments: serde_json::Value,
        ) -> Result<echo_defs::tool::ToolResult, echo_defs::tool::ToolError> {
            self.seen.lock().unwrap().push(cx.peer.clone());
            Ok(echo_defs::tool::ToolResult::text(format!(
                "peer={:?}",
                cx.peer
            )))
        }
        fn remote_capable(&self) -> bool {
            true
        }
    }

    fn req(call_id: &str, tool: &str, args: serde_json::Value) -> InvokeRequest {
        InvokeRequest {
            call_id: call_id.into(),
            tool: tool.into(),
            args,
            workdir: None,
            timeout_secs: None,
        }
    }

    /// 执行端用**传输层注入的 peer** 构造 `ToolContext`：参数里的任何
    /// `__team_id` 都不影响主体（这里是 `node-b`，不是参数值）。
    #[tokio::test]
    async fn remote_principal_is_injected_from_the_link() {
        let router = InvokeRouter::new("node-a".into());
        router.set_policy("node-b", policy(&["*"]));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(PeerEcho { seen: seen.clone() }));
        let (tx, mut rx) = mpsc::channel(4);
        let mut request = req(
            "node-b:1-0",
            "peer_echo",
            serde_json::json!({"__team_id": "forged"}),
        );
        request.timeout_secs = Some(5);
        let reply = router.handle_invoke("node-b", request, registry, tx, &[]);
        assert!(matches!(
            reply,
            FedFrame::InvokeAccepted {
                verdict: InvokeVerdict::Accepted,
                ..
            }
        ));
        let (_peer, frame) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("terminal")
            .expect("frame");
        match frame {
            FedFrame::InvokeResult(result) => {
                assert!(result.success, "{}", result.output);
                assert!(result.output.contains("node-b"), "{}", result.output);
            }
            other => panic!("expected InvokeResult, got {other:?}"),
        }
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [Some("node-b".to_string())]
        );
        assert_eq!(router.inbound_count(), 0, "终态后入站表必须清空");
    }

    /// 截止时间：到点停止等待并回终态，入站表清干净。
    #[tokio::test]
    async fn inbound_deadline_stops_waiting_and_returns_terminal() {
        let router = InvokeRouter::new_with_grace("node-a".into(), Duration::from_millis(100));
        router.set_policy("node-b", policy(&["*"]));
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(SlowTool {
            name: "slow",
            delay: Duration::from_secs(30),
            required: &[],
        }));
        let (tx, mut rx) = mpsc::channel(4);
        let mut request = req("node-b:2-0", "slow", serde_json::json!({}));
        request.timeout_secs = Some(1);
        let reply = router.handle_invoke("node-b", request, registry, tx, &[]);
        assert!(matches!(
            reply,
            FedFrame::InvokeAccepted {
                verdict: InvokeVerdict::Accepted,
                ..
            }
        ));
        let (_peer, frame) = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("deadline must fire")
            .expect("frame");
        match frame {
            FedFrame::InvokeResult(result) => {
                assert!(!result.success);
                assert!(result.output.contains("超时"), "{}", result.output);
            }
            other => panic!("expected InvokeResult, got {other:?}"),
        }
        assert_eq!(router.inbound_count(), 0);
    }

    /// 取消只能来自调用发起方：其他 peer 的 Cancel 只记审计、不生效。
    #[tokio::test]
    async fn cancel_from_another_peer_is_ignored() {
        let router = InvokeRouter::new("node-a".into());
        router.set_policy("node-b", policy(&["*"]));
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(SlowTool {
            name: "slow",
            delay: Duration::from_secs(30),
            required: &[],
        }));
        let (tx, mut rx) = mpsc::channel(4);
        let reply = router.handle_invoke(
            "node-b",
            req("node-b:3-0", "slow", serde_json::json!({})),
            registry,
            tx,
            &[],
        );
        assert!(matches!(reply, FedFrame::InvokeAccepted { .. }));
        // 非来源 peer 的取消：忽略。
        router.dispatch_frame(
            "node-c",
            &FedFrame::Cancel {
                call_id: "node-b:3-0".into(),
            },
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), rx.recv())
                .await
                .is_err(),
            "foreign cancel must not terminate the call"
        );
        assert_eq!(router.inbound_count(), 1);
        // 来源 peer 的取消：生效并回终态。
        router.dispatch_frame(
            "node-b",
            &FedFrame::Cancel {
                call_id: "node-b:3-0".into(),
            },
        );
        let (_peer, frame) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("origin cancel must terminate")
            .expect("frame");
        match frame {
            FedFrame::InvokeResult(result) => {
                assert!(!result.success);
                assert!(result.output.contains("取消"), "{}", result.output);
            }
            other => panic!("expected InvokeResult, got {other:?}"),
        }
        assert_eq!(router.inbound_count(), 0);
        // 迟到/未知 call 的取消：no-op（不 panic）。
        router.dispatch_frame(
            "node-b",
            &FedFrame::Cancel {
                call_id: "node-b:3-0".into(),
            },
        );
    }

    /// 同一 call_id 不得有两个执行体（重放/串味）。
    #[tokio::test]
    async fn duplicate_call_id_is_rejected_while_in_flight() {
        let router = InvokeRouter::new("node-a".into());
        router.set_policy("node-b", policy(&["*"]));
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(SlowTool {
            name: "slow",
            delay: Duration::from_secs(30),
            required: &[],
        }));
        let (tx, _rx) = mpsc::channel(4);
        let first = router.handle_invoke(
            "node-b",
            req("node-b:4-0", "slow", serde_json::json!({})),
            registry.clone(),
            tx.clone(),
            &[],
        );
        assert!(matches!(first, FedFrame::InvokeAccepted { .. }));
        let second = router.handle_invoke(
            "node-b",
            req("node-b:4-0", "slow", serde_json::json!({})),
            registry,
            tx,
            &[],
        );
        assert!(
            matches!(
                second,
                FedFrame::Error {
                    code: crate::FedError::Forbidden,
                    ..
                }
            ),
            "duplicate call_id must be rejected: {second:?}"
        );
        router.dispatch_frame(
            "node-b",
            &FedFrame::Cancel {
                call_id: "node-b:4-0".into(),
            },
        );
        assert_eq!(router.inbound_count(), 1, "只应有第一个调用仍在登记");
    }

    /// 参数 schema 预检：缺必需字段 → 立即终态明文，不占用执行资源。
    #[tokio::test]
    async fn argument_preflight_fails_fast_with_readable_message() {
        let router = InvokeRouter::new("node-a".into());
        router.set_policy("node-b", policy(&["*"]));
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(SlowTool {
            name: "need_args",
            delay: Duration::from_millis(1),
            required: &["path"],
        }));
        let (tx, _rx) = mpsc::channel(4);
        let reply = router.handle_invoke(
            "node-b",
            req("node-b:5-0", "need_args", serde_json::json!({})),
            registry,
            tx,
            &[],
        );
        match reply {
            FedFrame::InvokeResult(result) => {
                assert!(!result.success);
                assert!(result.output.contains("缺少必需参数"), "{}", result.output);
            }
            other => panic!("preflight must answer with a terminal result: {other:?}"),
        }
        assert_eq!(router.inbound_count(), 0, "预检失败不得登记入站调用");
    }

    /// 并发上限：超过 per-peer 额度立刻回 busy（不排队到调用方超时）。
    #[tokio::test]
    async fn per_peer_concurrency_limit_reports_busy() {
        let router = InvokeRouter::new("node-a".into());
        router.set_policy("node-b", policy(&["*"]));
        let registry = Arc::new(ToolRegistry::new());
        registry.register_shared(Arc::new(SlowTool {
            name: "slow",
            delay: Duration::from_secs(30),
            required: &[],
        }));
        let (tx, mut rx) = mpsc::channel(64);
        for index in 0..MAX_INBOUND_PER_PEER {
            let reply = router.handle_invoke(
                "node-b",
                req(&format!("node-b:6-{index}"), "slow", serde_json::json!({})),
                registry.clone(),
                tx.clone(),
                &[],
            );
            assert!(
                matches!(reply, FedFrame::InvokeAccepted { .. }),
                "{reply:?}"
            );
        }
        let busy = router.handle_invoke(
            "node-b",
            req("node-b:6-over", "slow", serde_json::json!({})),
            registry,
            tx,
            &[],
        );
        match busy {
            FedFrame::Error { code, message, .. } => {
                assert_eq!(code, crate::FedError::Internal);
                assert!(message.contains("busy"), "{message}");
            }
            other => panic!("over-limit call must be rejected: {other:?}"),
        }
        // 收尾：取消全部在飞调用，验证额度会释放。
        for index in 0..MAX_INBOUND_PER_PEER {
            router.dispatch_frame(
                "node-b",
                &FedFrame::Cancel {
                    call_id: format!("node-b:6-{index}"),
                },
            );
        }
        for _ in 0..MAX_INBOUND_PER_PEER {
            let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
        }
        assert_eq!(router.inbound_count(), 0);
        // 额度已释放：新调用不再 busy。
        let fresh_registry = Arc::new(ToolRegistry::new());
        fresh_registry.register_shared(Arc::new(SlowTool {
            name: "slow",
            delay: Duration::from_secs(30),
            required: &[],
        }));
        let after = router.handle_invoke(
            "node-b",
            req("node-b:6-after", "slow", serde_json::json!({})),
            fresh_registry,
            mpsc::channel(4).0,
            &[],
        );
        assert!(
            matches!(after, FedFrame::InvokeAccepted { .. }),
            "{after:?}"
        );
        router.dispatch_frame(
            "node-b",
            &FedFrame::Cancel {
                call_id: "node-b:6-after".into(),
            },
        );
    }

    /// 统一入口校验：call_id origin 必须等于来源 peer（防冒用他人 id）。
    #[tokio::test]
    async fn call_id_origin_must_match_the_link_peer() {
        let router = InvokeRouter::new("node-a".into());
        router.set_policy("node-b", policy(&["*"]));
        let registry = file_registry();
        let (tx, _rx) = mpsc::channel(4);
        let reply = router.handle_invoke(
            "node-b",
            req("node-evil:1-0", "bash", serde_json::json!({})),
            registry,
            tx,
            &[],
        );
        match reply {
            FedFrame::Error { code, message, .. } => {
                assert_eq!(code, crate::FedError::Forbidden);
                assert!(message.contains("origin"), "{message}");
            }
            other => panic!("origin mismatch must be Forbidden: {other:?}"),
        }
    }

    /// 统一入口校验：args 必须是对象（数组/标量一律拒绝）。
    #[tokio::test]
    async fn non_object_arguments_are_rejected() {
        let router = InvokeRouter::new("node-a".into());
        router.set_policy("node-b", policy(&["*"]));
        let registry = file_registry();
        let (tx, _rx) = mpsc::channel(4);
        let reply = router.handle_invoke(
            "node-b",
            req("node-b:7-0", "bash", serde_json::json!([1, 2, 3])),
            registry,
            tx,
            &[],
        );
        assert!(
            matches!(
                reply,
                FedFrame::Error {
                    code: crate::FedError::Forbidden,
                    ..
                }
            ),
            "{reply:?}"
        );
    }
}
