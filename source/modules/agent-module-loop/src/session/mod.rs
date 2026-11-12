//! 会话子系统 — **已外置为模块 crate `agent-module-session`**（解耦计划 S6）。
//!
//! 本模块只保留一层**兼容转发**：`crate::session::X` 与 `echo_agent::TrunkStore`
//! 的既有引用不必全线改名。实现、测试与设计文档都在模块里。
//!
//! ## 为什么不干脆全仓重命名为 `agent_module_session::...`
//!
//! `Agent::trunk` 与它周围的 58 处调用点是**宿主的用法**（谁在什么时机读写
//! 会话），不是会话实现的一部分。保留转发层让两件事能各自演进：换掉 trunk
//! 的实现不动宿主的用法；调宿主的编排不动事件溯源的纪律。
//!
//! ## 模块归属
//!
//! | 关注点 | 现在在哪 |
//! |---|---|
//! | 事件词汇（`SessionEvent` / `EventLog` / 投影） | `echo-session`（机制层） |
//! | trunk 实例、持久化、归档、压缩编排、历史回填 | `agent-module-session`（本转发目标的模块） |
//! | 谁在什么时机用 trunk | `echo-agent`（宿主，即本文件） |

pub use agent_module_session::store::*;
