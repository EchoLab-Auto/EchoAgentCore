//! # agent-module-tools — 内置工具集（`echo-agent.tools.builtin` 的实现）
//!
//! 模型可直接调用的平台无关工具，原 `echo-agent::packages::tools_builtin`
//! 的主体（解耦计划 Phase 3 之后的延续工作：把仍内联在业务 crate 里的
//! 包实现外迁）。
//!
//! ## 内容
//!
//! | 家族 | 工具 | 归属说明 |
//! |---|---|---|
//! | 计算 / 检索 | `calculator` · `web_search` | 纯函数，无外部状态 |
//! | 清单 | `checklist` | 按会话隔离（`Tool::execute` 的会话键） |
//! | 模块实例管理 | `instance_*` | 依赖 `echo-defs` 的 `InstanceRegistry`（平台中立） |
//! | 编码 | `read_file` / `write_file` / `edit_file` / `list_files` / `search_code` / `run_command` | 工作区根内沙箱；`node://` 前缀经注入的 [`RemoteInvoker`] 转发 |
//! | Shell | `shell_*` | 依赖 `agent-module-shell` 的会话管理器（进程组 + 哨兵分帧） |
//!
//! ## 接缝（为什么需要它）
//!
//! 三个外部能力经**注入**而非全局获取——这是本 crate 能脱离业务 crate 的前提：
//!
//! - [`RemoteInvoker`]：`node://` 远程文件/命令调用（联邦子系统实现）；
//! - `agent_module_shell`：Shell 会话管理器与事件发射（宿主装配时设置）；
//! - 工作区根：构造时传入 `PathBuf`（不做目录发现）。
//!
//! 未注入时的行为与注入后一致地**可预测**：远程前缀报「联邦未接线」、
//! Shell 工具报「manager unavailable」——降级是显式的，不是静默换实现。
//!
//! ## 依赖纪律
//!
//! 只依赖 `echo-defs`（词汇）/ `agent-module-tools`（工具机制）/ `agent-module-shell`
//! （Shell 机制）+ 基础 crate；**不依赖 `echo-agent`**，也不依赖组合根。
//! `scripts/deps-policy.txt` 与 `module_dependency_direction.rs` 双门禁守着。

use std::sync::Arc;

pub mod calculator;
pub mod checklist;
pub mod coding;
pub use echo_defs::execute;
pub mod host_keys;
pub mod instance;
pub mod instance_bridge;
pub mod instance_commands;
pub mod instance_ops;
pub mod list_api_profiles;
pub mod module;
pub use echo_defs::registry;
pub mod session;
pub mod tool_commands;
pub mod websearch;

pub use calculator::CalculatorTool;
pub use checklist::ChecklistTool;
pub use coding::{register_coding_tools, RemoteAwareTool};
pub use instance::register_instance_tools;
pub use instance_commands::{
    InstanceCommandHandler, InstanceCommandHost, COMMANDS as INSTANCE_COMMANDS,
};
pub use instance_ops::{
    broadcast_to_same_platform, invoke_method, resolve_instance, InstanceOpRunner,
};
pub use list_api_profiles::ListApiProfilesTool;
pub use module::{BuiltinToolProvider, BuiltinTools, ToolsHostCtx, MODULE_ID, SERVICE_KEY};
pub use tool_commands::{ToolsCommandHandler, ToolsCommandHost, COMMANDS as TOOL_COMMANDS};
pub use websearch::WebSearchTool;

/// 远程工具调用出口（`node://<peer>/…` 前缀的分流目标）。
///
/// **类型定义在定义层**（`echo_defs::remote_tool::RemoteInvoker`，2026-11 S12）：
/// 声明方是本 crate（`node://` 的消费方），实现方是联邦子系统。此前类型住在
/// 这里，于是联邦模块不得不 `use crate::RemoteInvoker`——两个互不
/// 相干的模块因此结了 Cargo 依赖。
pub use echo_defs::remote_tool::RemoteInvoker;

/// 注入口：`RemoteInvoker` 由组合根在**注册工具时**传入
/// （[`register_coding_tools`] 的 `invoker` 参数）。
///
/// 为什么不再有全局回查（旧的 `remote_invoker::global()`）：这个 crate 是
/// 工具实现层，不该持有进程级状态；而"联邦可能在工具注册之后才接线"这个
/// 顾虑在现行组合根里不成立——联邦装配（含 invoker 闭包）发生在所有
/// `Agent` 构造之前。此前那份 kernel cell **从未被任何代码 set 过**：
/// `node://` 远程文件/命令分流因此恒报「联邦未接线」。改成参数后，
/// 装配期传进来的就是运行期用的。
/// 命令护栏（**类型定义在机制层** `agent_module_shell::CommandGuard`，2026-11）。
///
/// 本 crate 的 `bash`（一次性）与 `agent-module-shell` 的 `bash-exec`（会话内）
/// 都要在执行前问同一句"这条命令能不能跑"，而两个模块互不认识（S12 零依赖）。
/// 于是类型与默认实现（[`agent_module_shell::self_stop_guard`]）住在 shell 机制层，
/// 两边各构造一份——一份实现，零跨模块边。
pub type CommandGuard = std::sync::Arc<dyn Fn(&str) -> Result<(), ToolError> + Send + Sync>;

/// 注册「核心」工具家族：计算 / 检索 / 清单。
///
/// 与 [`register_instance_tools`] / [`register_coding_tools`] /
/// [`register_shell_tools`] 分开，是为了让组合根按可用性选择（例如沙箱环境
/// 里不装 Shell 工具）。包标签由调用方打——本 crate 不认识包 id 的语义。
pub fn register_core_tools(registry: &mut ToolRegistry) {
    registry.register(Arc::new(CalculatorTool));
    // HTTP 客户端（连接池）在此构造一次、注入工具：仍是全进程复用一份，
    // 但不再藏进进程级全局槽（2026-10 技术债清理）。
    registry.register(Arc::new(
        WebSearchTool::new(WebSearchTool::default_client()),
    ));
    registry.register(Arc::new(ChecklistTool::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个注册函数合起来必须产出互不重名的工具集——重名会被
    /// `ToolRegistry::register` 静默覆盖（见 `agent-module-tools` 的 `name_conflicts`）。
    ///
    /// `bash-*` 三个工具在 `agent-module-shell`，那里有自己的同款断言；
    /// 两个模块的工具**跨模块**重名由组合根装配期检查（`name_conflicts`）。
    #[test]
    fn registered_tool_names_are_unique_across_families() {
        let mut registry = ToolRegistry::new();
        register_core_tools(&mut registry);
        register_coding_tools(&mut registry, std::env::temp_dir(), None, None);
        register_instance_tools(
            &mut registry,
            Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
        );
        let conflicts = registry.name_conflicts();
        assert!(conflicts.is_empty(), "工具集内部重名: {conflicts:?}");
    }

    #[test]
    fn core_tool_families_are_registered() {
        let mut registry = ToolRegistry::new();
        register_core_tools(&mut registry);
        let names = registry.names();
        for expected in ["calculator", "web_search", "checklist"] {
            assert!(
                names.iter().any(|n| n == expected),
                "缺少工具 {expected}（实际: {names:?}）"
            );
        }
    }
}

pub use registry::{
    ModuleTool, ModuleToolRegistration, Tool, ToolDefinition, ToolError, ToolRegistry, ToolResult,
};
