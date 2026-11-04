//! 远程子代理委派的**接缝词汇**（2026-11 S12）。
//!
//! ## 它是什么
//!
//! 远程子任务的全部终态到达后，需要把「按父 turn 分组的聚合摘要」投递回父
//! 会话——那份状态属联邦子系统（进程级聚合表）。本 trait 只表达**一件事**：
//! 受理一次远程委派时，把「哪个 call 属于哪个 turn / peer」告知登记方。
//!
//! ## 为什么在定义层
//!
//! 声明方是 `echo-plugin-subagent`（受理委派的一方），实现方是
//! `echo-plugin-federation`（聚合表的所有者）。此前 trait 住在 subagent 插件
//! 里，于是联邦插件要 `impl echo_plugin_subagent::RemoteDelegationSink` 才能
//! 接上——聚合功能因此**把两个插件绑成了 Cargo 依赖**。
//!
//! 不注入时是**降级而非错误**：远程委派照常发出、`<subagent_event>` 逐个
//! 回报，只是没有分组聚合摘要——单节点部署根本不需要聚合。

/// 远程委派的聚合登记接缝（宿主注入）。
pub trait RemoteDelegationSink: Send + Sync {
    /// 登记一次远程委派。
    ///
    /// - `session_id`：父会话；
    /// - `parent_branch_id`：父 turn 分支（聚合分组的键）；
    /// - `call_id`：子任务 id；
    /// - `peer`：目标节点名；
    /// - `task`：任务摘要（聚合摘要里展示给模型看的短句）。
    fn register_remote_delegation(
        &self,
        session_id: &str,
        parent_branch_id: &str,
        call_id: &str,
        peer: &str,
        task: &str,
    );
}
