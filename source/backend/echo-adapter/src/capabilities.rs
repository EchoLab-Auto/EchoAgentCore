//! 适配器管理能力扩展（capability extension）。
//!
//! 定义在 `echo-defs`（定义层）；本模块 re-export 保持
//! `echo_adapter::capabilities::AdapterManagement`（及 crate 根
//! `echo_adapter::AdapterManagement`）路径兼容。
//!
//! `Adapter` trait 只保留**平台中立**的最小面：生命周期（start/stop/
//! remove_instance）、收发（send_message/subscribe）、状态（status_info）；
//! 平台管理能力经本模块的 [`AdapterManagement`] 暴露——**唯一的能力扩展
//! 形态**（2026-10 适配器管理通道：原逐平台专属 trait 已删除，见
//! `document/design/adapter-plugin.md` §P0 W3.5）。

pub use echo_defs::adapter::AdapterManagement;
