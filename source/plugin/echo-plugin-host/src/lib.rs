//! # echo-plugin-host — 插件宿主
//!
//! 实现总览见 `document/decoupling-plan.md`（Phase 0 / Phase 2）。宿主提供：
//! - [`transport`]：运输层抽象（[`transport::Transport`] / [`transport::PluginConnection`]）；
//! - [`inproc`]：in-proc 参考实现 [`inproc::InprocTransport`]（类型直连，零序列化）；
//! - [`supervisor`]：插件生命周期监督 [`supervisor::PluginSupervisor`]
//!   （握手 / 贡献注册 / 调用 / 取消 / 排空 / 崩溃重启）。
//!
//! 协议契约（冻结）见 [`echo_plugin_api`]；一致性用例见 `tests/conformance.rs`
//! （同一套用例按 transport 参数化，新增运输层应全量复跑）。

pub use echo_plugin_api as api;

pub mod inproc;
pub mod supervisor;
pub mod transport;

pub use inproc::{InprocIo, InprocPlugin, InprocTransport};
pub use supervisor::{PluginHandle, PluginState, PluginSupervisor, RestartPolicy};
pub use transport::{PluginConnection, PluginSpec, Transport, TransportError};
