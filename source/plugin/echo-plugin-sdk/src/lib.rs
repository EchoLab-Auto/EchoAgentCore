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
//!   stdin EOF → 中止在途任务并退出；
//! - **回呼**（P2）：`Ready` 后经 [`PluginHandler::on_ready`] 交付
//!   [`HostClient`]，插件可随时向宿主注册的服务发起 `HostCall`
//!   （默认 30s 超时；连接断开时在途调用以 [`HostCallError::Closed`] 结束）。
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
//!         InvokeOutcome::ok("ok")
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
pub use serde_json;
pub use serve::{
    serve_io, serve_io_with, serve_stdio, serve_stdio_with, HostCallError, HostClient,
    DEFAULT_HOST_CALL_TIMEOUT,
};

/// 适配器插件（P1.2，2026-10 适配器插件化）：[`PluginHandler`] 的适配器
/// 形态——注册 `Contribution::Adapter`（宿主据此把 `RemoteAdapter` 注册进
/// `AdapterRegistry`），`Invoke("<name>:<op>")` 寻址到 [`AdapterPlugin::invoke_op`]
/// 的 method 路由（实例名前缀由 SDK 剥离）。作者实现本 trait 后仍交给
/// [`serve_stdio`]——协议握手 / 帧 / 回呼全部由 SDK 承担。
#[async_trait::async_trait]
pub trait AdapterPlugin: Send + Sync + 'static {
    /// 接收宿主下发配置（`Hello.config`），先于 [`adapter`](Self::adapter)
    /// 调用；`Err` = 拒绝启动。缺省忽略。
    async fn configure(&self, _config: Value) -> Result<(), String> {
        Ok(())
    }

    /// 适配器贡献（`Contribution::Adapter`；实例名 / 平台 / 能力位 / UI 描述符）。
    /// 在 [`configure`](Self::configure) 之后调用。
    fn adapter(&self) -> echo_plugin_api::AdapterContribution;

    /// 处理一次适配器方法调用（`Invoke("<name>:<op>")` 的 `op` 部分；
    /// SDK 已剥离实例名前缀）。
    ///
    /// 生命周期 `start` / `stop`、出站 `send_text` / `send_media`、目录
    /// `get_groups` / `get_friends`、管理 `custom`（`{method, payload}`）
    /// 都走本方法路由；未知方法返回 `Error{code:"unknown_contribution"}`。
    async fn invoke_op(&self, op: &str, payload: Value) -> InvokeOutcome;

    /// 握手完成后交付宿主客户端（调用一次）。插件自存（如
    /// `OnceLock<HostClient>`），后续 `deliver_message` / `report_state`
    /// 等回呼经它发起（见 `document/adapter-plugin.md` §插件动作 表）。
    async fn on_ready(&self, _host: HostClient) {}

    /// `Drain` 排空通知（随后进程退出）；缺省空实现。
    async fn on_drain(&self, _deadline_ms: u64) {}
}

/// 把 [`AdapterPlugin`] 包装成 [`PluginHandler`]：注册单个
/// `Contribution::Adapter`，剥离 `"<name>:"` 前缀后路由到
/// [`AdapterPlugin::invoke_op`]。通常经 [`serve_adapter_stdio`] 使用。
pub struct AdapterHandler<A: AdapterPlugin> {
    plugin: A,
    /// 实例名：注册时（`configure` 之后）锁定，寻址按它匹配。
    name: std::sync::OnceLock<String>,
}

impl<A: AdapterPlugin> AdapterHandler<A> {
    /// 包装适配器插件（实例名在注册时取自 [`AdapterPlugin::adapter`]）。
    pub fn new(plugin: A) -> Self {
        Self {
            plugin,
            name: std::sync::OnceLock::new(),
        }
    }

    /// 取内层插件。
    pub fn inner(&self) -> &A {
        &self.plugin
    }
}

#[async_trait::async_trait]
impl<A: AdapterPlugin> PluginHandler for AdapterHandler<A> {
    async fn configure(&self, config: Value) -> Result<(), String> {
        self.plugin.configure(config).await
    }

