//! 联邦出口集合（`FederationPorts`）：装配期构造一份，显式下发。
//!
//! ## 它是什么
//!
//! 六个出口 + 一个共享聚合器。它们**天然同源同寿命**（同一次联邦装配产生），
//! 所以是一个值而不是六个参数；`Clone` 廉价、无全局槽位。
//!
//! ## 为什么 `command_handler` 不再引用 `Agent`
//!
//! 搬进独立 crate 时唯一的宿主依赖是这个类型：`Fn(&Agent, BackendCommand)`。
//! 但它的实现（组合根的 `handle_federation_command`）真正用到的只有
//! **事件出口**（`emit`）——`Agent` 是多余的一层。改成
//! `Fn(BackendCommand) -> future`，由构造方在装配期捕获出口，于是本 crate
//! 对宿主**零依赖**（`scripts/deps-policy.txt` 上有门禁）。
//!
//! 顺带修掉一个隐性耦合：此前每个 persona 都会把"自己的 emit"塞进同一个
//! 进程级 ports，谁的装配最后跑谁生效。现在出口在装配期固定（进程级汇聚点），
//! 与"哪个 agent 恰好装配"无关。

use std::sync::Arc;

use crate::aggregator;

// ── 联邦出口集合（P3 终态：参数下发，不再经 kernel cell）──
//
// 这些出口都由**组合根**在装配联邦时构造（它们必须持有 `Federation` /
// `InvokeRouter` / supervisor 句柄，全在组合根手里），由 Agent 与联邦泵
// 在运行期使用。此前每个出口各占一个进程级 kernel cell，读写两侧隔着
// 全局表；现在收敛成一个 `FederationPorts` 值，装配期构造一份、显式
// 传给需要它的两方（Agent / 组合根自己的联邦泵）。
//
// 为什么仍然是一个"集合"而不是逐个参数：它们天然同源同寿命（同一次联邦
// 装配产生），拆成 5 个参数只是在每层签名里重复同一件事；而集合本身是
// **值**（Clone 廉价、无全局槽位），不构成新的耦合点。

pub use echo_federation::SubagentStatus;

/// 联邦管理命令的异步返回（组合根的实现要 await 网络 / 磁盘）。
pub type FederationCommandFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>;

/// 联邦管理命令处理器：`BackendCommand` 进，副作用（发事件）出。
///
/// 装配期由组合根构造（它持有 `Federation` 句柄、`ConfigStore` 与事件出口），
/// 插件命令域只负责"这条命令归我"的注册关系。
pub type FederationCommandHandler =
    Arc<dyn Fn(echo_protocol::BackendCommand) -> FederationCommandFuture + Send + Sync>;

/// 远程 subagent 观测出口：`(peer, call_id, task, timeout, status, result)`。
pub type RemoteSubagentNotifier = std::sync::Arc<
    dyn Fn(&str, &str, &str, Option<u64>, SubagentStatus, Option<String>) + Send + Sync,
>;

/// 聚合摘要投递出口（向父会话 timeline 追加 system 消息；需要 supervisor
/// 句柄——echo-agent crate 不可达，只能由组合根注入）。
pub type AggregateDeliver = std::sync::Arc<dyn Fn(aggregator::AggregateSummary) + Send + Sync>;

/// node:// 远程文件 / 命令分流出口：`(peer_name, tool, args) → 对端输出`。
/// `Err` 文本原样透传（对端白名单 / 沙箱拒绝、链路失败、超时等）。
///
/// 类型与 `echo-plugin-tools` 共享（工具就注册在那个 crate 里）。
pub type RemoteInvoker = echo_plugin_tools::RemoteInvoker;

/// 远程**只读查询**发起函数（跨机工作区用）：
/// `(peer, kind, subject) → Result<payload_json, err>`。
pub type RemoteQuerier = std::sync::Arc<
    dyn Fn(
            String,
            echo_federation::QueryKind,
            String,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send>,
        > + Send
        + Sync,
>;

/// 联邦出口集合（组合根装配期构造一份，显式下发）。
///
/// [`Default`] = **联邦未接线**：全部出口为空、聚合器为空表——远程功能
/// 一律 fail-closed（工具报「联邦未接线」、远程目录降级为不可采集）。
#[derive(Clone)]
pub struct FederationPorts {
    /// node:// 分流出口（None = 联邦关闭）。
    pub invoker: Option<RemoteInvoker>,
    /// 跨机工作区只读查询（None = 联邦关闭）。
    pub querier: Option<RemoteQuerier>,
    /// 远程 subagent 状态上报对端（None = 联邦关闭，静默）。
    pub notifier: Option<RemoteSubagentNotifier>,
    /// 聚合摘要投递（None = 只记日志）。
    pub deliver: Option<AggregateDeliver>,
    /// 跨机子代理结果聚合器（登记 / 销账 / 产出摘要）。进程内一份：
    /// 大脑侧登记与组合根联邦泵销账必须看同一张分组表。
    pub aggregator: std::sync::Arc<aggregator::RemoteSubagentAggregator>,
    /// 联邦管理命令处理器（`SaveFederationPeer` / 邀请 / 会话迁移 /
    /// 自更新等）：需要 `Federation` 句柄与 ConfigStore，只能在组合根里
    /// 实现。None = 联邦未接线，命令回可读错误。
    ///
    /// 类型**不引用 `Agent`**：实现只需要事件出口，装配期捕获即可（见模块
    /// 文档"为什么 `command_handler` 不再引用 `Agent`"）。
    pub command_handler: Option<FederationCommandHandler>,
}

impl Default for FederationPorts {
    fn default() -> Self {
        Self {
            invoker: None,
            querier: None,
            notifier: None,
            deliver: None,
            aggregator: aggregator::RemoteSubagentAggregator::new(),
            command_handler: None,
        }
    }
}

impl std::fmt::Debug for FederationPorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FederationPorts")
            .field("invoker", &self.invoker.is_some())
            .field("querier", &self.querier.is_some())
            .field("notifier", &self.notifier.is_some())
            .field("deliver", &self.deliver.is_some())
            .field("command_handler", &self.command_handler.is_some())
            .finish()
    }
}

