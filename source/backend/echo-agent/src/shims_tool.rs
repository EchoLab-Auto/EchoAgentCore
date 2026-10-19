//! 工具子系统兼容路径（Phase 3 前置：机制已外迁到 [`echo_tool`]）。
//!
//! `echo_agent::tool::…` 仍是框架内唯一入口——调用点零改动：
//!
//! - `Tool` / `ToolDefinition` / `ToolError` / `ToolResult`：定义层词汇，
//!   经机制 crate 转出；
//! - `ToolRegistry`：运行期机制（可逆注册 + 定义缓存 + 出口脱敏），
//!   实现物理位于 `echo-tool`；
//! - `builtin`：内置工具集（`packages/tools_builtin`），属 echo-agent 自身
//!   的包实现，**不随机制外迁**。

pub use crate::packages::tools_builtin as builtin;
pub use echo_tool::{Tool, ToolDefinition, ToolError, ToolRegistry, ToolResult};
