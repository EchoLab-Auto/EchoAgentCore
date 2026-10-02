//! 「包」（插件）实现与工具子系统机制的聚合目录（2026-09-28 重组，
//! 2026-09-29 收编 `tool/`）。
//!
//! **第一层分类 = 包归属**：每个子目录对应一个插件 id
//! （`crate::plugins::*_PLUGIN_ID`），实现不再散落在 `tool/builtin`、
//! `skill/`、`agent/qq_commands` 等多处。
//!
//! | 目录 | 插件/包 id | 内容 |
//! |---|---|---|
//! | [`tools_builtin`] | `echo-agent.tools.builtin` | 内置工具（计算/搜索/清单/编码/Shell/适配器管理） |
//! | [`skills_dir`] | `echo-agent.skills.dir` | `SKILL.md` 注册表 + 热重载 + Git 来源安装 |
//! | [`adapter_qq`] | `echo-agent.adapter.qq` | QQ 入站 hook 桥 + QQ 命令域 |
//! | [`workspace`] | `echo-agent.workspace` | 工作区会话存储/工具 + 命令域 |
//! | [`subagent`] | `echo-agent.subagent` | 异步子任务委派 |
//! | [`federation`] | `echo-agent.federation.<peer>` | 远程代理工具 + Invoke 调度/裁决（federation Phase 2） |
//! | [`provider_llm`] | `echo-agent.provider.llm` | provider 工厂（名义挂载，重启生效） |
//!
//! **特例：[`tool`] 不是包**——它是工具子系统的**跨包机制**
//! （`ToolRegistry` 注册表：注册/可逆注销/逐名与按包启停/schema 缓存），
//! 被内置工具集、QQ 工具、workspace、subagent 等所有包共用。放在本目录是
//! 为了让"工具"这一子系统的代码（机制 + 各包实现）只有一处落点，不再有
//! `src/tool` 与 `src/packages/tools_builtin` 两处同名区域。
//!
//! 本模块**物理私有**（`mod packages`）：对外公开路径保持重组前原样
//! （`echo_agent::tool::…`、`echo_agent::subagent::…` 等经 `lib.rs` 的兼容
//! re-export），目录只服务"人看代码"的区分度，不引入第二套公开 API。
//!
//! 其余包不在本 crate：`loop.{single,parallel}` 的实现是 echo-loop crate
//! （本 crate 只做驱动接线，见 `agent/mod.rs` 的 `process_via_echo_loop`）；
//! `management.panel` 的实现是 echo-protocol（线协议）+ echo-agent-core
//! （本 crate 只 re-export 协议类型）。
//!
//! 不随包分组的框架基础设施仍留在 `src/` 顶层：`agent/`（循环/命令/边界/
//! 提示词/压缩）、`session.rs`（事件溯源 trunk）、`timeline.rs`（时间线投影）、
//! `shell.rs`（Shell 会话子系统，面板命令直用）、`bridge/command/event`
//! （线协议 re-export）、`plugins.rs`（插件机制）、`config.rs`、
//! `agent_manager.rs`、`input_marker.rs`（结构化输入标记）。

pub mod adapter_qq;
pub mod federation;
pub mod provider_llm;
pub mod skills_dir;
pub mod subagent;
pub mod tool;
pub mod tools_builtin;
pub mod workspace;
