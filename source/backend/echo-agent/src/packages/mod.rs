//! 仍**内联在 agent 框架内**的「包」（插件）实现聚合目录。
//!
//! **第一层分类 = 包归属**：每个子目录对应一个插件 id
//! （`crate::plugins::*_PLUGIN_ID`），实现不散落在 `tool/builtin`、
//! `skill/`、`agent/qq_commands` 等多处。
//!
//! | 目录 | 插件/包 id | 内容 |
//! |---|---|---|
//! | [`tools_builtin`] | `echo-agent.tools.builtin` | 内置工具（计算/搜索/清单/编码/Shell/插件实例管理） |
//! | [`instance_ops`] | `echo-agent.instance.qq` | 插件实例命令域（通用 + legacy 兼容层）+ 入站 hook 桥 |
//! | [`federation`] | `echo-agent.federation.<peer>` | 远程代理工具 + Invoke 调度/裁决 + 工作区跨机查询适配器 |
//!
//! **已外迁的包不再在此**（解耦计划 Phase 3，见
//! `document/design/decoupling-plan.md`）：
//!
//! | 插件 id | 现所在 crate |
//! |---|---|
//! | `echo-agent.provider.llm` | `echo-plugin-provider-llm` |
//! | `echo-agent.skills.dir` | `echo-plugin-skills` |
//! | `echo-agent.subagent` | `echo-plugin-subagent` |
//! | `echo-agent.workspace` | `echo-plugin-workspace`（命令域接线留在 `agent/workspace_commands.rs`） |
//! | `echo-agent.management.panel` | `echo-plugin-management` |
//! | `echo-agent.loop.{single,parallel}` | `echo-plugin-loop` |
//!
//! 工具子系统的**跨包机制**（`ToolRegistry`）亦已外迁到 `echo-tool`；
//! 本目录不再有 `tool/` 子目录。
//!
//! 本模块**物理私有**（`mod packages`）：对外公开路径保持重组前原样
//! （`echo_agent::tool::…`、`echo_agent::workspace::…` 等经 `lib.rs` 的兼容
//! re-export 指向上表的新 crate），目录只服务"人看代码"的区分度，
//! 不引入第二套公开 API。
//!
//! 不随包分组的框架基础设施仍留在 `src/` 顶层：`agent/`（循环/命令/边界/
//! 提示词/压缩）、`session.rs`（事件溯源 trunk）、`timeline.rs`（时间线投影）、
//! `shell.rs`（Shell 会话子系统，面板命令直用）、`bridge/command/event`
//! （线协议 re-export）、`plugins.rs`（插件机制）、`llm.rs`（provider 接缝的
//! 宿主 glue）、`shims_tool.rs`（工具机制的兼容路径）、`config.rs`、
//! `agent_manager.rs`、`input_marker.rs`（结构化输入标记）。

pub mod federation;
pub mod instance_ops;
pub mod tools_builtin;
