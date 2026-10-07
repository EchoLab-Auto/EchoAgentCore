//! # echo-plugin-host — 插件宿主（P0 骨架）
//!
//! 实现总览见 `document/decoupling-plan.md`（Phase 0 / Phase 2）。宿主提供：
//! - [`transport`]：运输层抽象与实现（inproc / subprocess stdio / [dylib / wasm]）；
//! - `supervisor`（Phase 0 收尾提交填充）：插件生命周期监督
//!   （握手 / 贡献注册 / 调用 / 取消 / 排空 / 崩溃重启）。
//!
//! 协议契约（冻结）见 [`echo_plugin_api`]。

pub use echo_plugin_api as api;

pub mod transport;
