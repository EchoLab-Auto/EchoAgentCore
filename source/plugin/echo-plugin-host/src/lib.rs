//! # echo-plugin-host — 插件宿主
//!
//! 实现总览见 `document/decoupling-plan.md`（Phase 0 / Phase 2）。宿主提供：
//! - [`transport`]：运输层抽象（[`transport::Transport`] / [`transport::PluginConnection`]）；
//! - [`inproc`]：in-proc 参考实现 [`inproc::InprocTransport`]（类型直连，零序列化）；
//! - [`stdio`]：子进程实现 [`stdio::StdioTransport`]（4 字节小端长度前缀 + JSON）；
//! - [`supervisor`]：插件生命周期监督 [`supervisor::PluginSupervisor`]
//!   （握手 / 贡献注册 / 调用 / 取消 / 排空 / 崩溃重启）。
//!
//! 协议契约（冻结）见 [`echo_plugin_api`]；一致性用例见 `tests/conformance.rs`
//! （inproc）与 `tests/conformance_stdio.rs`（stdio 子进程）。

pub use echo_plugin_api as api;

pub mod inproc;
pub mod stdio;
pub mod supervisor;
pub mod transport;

pub use inproc::{InprocIo, InprocPlugin, InprocTransport};
pub use stdio::{StdioConnection, StdioTransport};
pub use supervisor::{PluginHandle, PluginState, PluginSupervisor, RestartPolicy};
pub use transport::{PluginConnection, PluginSpec, Transport, TransportError};
