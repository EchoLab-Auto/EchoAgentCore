//! 一条待下发的合并事件（wire 载体：事件 + 条目身份）。
//!
//! 合批**策略**（窗口、保序、上限）在 `agent-module-management::protocol::delta_batch`，
//! 这里只有被下发的形状。

use crate::event::BackendEvent;

/// 一条待下发的合并事件。
#[derive(Debug, Clone)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
pub struct Emit {
    pub event: BackendEvent,
    pub entry: Option<String>,
}
