//! 按会话派生**显示历史**的**宿主入口**（策略在
//! [`echo_plugin_management::history`]）。
//!
//! ## 搬家后的分工
//!
//! | 策略（插件里） | 宿主（本文件） |
//! |---|---|
//! | 事件 → 时间线条目的映射规则（user / timer / reasoning / tool / backend / compaction） | 事件日志从哪来（`TrunkStore::event_log`） |
//! | 工具结果如何就地补丁到调用条目 | — |
//! | `limit` 取最近的若干条 + 截断标记 | — |
//! | 合并水位线（哪些 kind 计入 `max_seq`） | — |
//!
//! 为什么它能搬：派生是**纯函数**（事件序列进、时间线条目出）。此前它挂在
//! `impl TrunkStore` 上只因为要读日志——现在它吃 `&[SessionEvent]`，于是与
//! 实时投影（[`echo_plugin_management::timeline`]）同处一个 crate，共用同一
//! 批展示规则（工具概要截断 / 敏感键隐藏 / 定时信封解析 / 单行折叠）。
//!
//! 详细口径（推理为什么纳入派生、水位线为什么排除推理）见插件模块文档——
//! 那些是**策略**，归策略所在的地方。

use crate::event::TimelineMessage;
use crate::session::TrunkStore;

pub use echo_plugin_management::history::history_watermark;

impl TrunkStore {
    /// 从事件日志为单个会话派生显示历史（最近 `limit` 条，时间升序）。
    ///
    /// 返回 `(messages, total, truncated)`。规则全在插件里，这里只把日志
    /// 交出去——**一行转发**。
    pub fn display_history_for_session(
        &self,
        session_id: &str,
        limit: usize,
    ) -> (Vec<TimelineMessage>, usize, bool) {
        echo_plugin_management::history::display_history_for_session(
            &self.event_log(),
            session_id,
            limit,
        )
    }
}
