//! # echo-plugin-tools — 内置工具集（`echo-agent.tools.builtin` 的实现）
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
//! | 插件实例管理 | `instance_*` | 依赖 `echo-defs` 的 `InstanceRegistry`（平台中立） |
//! | 编码 | `read_file` / `write_file` / `edit_file` / `list_files` / `search_code` / `run_command` | 工作区根内沙箱；`node://` 前缀经注入的 [`RemoteInvoker`] 转发 |
//! | Shell | `shell_*` | 依赖 `echo-shell` 的会话管理器（进程组 + 哨兵分帧） |
//!
//! ## 接缝（为什么需要它）
//!
//! 三个外部能力经**注入**而非全局获取——这是本 crate 能脱离业务 crate 的前提：
//!
//! - [`RemoteInvoker`]：`node://` 远程文件/命令调用（联邦子系统实现）；
//! - `echo_shell`：Shell 会话管理器与事件发射（宿主装配时设置）；
//! - 工作区根：构造时传入 `PathBuf`（不做目录发现）。
//!
//! 未注入时的行为与注入后一致地**可预测**：远程前缀报「联邦未接线」、
//! Shell 工具报「manager unavailable」——降级是显式的，不是静默换实现。
//!
//! ## 依赖纪律
//!
//! 只依赖 `echo-defs`（词汇）/ `echo-tool`（工具机制）/ `echo-shell`
//! （Shell 机制）+ 基础 crate；**不依赖 `echo-agent`**，也不依赖组合根。
//! `scripts/deps-policy.txt` 与 `plugin_dependency_direction.rs` 双门禁守着。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use echo_tool::ToolRegistry;

pub mod calculator;
pub mod checklist;
pub mod coding;
pub mod instance;
pub mod shell_tools;
pub mod websearch;

pub use calculator::CalculatorTool;
pub use checklist::ChecklistTool;
pub use coding::{register_coding_tools, RemoteAwareTool};
pub use instance::register_instance_tools;
pub use shell_tools::{register_shell_tools, ShellExecTool, ShellStartTool, ShellStopTool};
pub use websearch::WebSearchTool;

/// 远程工具调用出口（`node://<peer>/…` 前缀的分流目标）。
///
/// 签名 `(peer, tool, args) -> Result<text, error>`：与联邦的 Invoke 一一对应。
/// 定义为 type alias（而非 trait）是刻意的——它只是「一个异步函数」，
/// 加一层 trait 只会让组合根多写一次装箱；实现方（联邦）本来就是一个
/// 闭包/函数指针。
pub type RemoteInvoker = Arc<
    dyn Fn(
            String,
            String,
            serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// 注入口：`RemoteInvoker` 由组合根在**注册工具时**传入
/// （[`register_coding_tools`] 的 `invoker` 参数）。
///
/// 为什么不再有全局回查（旧的 `remote_invoker::global()`）：这个 crate 是
/// 工具实现层，不该持有进程级状态；而"联邦可能在工具注册之后才接线"这个
/// 顾虑在现行组合根里不成立——联邦装配（含 invoker 闭包）发生在所有
/// `Agent` 构造之前。此前那份 kernel cell **从未被任何代码 set 过**：
/// `node://` 远程文件/命令分流因此恒报「联邦未接线」。改成参数后，
/// 装配期传进来的就是运行期用的。
/// 命令护��：`run_command` / `shell_exec` 在执行**之前**调用。
///
/// 为什么是注入而不是内置：护栏守的是「宿主自己的服务名」（例如禁止工具
/// 停掉运行中的 Core），那是宿主身份的一部分，不该硬编码进工具实现。
/// 实现方（组合根）提供；未注入 = 不做该检查（工具本身仍是沙箱内的）。
///
/// 返回 `Err` 即拒绝执行，错误文本原样回给模型。
pub type CommandGuard = Arc<dyn Fn(&str) -> Result<(), echo_tool::ToolError> + Send + Sync>;

/// 注册「核心」工具家族：计算 / 检索 / 清单。
///
/// 与 [`register_instance_tools`] / [`register_coding_tools`] /
/// [`register_shell_tools`] 分开，是为了让组合根按可用性选择（例如沙箱环境
/// 里不装 Shell 工具）。包标签由调用方打——本 crate 不认识包 id 的语义。
pub fn register_core_tools(registry: &mut ToolRegistry) {
    registry.register(Arc::new(CalculatorTool));
    registry.register(Arc::new(WebSearchTool));
    registry.register(Arc::new(ChecklistTool::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三个注册函数合起来必须产出互不重名的工具集——重名会被
    /// `ToolRegistry::register` 静默覆盖（见 `echo-tool` 的 `name_conflicts`）。
    #[test]
    fn registered_tool_names_are_unique_across_families() {
        let mut registry = ToolRegistry::new();
        register_core_tools(&mut registry);
        register_coding_tools(&mut registry, std::env::temp_dir(), None, None);
        register_shell_tools(&mut registry, None);
        register_instance_tools(
            &mut registry,
            Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
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
