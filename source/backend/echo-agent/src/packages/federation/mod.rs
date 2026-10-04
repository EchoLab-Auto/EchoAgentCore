//! 联邦执行层（federation Phase 2）：远程工具调用的大脑侧与执行端。
//!
//! - [`router`]：调用调度——`call_id → oneshot` 路由表，消费链路飞来的
//!   `InvokeAccepted/InvokeOutput/InvokeResult`，按 call_id 派发给等待中的
//!   代理工具；同时承担执行端入口（收 `Invoke` → 白名单裁决 → 本机注册表
//!   执行 → 回传）。
//! - [`remote_tool`]：`RemoteTool` 代理工具——`Tool` trait 实现，`execute()`
//!   内发 `FedFrame::Invoke` 并等待终态。
//!
//! 设计约束（RFC §4）：不改 `Tool` trait；远程工具以 `<peer>:<tool>` 命名
//! 显式注册；路径校验由执行端在本机工作区根内完成；取消/超时尽力送达。

pub mod aggregator;
pub mod remote_tool;
pub mod router;

pub use remote_tool::{proxy_schema, RemoteTool};
pub use router::{remote_tool_candidates, ExecutorPolicy, InvokeRouter, PendingInvoke};

// ── Phase 3：远程 subagent 对端观测出口 ──
//
// 大脑侧（spawn_subagent node=...）受理/完成时经本出口通知对端；
// 组合根装配期注入（需要 Federation 句柄——不在 Agent 能力面内）。
// 未注入（联邦关闭）时为 no-op。

pub use echo_federation::SubagentStatus;

/// 观测出口类型：`(peer, call_id, task, timeout, status, result)`。
pub type RemoteSubagentNotifier = std::sync::Arc<
    dyn Fn(&str, &str, &str, Option<u64>, SubagentStatus, Option<String>) + Send + Sync,
>;

static REMOTE_SUBAGENT_NOTIFIER: std::sync::OnceLock<RemoteSubagentNotifier> =
    std::sync::OnceLock::new();

pub fn set_remote_subagent_notifier(notifier: RemoteSubagentNotifier) {
    let _ = REMOTE_SUBAGENT_NOTIFIER.set(notifier);
}

/// 通知对端远程子任务状态（联邦关闭时 no-op）。
pub fn notify_remote_subagent(
    peer: &str,
    call_id: &str,
    task: &str,
    timeout_secs: Option<u64>,
    status: SubagentStatus,
    result: Option<String>,
) {
    // P3-3 聚合销账（大脑侧）：终态时本组销账——登记在本机（spawn
    // 受理处），对端收帧方的 settle 是另一条路径（执行端真正执行并
    // 回报的场景）。终态判定排除 Running（受理通知）。
    if !matches!(status, SubagentStatus::Running) {
        if let Some(summary) = aggregator::settle_remote_subagent(call_id, status, result.clone()) {
            // 聚合摘要投递经组合根出口（进程级；未装配时摘要仅记日志）。
            if let Some(deliver) = AGGREGATE_DELIVER.get() {
                deliver(summary);
            } else {
                tracing::info!(
                    target: "federation",
                    group = %summary.group_key, "aggregate summary (no deliver wired)"
                );
            }
        }
    }
    if let Some(notify) = REMOTE_SUBAGENT_NOTIFIER.get() {
        notify(peer, call_id, task, timeout_secs, status, result);
    }
}

/// 聚合摘要投递出口（组合根 core/main.rs 装配时注入；向父会话
/// timeline 追加 system 消息需要 supervisor 句柄——echo-agent crate
/// 不可达）。
pub type AggregateDeliver = std::sync::Arc<dyn Fn(aggregator::AggregateSummary) + Send + Sync>;

static AGGREGATE_DELIVER: std::sync::OnceLock<AggregateDeliver> = std::sync::OnceLock::new();

/// 装配期注入聚合摘要投递出口（重复调用保留首个）。
pub fn set_aggregate_deliver(deliver: AggregateDeliver) {
    let _ = AGGREGATE_DELIVER.set(deliver);
}

// ── P3-1：跨机文件协作分流出口 ──
//
// 5 个文件工具（read_file/write_file/edit_file/list_files/search_code）的
// `path` 参数带 `node://<peer>/<绝对路径>` 前缀时，echo-agent 侧分流层
// （tools_builtin::coding::RemoteAwareTool）经本出口把调用转发到对端同名
// 工具执行。装配层注入的闭包负责：peer 名 → node_id 反查
// （FederationRuntime.peer_names——InvokeRouter 只按 node_id 路由，无
// 按名解析方法）→ `InvokeRouter::invoke` + `Federation::send_to` → 等终态。
// 未接线（联邦关闭）时分流层报「联邦未接线」。

/// 远程文件调用出口：`(peer_name, tool, args) → 对端输出`。
/// `Err` 文本原样透传为 `ToolError::Execution`（对端白名单/沙箱拒绝、
/// 链路失败、超时等）。
pub type RemoteInvoker = std::sync::Arc<
    dyn Fn(
            String,
            String,
            serde_json::Value,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

static REMOTE_INVOKER: std::sync::OnceLock<RemoteInvoker> = std::sync::OnceLock::new();

/// 装配期注入远程文件调用出口（重复调用保留首个，与 notifier 同约定）。
pub fn set_remote_invoker(invoker: RemoteInvoker) {
    let _ = REMOTE_INVOKER.set(invoker);
}

/// 当前远程文件调用出口（None = 联邦未接线）。
pub fn remote_invoker() -> Option<RemoteInvoker> {
    REMOTE_INVOKER.get().cloned()
}

/// 联邦沙箱：绝对路径是否落在给定工作区根并集内（canonicalize 后按
/// 路径分量前缀判定；不存在的路径退回其父链最近已存在祖先）。
/// 用于执行端拒绝"工作区外绝对路径"的远程文件调用（防 ~/.ssh 等读取）。
pub fn path_within_roots(raw: &str, roots: &[std::path::PathBuf]) -> bool {
    let target = std::path::Path::new(raw);
    // canonicalize 目标或其最近已存在祖先（防符号链接逃逸 + 新建文件场景）。
    let mut probe = target;
    let canonical_target = loop {
        match probe.canonicalize() {
            Ok(c) => break c,
            Err(_) => match probe.parent() {
                Some(p) => probe = p,
                None => return false,
            },
        }
    };
    roots.iter().any(|root| {
        root.canonicalize()
            .map(|r| canonical_target.starts_with(r))
            .unwrap_or(false)
    })
}
