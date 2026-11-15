//! 联邦出口集合（`FederationPorts`）：装配期构造一份，显式下发。
//!
//! ## 它是什么
//!
//! 六个出口 + 一个共享聚合器。它们**天然同源同寿命**（同一次联邦装配产生），
//! 所以是一个值而不是六个参数；`Clone` 廉价、无全局槽位。
//!
//! ## 为什么出口是 `FederationAdminHost` 而不是一个命令回调
//!
//! 搬进独立 crate 时唯一的宿主依赖曾是 `Fn(&Agent, BackendCommand)`；后来
//! 收成 `Fn(BackendCommand) -> future`（去 `Agent` 化）。但那个形状仍然把
//! **整条命令的处理**留在组合根——模块只声明"我处理这几条"，实现却在宿主。
//!
//! 2026-11 S11 改成 [`crate::admin::FederationAdminHost`]：一个只含**原子
//! 状态迁移**的 trait（落 peer / 删 peer / 登记邀请 / 迁移会话 / 取状态）。
//! 规则（token 继承与冲突、URL 形状、邀请地址推导、失败文案）随之搬进模块
//! 的 `admin.rs`，是纯函数、可单测。于是：
//!
//! | 归模块 | 归宿主 |
//! |---|---|
//! | 判什么、按什么顺序判、失败说什么 | 真的改内存与磁盘 |
//!
//! 本 crate 对宿主仍**零依赖**（`scripts/deps-policy.txt` 上有门禁）。
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

pub use crate::SubagentStatus;

/// 联邦管理面的状态迁移出口（`Arc` 包装，装配期放进
/// [`FederationPorts::admin`]）。
pub type FederationAdmin = Arc<dyn crate::admin::FederationAdminHost>;

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
/// 类型与 `agent-module-tools` 共享（工具就注册在那个 crate 里）。
pub type RemoteInvoker = echo_defs::remote_tool::RemoteInvoker;

/// 远程**只读查询**发起函数（跨机工作区用）：
/// `(peer, kind, subject) → Result<payload_json, err>`。
pub type RemoteQuerier = std::sync::Arc<
    dyn Fn(
            String,
            crate::QueryKind,
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
    /// 联邦管理面的**状态迁移**出口（2026-11 S11）。
    ///
    /// 只含原子动作（落 peer / 删 peer / 登记邀请 / 迁移会话 / 取状态），
    /// 规则判定在 [`crate::admin`] 的纯函数里。None = 联邦未接线，命令回
    /// 可读错误（文案由模块给出，不是宿主）。
    ///
    /// 类型**不引用 `Agent`**，也不引用组合根的任何类型：本 crate 对宿主
    /// 零依赖（见模块文档）。
    pub admin: Option<FederationAdmin>,
}

