//! 模块实例工具的**通用桥**（兼容路径）。
//!
//! 2026-11 全模块化 S6：643 行搬进 [`echo_module_tools::instance_bridge`]。
//! 它是「模块实例能力」的工具面——工具名 / schema 来自模块实例自己的声明，
//! 合并 / 寻址 / 出站闸门三件事与平台无关。此前住在组合根，只因为工具要
//! 实现 `echo_module_loop::Tool`，而那个 trait 早已在 `echo-module-tools` 里。
//!
//! 保留 `crate::instance_tools::…` 这条路径（`main.rs` 的装配点在用）；
//! 内容全部 re-export。

pub use echo_module_tools::instance_bridge::{register_instance_tools_multi, OutboundGate};
