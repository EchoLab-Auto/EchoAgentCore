//! # echo-plugin-sdk — 插件侧运行时
//!
//! Phase 3 试点产物（见 `document/decoupling-plan.md`）：让插件二进制
//! **零样板**实现 [`echo_plugin_api`] 协议——实现 [`PluginHandler`] 后调用
//! [`serve_stdio`]，SDK 负责：
//!
//! - **帧读写**：4 字节小端 `u32` 长度前缀 + JSON（与宿主
//!   `echo-plugin-host::stdio` 同一编码）；单帧上限 [`MAX_FRAME_BYTES`]，
//!   读侧以 `read_exact` 容忍分段到达；
//! - **握手**：`Hello` → 校验协议名 / 版本（[`echo_plugin_api::compatible`]）→
//!   `Welcome` + `Register`（[`PluginHandler::contributions`]）+ `Ready`；
//!   不兼容时写 `Failed` 并以错误退出（插件 main 侧转 `exit(1)`）；
//! - **调用**：每个 `Invoke` 并发处理（各 spawn 一个任务），结果经内部 mpsc
//!   交由**单写者任务**串行写出——帧不会交错；
//! - **控制**：`Cancel` → [`PluginHandler::on_cancel`]；`Drain` →
//!   [`PluginHandler::on_drain`] 后等待在途调用完成并退出（0）；`Dispose` /
//!   stdin EOF → 中止在途任务并退出。
//!
//! 最小插件：
//!
//! ```no_run
//! use echo_plugin_sdk::{
//!     async_trait, serve_stdio, Contribution, InvokeOutcome, InvokeRequest, PluginHandler,
//! };
//!
//! struct MyPlugin;
//!
//! #[async_trait]
//! impl PluginHandler for MyPlugin {
//!     fn contributions(&self) -> Vec<Contribution> {
//!         vec![]
//!     }
//!     async fn invoke(&self, _req: InvokeRequest) -> InvokeOutcome {
//!         InvokeOutcome::Ok { text: "ok".to_string(), images: vec![] }
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     if let Err(e) = serve_stdio(MyPlugin).await {
//!         eprintln!("plugin exited: {e}");
//!         std::process::exit(1);
//!     }
//! }
//! ```
//!
//! 泛型核心 [`serve_io`] / [`serve_io_with`] 供测试 / 内嵌运输复用
//! （如 `tokio::io::duplex`）。
//!
//! 约定：帧只走 stdout，插件自身的日志请走 stderr。

#![warn(missing_docs)]

mod error;
mod frame;
mod serve;
#[cfg(test)]
mod tests;

pub use async_trait::async_trait;
pub use echo_plugin_api;
pub use echo_plugin_api::{
    capabilities, Contribution, InvokeContext, InvokeOutcome, ToolContribution,
};
pub use frame::MAX_FRAME_BYTES;
pub use serde;
pub use serde_json;
pub use serve::{serve_io, serve_io_with, serve_stdio, serve_stdio_with};

use serde_json::Value;

/// 插件处理器：插件作者实现本 trait，[`serve_stdio`] 完成全部协议工作。
#[async_trait::async_trait]
pub trait PluginHandler: Send + Sync + 'static {
    /// 要注册的贡献集合（Hello 后经 `Register` 全量上报）。
    fn contributions(&self) -> Vec<Contribution>;

    /// 处理一次调用。
    ///
    /// SDK 已校验贡献名在 [`contributions`](Self::contributions) 之内；返回
    /// [`InvokeOutcome`] 即终结该 `call_id`（结果原样回发宿主）。
    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome;

    /// `Cancel` 通知（尽力送达；对端可能已完成）。缺省空实现。
    async fn on_cancel(&self, _call_id: &str) {}

    /// `Drain` 排空通知（随后进程退出；`deadline_ms` 为宿主给定的排空期限）。
    /// 缺省空实现。
    async fn on_drain(&self, _deadline_ms: u64) {}
}

/// 一次调用的请求（`Invoke` 消息的插件侧视图）。
#[derive(Debug, Clone, PartialEq)]
pub struct InvokeRequest {
    /// 调用 id（回发结果时原样带回）。
    pub call_id: String,
    /// 目标贡献名（工具名）。
    pub contribution: String,
    /// 调用上下文（会话归属等）。
    pub ctx: InvokeContext,
    /// 调用参数（JSON Schema 由贡献注册时声明）。
    pub payload: Value,
}

/// 插件自报 id：`ECHO_PLUGIN_ID`（非空时）否则 `"stdio-plugin"`。
pub fn plugin_id() -> String {
    match std::env::var("ECHO_PLUGIN_ID") {
        Ok(id) if !id.trim().is_empty() => id,
        _ => "stdio-plugin".to_string(),
    }
}

/// 服务选项（[`serve_stdio_with`] / [`serve_io_with`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeOptions {
    /// `Welcome` 自报插件 id；缺省 [`plugin_id()`]。
    pub plugin_id: String,
    /// `Welcome` 能力列表；缺省 `["tools", "cancel"]`（双方都声明才可用）。
    pub capabilities: Vec<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            plugin_id: plugin_id(),
            capabilities: vec![
                capabilities::TOOLS.to_string(),
                capabilities::CANCEL.to_string(),
            ],
        }
    }
}