impl FederationPorts {
    /// 空集合（联邦未接线）。
    pub fn unlinked() -> Self {
        Self::default()
    }

    /// 是否至少有一条出口接过线（观测用）。
    pub fn is_linked(&self) -> bool {
        self.invoker.is_some()
            || self.querier.is_some()
            || self.notifier.is_some()
            || self.deliver.is_some()
            || self.command_handler.is_some()
    }
}

/// 通知对端远程子任务状态（未注入 notifier = no-op）。
///
/// 顺带做大脑侧聚合销账：终态（非 Running）时先回调聚合器，组内全员
/// 终态则产出一条摘要，经 [`FederationPorts::deliver`] 投递到父会话
/// timeline（未装配则只记日志）。
pub fn notify_remote_subagent(
    ports: &FederationPorts,
    peer: &str,
    call_id: &str,
    task: &str,
    timeout_secs: Option<u64>,
    status: SubagentStatus,
    result: Option<String>,
) {
    if !matches!(status, SubagentStatus::Running) {
        if let Some(summary) =
            aggregator::settle_remote_subagent(&ports.aggregator, call_id, status, result.clone())
        {
            if let Some(deliver) = &ports.deliver {
                deliver(summary);
            } else {
                tracing::info!(
                    target: "federation",
                    group = %summary.group_key, "aggregate summary (no deliver wired)"
                );
            }
        }
    }
    if let Some(notify) = &ports.notifier {
        notify(peer, call_id, task, timeout_secs, status, result);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// 缺省集合 = 联邦未接线：任何通知都不 panic，也不吞掉聚合语义
    ///（登记仍进聚合器，只是没人投递——与接线前行为一致）。
    #[test]
    fn unlinked_ports_swallow_notifications_without_panicking() {
        let ports = FederationPorts::unlinked();
        assert!(!ports.is_linked());
        assert!(ports.invoker.is_none());
        assert!(ports.querier.is_none());
        assert!(ports.notifier.is_none());
        assert!(ports.deliver.is_none());
        assert!(ports.command_handler.is_none());
        // 未接线：受理（Running）与终态都不应 panic。
        notify_remote_subagent(
            &ports,
            "peer-a",
            "call-1",
            "task",
            Some(5),
            SubagentStatus::Running,
            None,
        );
        notify_remote_subagent(
            &ports,
            "peer-a",
            "call-1",
            "task",
            None,
            SubagentStatus::Completed,
            Some("ok".into()),
        );
    }

    /// **同一份聚合器**是 ports 的全部意义：notifier 侧受理登记、终态销账，
    /// 组内全员终态后经 deliver 投出摘要；notifier 也照常收到全部回调。
    #[test]
    fn ports_share_one_aggregator_and_deliver_the_summary() {
        let delivered: std::sync::Arc<Mutex<Vec<aggregator::AggregateSummary>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let notified = std::sync::Arc::new(AtomicUsize::new(0));

        let ports = FederationPorts {
            notifier: Some({
                let notified = notified.clone();
                std::sync::Arc::new(move |_peer, _call, _task, _timeout, _status, _result| {
                    notified.fetch_add(1, Ordering::SeqCst);
                })
            }),
            deliver: Some({
                let delivered = delivered.clone();
                std::sync::Arc::new(move |summary| {
                    delivered.lock().unwrap().push(summary);
                })
            }),
            ..FederationPorts::unlinked()
        };
        assert!(ports.is_linked());

        // 两个远程子代理登记在同一个父 turn 上。
        let sink = aggregator::AggregatorDelegationSink::from_ports(&ports);
        use echo_plugin_subagent::RemoteDelegationSink;
        sink.register_remote_delegation("sess-1", "turn-1", "call-a", "node-a", "任务 A");
        sink.register_remote_delegation("sess-1", "turn-1", "call-b", "node-b", "任务 B");

        // 受理通知（Running）：只上报，不销账、不聚合。
        notify_remote_subagent(
            &ports,
            "node-a",
            "call-a",
            "任务 A",
            Some(5),
            SubagentStatus::Running,
            None,
        );
        assert!(delivered.lock().unwrap().is_empty());

        // 第一个终态：仍不聚合。
        notify_remote_subagent(
            &ports,
            "node-a",
            "call-a",
            "任务 A",
            None,
            SubagentStatus::Completed,
            Some("A 成功".into()),
        );
        assert!(delivered.lock().unwrap().is_empty());

        // 第二个终态：聚合摘要经 deliver 投出（证明两侧看的是同一张分组表）。
        notify_remote_subagent(
            &ports,
            "node-b",
            "call-b",
            "任务 B",
            None,
            SubagentStatus::Failed,
            Some("B 失败".into()),
        );
        let summaries = delivered.lock().unwrap();
        assert_eq!(summaries.len(), 1, "聚合摘要应只投递一次");
        assert_eq!(summaries[0].session_id, "sess-1");
        assert!(
            summaries[0].text.contains("成功 1/2"),
            "{}",
            summaries[0].text
        );
        // notifier 收到全部三次回调（受理 + 两个终态）——聚合不影响对端上报。
        assert_eq!(notified.load(Ordering::SeqCst), 3);
    }
}
