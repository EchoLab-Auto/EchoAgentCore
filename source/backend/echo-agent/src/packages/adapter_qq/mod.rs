//! QQ 适配器的框架侧实现（包 `echo-agent.adapter.qq`）。
//!
//! 适配器本体在 `echo-adapter-qq` crate（反向 WS 服务 + NapCat 客户端）；
//! 本目录是它在 agent 框架内的接缝：
//! - [`bridge`]：入站 hook 桥（`AgentMessageHook`——把 QQ 消息包成结构化
//!   输入并投递给 agent）；
//! - `commands`：QQ 命令域（名单/门控/群与好友列表/登录查询/owner 设置），
//!   由主 `apply_command` 一行委托进来（见 `agent/commands.rs`）。

pub mod bridge;
pub(crate) mod commands;