    fn contributions(&self) -> Vec<Contribution> {
        let adapter = self.plugin.adapter();
        let _ = self.name.set(adapter.name.clone());
        vec![Contribution::Adapter(adapter)]
    }

    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        match serve::split_adapter_op(&req.contribution) {
            Some((name, op)) if self.name.get() == Some(&name) => {
                self.plugin.invoke_op(&op, req.payload).await
            }
            _ => InvokeOutcome::Error {
                code: "unknown_contribution".to_string(),
                message: format!("unknown adapter contribution: {}", req.contribution),
            },
        }
    }

    async fn on_ready(&self, host: HostClient) {
        self.plugin.on_ready(host).await;
    }

    async fn on_drain(&self, deadline_ms: u64) {
        self.plugin.on_drain(deadline_ms).await;
    }
}

/// 以 stdio 服务一个适配器插件（= `serve_stdio_with(AdapterHandler::new(plugin), …)`）。
///
/// `Welcome.capabilities` 自动追加 `"adapter"`（能力位双方声明才可用；
/// 宿主据此识别适配器插件）。
pub async fn serve_adapter_stdio(plugin: impl AdapterPlugin) -> std::io::Result<()> {
    let mut options = ServeOptions::default();
    if !options.capabilities.iter().any(|c| c == capabilities::ADAPTER) {
        options.capabilities.push(capabilities::ADAPTER.to_string());
    }
    serve_stdio_with(AdapterHandler::new(plugin), options).await
}

use serde_json::Value;

/// 插件处理器：插件作者实现本 trait，[`serve_stdio`] 完成全部协议工作。
#[async_trait::async_trait]
pub trait PluginHandler: Send + Sync + 'static {
    /// 接收宿主下发配置（`Hello.config`；`plugins.toml [plugin.config]` 或
    /// 宿主组装的实例分配结果）。在 [`contributions`](Self::contributions)
    /// 之前调用一次；返回 `Err` = 拒绝启动（宿主收到 `Failed{invalid_config}`）。
    /// 缺省忽略配置。
    async fn configure(&self, _config: Value) -> Result<(), String> {
        Ok(())
    }

    /// 要注册的贡献集合（Hello 后经 `Register` 全量上报）。
    fn contributions(&self) -> Vec<Contribution>;

    /// 处理一次调用。
    ///
    /// SDK 已校验贡献名在 [`contributions`](Self::contributions) 之内；返回
    /// [`InvokeOutcome`] 即终结该 `call_id`（结果原样回发宿主）。
    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome;

    /// `Ready` 发出后的初始化完成通知（P2 新增；缺省空实现，老插件零改动）。
    ///
    /// SDK 在 `Welcome` + `Register` + `Ready` 入队后调用一次；插件借此保存
    /// [`HostClient`] 句柄，后续用它向宿主注册的服务发起回呼（`HostCall`）。
    /// 本方法在消息循环开始前被 await——如需在其中立即发起回呼，请自行
    /// spawn 到后台任务（直接 await 会等到读循环运行后才可能收到结果）。
    async fn on_ready(&self, _host: HostClient) {}

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
    /// `Welcome` 自报插件 id。
    ///
    /// **缺省空串 = 回显宿主 Hello 的 `plugin_id`**（实例身份由宿主掌握，
    /// 插件零配置即可）；非空 = 显式覆盖（与宿主不一致会被宿主拒绝，
    /// 该检查用于抓配置错误）。`ECHO_PLUGIN_ID` 环境变量提供 `plugin_id()`
    /// 辅助值，供需要自主身份的插件使用。
    pub plugin_id: String,
    /// `Welcome` 能力列表；缺省 `["tools", "cancel", "host_call"]`（双方都声明才可用）。
    pub capabilities: Vec<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            // 空 = 回显宿主 Hello 的 plugin_id（见字段文档）。
            plugin_id: String::new(),
            capabilities: vec![
                capabilities::TOOLS.to_string(),
                capabilities::CANCEL.to_string(),
                capabilities::HOST_CALL.to_string(),
            ],
        }
    }
}
