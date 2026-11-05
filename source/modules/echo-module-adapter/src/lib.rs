//! # echo-module-adapter — 远程贡献 → 领域 trait 的适配层
//!
//! 宿主（[`echo_module_host`]）只认协议：它能启动模块、调用贡献、收事件，
//! 但**不认识 agent 领域的 `Tool` / `ModuleInstance` trait**。这一层把「协议
//! 里的贡献」翻译成「框架里的 trait 对象」：
//!
//! - [`RemoteTool`]：`ToolContribution` → `echo_defs::tool::Tool`
//!   （注册进 `ToolRegistry` 即可被模型调用，与内置工具无差别）；
//! - [`RemoteInstance`] / [`RemoteInstanceManagement`]：
//!   `InstanceContribution` → `echo_defs::module_instance::{ModuleInstance, InstanceManagement}`
//!   （能力位 fail-closed，动作经 `Invoke("<name>:<op>")`）；
//! - [`CoreApiService`]：模块 → 宿主的 `core_api` 回呼服务
//!   （入站消息入队 / 连接态与 UI 状态上报）。
//!
//! ## 为什么单独成 crate
//!
//! 这一层是「模块机制的复用边界」：`echo-module-host` 不带 `echo-defs` 依赖，
//! 才能作为一个**领域中立**的 Rust 模块机制被别的项目拿去用；而适配器认识
//! 领域 trait，属于 agent 框架侧。两者分开后，依赖方向由
//! `scripts/deps-policy.txt` 与 `source/modules/core/tests/module_dependency_direction.rs`
//! 双门禁守着，不会有人把它们再合回去。
//!
//! 依赖纪律：只依赖 `echo-module-host`（机制）+ `echo-defs`（领域词汇）+
//! `echo-module-api`（贡献类型）——不依赖 `echo-agent`，也不依赖组合根。

#![warn(missing_docs)]

pub mod contract;
pub mod instance;
pub mod tool;

pub use instance::{
    message_to_incoming, CoreApiService, InstanceStateChange, InstanceUiStateChange,
    RemoteInstance, RemoteInstanceManagement,
};
pub use tool::{ModuleHandleTools, RemoteTool};

// 便捷：贡献类型从协议层转出，适配器的使用者不必同时依赖 api crate。
pub use contract::{IncomingMessage, InstanceCapabilities, InstanceContribution, ToolContribution};
pub use echo_module_host::api::Contribution;

use std::sync::Arc;

use echo_module_host::bootstrap::StartedModules;
use echo_module_host::ModuleHandle;

/// 汇总已启动模块的全部工具贡献（每个工具一个 [`RemoteTool`]）。
///
/// 之所以是自由函数而不是 `StartedModules` 的方法：`StartedModules` 定义在
/// 领域中立的宿主里，不该知道 `RemoteTool` 的存在。
pub fn tools_of(started: &StartedModules) -> Vec<Arc<RemoteTool>> {
    started
        .handles
        .iter()
        .flat_map(|h| h.remote_tools())
        .collect()
}

/// 汇总已启动模块的全部模块实例贡献：`(贡献, 句柄)`。
///
/// 调用方据此为每个实例构造 [`RemoteInstance`] 并注册进 `InstanceRegistry`。
pub fn instance_contributions(
    started: &StartedModules,
) -> Vec<(InstanceContribution, ModuleHandle)> {
    started
        .handles
        .iter()
        .flat_map(|h| {
            h.contributions()
                .into_iter()
                .filter_map(|c| {
                    contract::instance_from_custom(&c).map(|instance| (instance, h.clone()))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_started_modules_have_no_adapters() {
        let started = StartedModules {
            handles: Vec::new(),
            errors: Vec::new(),
        };
        assert!(tools_of(&started).is_empty());
        assert!(instance_contributions(&started).is_empty());
    }

    /// 扩展 trait 必须可用（调用方 `use echo_module_adapter::ModuleHandleTools;` 后
    /// 才能 `.remote_tools()`）——这是跨 crate 边界唯一的注入方式。
    #[test]
    fn module_handle_tools_trait_is_exported() {
        fn assert_exported<T: ModuleHandleTools>() {}
        assert_exported::<ModuleHandle>();
    }
}