impl Default for FederationPorts {
    fn default() -> Self {
        Self {
            invoker: None,
            querier: None,
            notifier: None,
            deliver: None,
            aggregator: aggregator::RemoteSubagentAggregator::new(),
            admin: None,
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
            .field("admin", &self.admin.is_some())
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
            || self.admin.is_some()
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

/// 联邦沙箱：绝对路径是否落在给定工作区根并集内。
///
/// ## 判定顺序（2026-11 收紧：祖先回退曾是绕过口）
///
/// 1. 非绝对路径 / 含 `..` 组件 → **拒绝**（受限策略：`..` 让"存在的祖先"
///    与"最终落点"可以差出任意目录，任何基于前缀的判定都会失效）；
/// 2. 从根往下**逐段**解析：遇到符号链接就当场 `canonicalize`（解析其真实
///    指向并继续），解析失败（悬空链接 / 链接环）→ **拒绝**；
/// 3. 遇到第一个不存在的组件即停（其后的路径尚不存在，不可能再出现需要
///    解析的链接），用已解析到的真实前缀做判定；
/// 4. 最终真实路径必须落在某个 **canonical 化后的** 工作区根内。
///
/// 旧实现只做「目标或最近已存在祖先的 canonicalize」：当目标是**悬空符号
/// 链接**（`/ws/link -> /etc/newfile`，目标尚不存在）时 canonicalize(目标)
/// 失败、回退到父链 `/ws`（在根内）→ 放行；随后 `write_file` 跟随链接把
/// 内容写到根外（`/etc/newfile`）。第 2 步把这条路封死：链接本身必须先被
/// 解析成功，否则一律不认。
///
/// 语义边界：这是**受限策略**——不跟随无法确认的链接，宁可拒绝；完全互信
/// 的对端（`allow_queries = ["*"]`）在上层根本不调用本函数。
pub fn path_within_roots(raw: &str, roots: &[std::path::PathBuf]) -> bool {
    let target = std::path::Path::new(raw);
    if !target.is_absolute() {
        return false;
    }
    // 1. 拒绝显式上跳：前缀判定对 `..` 无意义（受限策略）。
    if target
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }
    // 2. 逐段解析（链接就地解析；悬空即拒绝）。
    let Some(resolved) = resolve_without_dangling_links(target) else {
        return false;
    };
    // 4. 真实路径必须落在某个真实根内。
    roots.iter().any(|root| {
        root.canonicalize()
            .map(|real_root| resolved.starts_with(&real_root))
            .unwrap_or(false)
    })
}

/// 逐段解析绝对路径：已存在的符号链接就地展开，悬空链接返回 `None`。
///
/// 返回的是"已确认存在的前缀（真实路径）+ 未存在的尾部"——尾部不含 `..`
/// （上层已拒绝），且不会跨越任何链接（不存在的东西不可能是链接）。
fn resolve_without_dangling_links(target: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut resolved = std::path::PathBuf::new();
    for component in target.components() {
        resolved.push(component);
        match std::fs::symlink_metadata(&resolved) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    // 悬空 / 成环：无法确认落点 → 拒绝（受限策略）。
                    resolved = resolved.canonicalize().ok()?;
                }
            }
            // 第一个不存在的组件：其后都是"尚未创建"的路径，
            // 不可能是链接，直接用当前真实前缀判定。
            Err(_) => return Some(resolved),
        }
    }
    Some(resolved)
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
        assert!(ports.admin.is_none());
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
        use echo_defs::subagent::RemoteDelegationSink;
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

    // ── 联邦沙箱（2026-11 收紧）──

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("echo-fed-sandbox-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create temp root");
        root
    }

    /// 根内路径（含尚不存在的子路径）放行。
    #[test]
    fn path_within_roots_allows_existing_and_new_paths_inside() {
        let root = temp_root("inside");
        let existing = root.join("a.txt");
        std::fs::write(&existing, "x").unwrap();
        assert!(path_within_roots(
            existing.to_str().unwrap(),
            &[root.clone()]
        ));
        let fresh = root.join("nested").join("new.txt");
        assert!(path_within_roots(fresh.to_str().unwrap(), &[root.clone()]));
        // 根外绝对路径拒绝。
        assert!(!path_within_roots("/etc/hostname", &[root]));
    }

    /// `..` 显式上跳：前缀判定对它无意义，受限策略直接拒绝。
    #[test]
    fn path_within_roots_rejects_parent_traversal() {
        let root = temp_root("dotdot");
        let escape = root.join("..").join("etc").join("passwd");
        assert!(
            !path_within_roots(escape.to_str().unwrap(), &[root]),
            "`..` 必须被拒绝（否则 write_file 可借道父目录写到根外）"
        );
    }

    /// 非绝对路径不属于联邦沙箱的判定范围（调用方只对绝对路径提问）。
    #[test]
    fn path_within_roots_rejects_relative_paths() {
        assert!(!path_within_roots("relative/file.txt", &[]));
    }

    /// **回归（本次修复的核心）**：悬空符号链接曾绕过祖先回退。
    ///
    /// 旧实现：canonicalize(目标) 失败（悬空）→ 回退到父链 `/root`（在根内）
    /// → 放行；随后 `write_file` 跟随链接写到根外。新实现：逐段遇到链接必须
    /// 能解析，悬空即拒绝。
    #[cfg(unix)]
    #[test]
    fn path_within_roots_rejects_dangling_symlink_escape() {
        use std::os::unix::fs::symlink;
        let root = temp_root("dangling");
        let dangling = root.join("escape");
        let outside = format!("/etc/echo-fed-nonexistent-{}", std::process::id());
        symlink(&outside, &dangling).expect("create dangling symlink");
        assert!(
            !path_within_roots(dangling.to_str().unwrap(), &[root.clone()]),
            "悬空链接指向根外时必须拒绝"
        );
        // 链接 + 子路径同样拒绝（父链回退不得放行）。
        let nested = dangling.join("child.txt");
        assert!(!path_within_roots(nested.to_str().unwrap(), &[root]));
    }

    /// 指向根外的**存在**链接：解析后落在根外 → 拒绝。
    #[cfg(unix)]
    #[test]
    fn path_within_roots_rejects_symlink_to_outside() {
        use std::os::unix::fs::symlink;
        let root = temp_root("symlink-outside");
        let link = root.join("etc-link");
        symlink("/etc", &link).expect("create symlink");
        assert!(!path_within_roots(
            link.join("hostname").to_str().unwrap(),
            &[root.clone()]
        ));
        assert!(!path_within_roots(link.to_str().unwrap(), &[root]));
    }

    /// 根内的链接（指向根内真实目录）：正常放行。
    #[cfg(unix)]
    #[test]
    fn path_within_roots_follows_links_that_stay_inside() {
        use std::os::unix::fs::symlink;
        let root = temp_root("symlink-inside");
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        symlink(&real, &link).expect("create symlink");
        assert!(path_within_roots(
            link.join("new.txt").to_str().unwrap(),
            &[root]
        ));
    }
}
