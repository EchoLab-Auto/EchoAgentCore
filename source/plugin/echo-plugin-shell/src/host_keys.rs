//! 宿主能力的**服务键**（Shell 命令域解析用）。
//!
//! 键 = 「我需要什么」的声明，属于**需求方**（与 `echo-plugin-tools` 的三个键
//! 同一纪律）。宿主实现 `ShellCommandHost` 并按同一批键注册——它依赖本 crate，
//! 方向天然正确。
//!
//! ## 为什么键名从 `echo-agent.host.tools.shell` 改了
//!
//! 2026-11 之前 shell 只是"内置工具集"里的一个工具族，所以键挂在
//! `tools.` 命名空间下。搬成独立插件后那个前缀会误导：本插件不提供任何
//! `tools.*` 能力。改成 `echo-agent.host.shell`——**同一份宿主实现，只换
//! 归属声明**。
//!
//! 改键要两边同改（这里 + `core/host_services.rs`）；漏改的表现是
//! `MissingService` 启动失败并列出当时可用的键，好查。

use std::sync::Arc;

use echo_context::ServiceKey;

use crate::commands::ShellCommandHost;

/// Shell 会话命令域（会话列表 / 起 / 执行 / 停）。
pub static SHELL_HOST: ServiceKey<Arc<dyn ShellCommandHost>> =
    ServiceKey::new("echo-agent.host.shell");
