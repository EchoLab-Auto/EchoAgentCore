//! 各「包」（插件）在框架内的实现聚合目录（2026-09-28 重组）。
//!
//! 物理目录 ↔ 插件 id（`crate::plugins::*_PLUGIN_ID`）的对应关系：
//!
//! | 目录 | 插件/包 id | 内容 |
//! |---|---|---|
//! | [`tools_builtin`] | `echo-agent.tools.builtin` | 内置工具（计算/搜索/清单/编码/Shell/适配器管理） |
//! | [`skills_dir`] | `echo-agent.skills.dir` | `SKILL.md` 注册表 + 热重载 + Git 来源安装 |
//! | [`adapter_qq`] | `echo-agent.adapter.qq` | QQ 入站 hook 桥 + QQ 命令域 |
//! | [`workspace`] | `echo-agent.workspace` | 工作区会话存储/工具 + 命令域 |
//! | [`subagent`] | `echo-agent.subagent` | 异步子任务委派 |
//! | [`provider_llm`] | `echo-agent.provider.llm` | provider 工厂（名义挂载，重启生效） |
//!
//! 本模块**私有**（`mod packages`）：对外公开路径保持重组前原样
//! （`echo_agent::subagent::…` 等，见 `lib.rs` 的兼容 re-export 群），
//! 目录只服务"人看代码"的区分度，不引入第二套公开 API。
//!
//! 其余包不在本 crate：`loop.{single,parallel}` 的实现是 echo-loop crate
//! （本 crate 只做驱动接线，见 `agent/mod.rs` 的 `process_via_echo_loop`）；
//! `management.panel` 的实现是 echo-protocol（线协议）+ echo-agent-core
//! （本 crate 只 re-export 协议类型）。
//!
//! 不随包分组的框架基础设施留在 `src/` 顶层：`agent/`（循环/命令/边界/
//! 提示词/压缩）、`tool/mod.rs`（工具注册表）、`session.rs`（事件溯源 trunk）、
//! `timeline.rs`（时间线投影）、`shell.rs`（Shell 会话子系统，面板命令直用）、
//! `bridge/command/event`（线协议 re-export）、`plugins.rs`（插件机制）、
//! `config.rs`、`agent_manager.rs`、`input_marker.rs`（结构化输入标记）。

pub mod adapter_qq;
pub mod provider_llm;
pub mod skills_dir;
pub mod subagent;
pub mod tools_builtin;
pub mod workspace;
