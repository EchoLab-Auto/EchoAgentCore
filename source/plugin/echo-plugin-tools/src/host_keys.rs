//! 宿主能力的**服务键**（本插件的三个命令域各自要一份）。
//!
//! ## 为什么键在这里而不是宿主那边
//!
//! 键 = 「我需要什么」的声明，属于**需求方**。宿主实现这些 trait、按同一批
//! 键注册（它依赖本 crate，方向天然正确）。于是组合根里可以写
//! `echo_plugin_tools::plugin(version)` —— 不带任何 `Arc<dyn …>` 参数，
//! 接线全部发生在服务表里（`MountContext::require`）。
//!
//! ## 一个插件三个键，而不是一个"上帝键"
//!
//! shell / instance / tools 是**三个独立的能力**（宿主侧也确实是三个适配器）。
//! 合成一个键会让"我只想要 shell"的地方被迫依赖另外两个的形状。

use std::sync::Arc;

use echo_context::ServiceKey;

use crate::instance_commands::InstanceCommandHost;
use crate::shell_commands::ShellCommandHost;
use crate::tool_commands::ToolsCommandHost;

/// Shell 命令域（会话 / 执行 / 停止 + `shell_*` 工具）。
pub static SHELL_HOST: ServiceKey<Arc<dyn ShellCommandHost>> =
    ServiceKey::new("echo-agent.host.tools.shell");

/// 插件实例命令域（启停 / 状态 / 通用管理方法）。
pub static INSTANCE_HOST: ServiceKey<Arc<dyn InstanceCommandHost>> =
    ServiceKey::new("echo-agent.host.tools.instance");

/// 工具管理面（清单 / 启停 + 持久化）。
pub static TOOLS_HOST: ServiceKey<Arc<dyn ToolsCommandHost>> =
    ServiceKey::new("echo-agent.host.tools.registry");
