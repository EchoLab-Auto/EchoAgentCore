//! 命令词汇的**稳定路径**（`echo_agent::command::BackendCommand`）。
//!
//! 线契约类型本身在 `echo-protocol`；这里只做 re-export，让既有调用点
//! （`agent/commands.rs`、`agent/mod.rs`）的 `crate::command::BackendCommand`
//! 保持不变。
//!
//! ## 这里曾经有什么（2026-11 删除）
//!
//! 一个 `CommandHandler` trait + `CommandRegistry`（约 90 行）：按注册顺序问
//! 一遍"谁认领这条命令"。它在**全插件化 S3/S5 之后彻底多余**——命令域现在由
//! 插件注册关系承载（`Contribution::Command` + `PluginHost::dispatch_command`），
//! 而宿主自己处理的那 11 条本来就是 `apply_command` 的 match。
//!
//! 删掉它的理由不是"没人用"，是**它会误导**：模块文档写着"加一条命令 = 加一个
//! handler，永远不用改分发器"，而真实的分发器（`Agent::apply_command`）仍然
//! 是一个 match，新命令照样要改它。留着两套"可扩展命令"的说法，下一个人会先
//! 花半小时搞清楚该用哪套。
//!
//! 现在只有一条扩展路径：**写一个插件，声明它认领的命令**。

pub use echo_protocol::BackendCommand;
