//! 按会话派生**显示历史**（事件日志 → 显示时间线形状）。
//!
//! 2026-11 自 `agent-module-management` 迁入：派生规则是**会话的投影**，与
//! trunk 同一归属；留在管理面模块里会让"会话怎么派"与"面板怎么传"共用一次
//! 编译，也触发模块互依赖门禁（`scripts/deps-policy.txt` 第 4 条）。共用的
//! 显示文本工具已下沉到 `agent-module-management::timeline_text`。
//!
//! ## 为什么在这里
//!
//! 显示时间线是每人格一条、上限 1024 条的 FIFO 窗口——长期无活动的会话条目
//! 会被后续会话的活动整体挤出，面板切回该会话时只能显示「暂无消息」。事件
//! 日志（唯一事实来源）按会话分区且完整保留，所以显示历史**按需从事件日志
//! 派生**：实时投影（[`super::timeline`]）负责窗口，本模块负责回填旧会话。
//!
//! 两者是**同一件事的两个方向**，共用同一批展示规则（工具概要截断 / 敏感键
//! 隐藏 / 定时信封解析 / 单行折叠），所以同处一个 crate。此前派生住在宿主
//! `session/history.rs`（611 行），只因为它要读 `TrunkStore` 的事件日志——
//! 现在它吃 `&[SessionEvent]`，宿主一行转发。
//!
//! ## 与实时投影的已知差异（接受）
//!
//! - 归档提示行不在事件日志中 → 派生不含；
//! - `ToolResult` 事件没有 `timed_out` 字段 → 派生条目的 `failed` 只按
//!   `"error:"` 前缀判定（实时投影还有超时维度）。
//!
//! ## 推理**会**派生（2026-10 根治）
//!
//! 此前推理从不落事件日志——活链路的 `AssistantMessage` 恒为
//! `reasoning_content: None`，于是派生结果里没有推理，前端只能靠实时窗口
//! 补，导致「刷新后思考全堆到正文后面」的时序错乱无法根除。
//!
//! 现在每段推理以 [`echo_session::SessionEvent::Reasoning`] 落日志（模型不可
//! 见，投影跳过），派生按到达顺序给出 `kind = "reasoning"` 条目，与工具调用
//! 真实交错。
//!
//! ## ⚠️ 水位线：仍排除推理（保守）
//!
//! 前端用 `SessionHistory.max_seq` 作合并水位线（`seq <= baseSeq` 视为已被
//! 快照覆盖）。历史派生**曾**不含推理，把它计入水位线会让前端把实时推理判成
//! "快照已有"而整条丢弃（「看不到思考 chip」，2026-10 事故）。现在虽然派生
//! 含推理，旧日志里却没有 `Reasoning` 事件——保持排除是安全的；前端另有
//! 「推理永不按 seq 过滤」的独立防线，两处刻意互不依赖。

use std::collections::HashMap;

use agent_module_management::protocol::{TimelineMessage, TimelineSource, TimelineTool};

use crate::store::TrunkStore;
use agent_module_management::protocol::timeline_text::{
    summarize_timeline_value, timeline_timer_task, truncate_one_line,
};

pub fn display_history_for_session(
    events: &[echo_session::SessionEvent],
    session_id: &str,
    limit: usize,
) -> (Vec<TimelineMessage>, usize, bool) {
    let mut messages: Vec<TimelineMessage> = Vec::new();
    // 已发出、等待结果的工具条目：tool_call_id → 条目索引。结果到达时
    // 就地补丁（与实时投影同一配对语义）；id 为空的旧版调用不入索引。
    let mut open: HashMap<String, usize> = HashMap::new();
    // carry-forward 时间游标：backend/compaction 条目自身不带时间，
    // 沿用最近一条带时间事件的时间（与实时投影的"当前时间"同义）。
    let mut last_time: i64 = 0;
    for event in events {
        if event.session() != Some(session_id) {
            continue;
        }
        match event {
            echo_session::SessionEvent::UserMessage(user) => {
                match user
                    .source
                    .as_ref()
                    .map(|source| source.instance_name.as_str())
                {
                    // 内部父上下文 hook，不是会话内容（与实时投影一致）。
                    Some("background") => continue,
                    Some("timer") => {
                        let task = timeline_timer_task(&user.content);
                        let content = match task {
                            Some(task) => {
                                format!("定时任务触发 · {}", truncate_one_line(&task, 120))
                            }
                            None => "定时任务触发".into(),
                        };
                        messages.push(TimelineMessage::system(content, session_id, user.timestamp));
                    }
                    _ => {
                        // 图片规范化（与实时投影一致）：媒体引用（`/media/<id>`）
                        // 原样保留，遗留的内嵌 data URI 落盘后改写为引用。
                        let images: Vec<String> = user
                            .images
                            .iter()
                            .map(|image| echo_defs::media_store::spill_or_keep(image))
                            .collect();
                        let source = user.source.as_ref().map(|source| TimelineSource {
                            instance_name: source.instance_name.clone(),
                            platform: source.platform.clone(),
                            user_id: source.user_id.clone(),
                            user_name: source.user_name.clone(),
                            channel: source.channel.clone(),
                            group_name: source.group_name.clone(),
                            received_at_ms: source.received_at_ms,
                            message_sequence: source.message_sequence,
                        });
                        messages.push(TimelineMessage::user(
                            user.content.clone(),
                            session_id,
                            user.timestamp,
                            source,
                            images,
                        ));
                    }
                }
                // 两类用户事件都带真实时间戳：推进 carry-forward 游标
                // （timer 的系统条目同样算"最近已知时间"）。
                last_time = last_time.max(user.timestamp);
            }
            echo_session::SessionEvent::AssistantMessage(assistant) => {
                // 空正文（纯 tool-call 轮）不落显示条目；tool 条目由
                // ToolCall 事件产生，这里不再展开 `tool_calls` 以免重复。
                if assistant.content.trim().is_empty() {
                    continue;
                }
                // 时间取**回答自身的落盘时刻**（2026-10 显示顺序根治）。
                //
                // 此前一律用 `last_time`（carry-forward），对 backend 条目那是
                // 错的语义：它拿的是"上一条带时间的事件"的时间，通常就是上一个
                // 工具的 `started_at_ms`，可以比回答真实产出时刻早**数十分钟**。
                // 而实时投影（`timeline.rs`）给同一个回答的是 `Utc::now()`。
                // 两侧时间不等 → 面板的 `(role, time)` 认不出是同一条 → 同一条
                // 回答显示两次、其中一份被插到列表靠后位置。
                //
                // 旧日志没有该字段（`timestamp == 0`）→ 退回 carry-forward，
                // 行为与改前一致（那批老数据由前端的内容兜底负责）。
                let time = if assistant.timestamp > 0 {
                    last_time = last_time.max(assistant.timestamp);
                    assistant.timestamp
                } else {
                    last_time
                };
                // 推理不落日志：`reasoning_content` 不派生（见模块注释）。
                messages.push(TimelineMessage::backend(
                    assistant.content.clone(),
                    session_id,
                    time,
                    None,
                ));
            }
            echo_session::SessionEvent::Reasoning(reasoning) => {
                // 推理落独立条目（与实时投影同形状）：按到达顺序与工具
                // 调用真实交错。时间用事件自带的产出时刻；旧数据缺失时
                // 退回 carry-forward（与 backend/compaction 条目同规则）。
                let content = reasoning.content.trim();
                if content.is_empty() {
                    continue;
                }
                let time = if reasoning.timestamp > 0 {
                    last_time = last_time.max(reasoning.timestamp);
                    reasoning.timestamp
                } else {
                    last_time
                };
                messages.push(TimelineMessage {
                    kind: "reasoning".into(),
                    content: content.to_string(),
                    session_id: session_id.to_string(),
                    time,
                    source: None,
                    reasoning: None,
                    tool: None,
                    images: None,
                    archive: None,
                    seq: 0,
                    ord: 0, // 派生条目：非时间线缓冲条目，无需分页锚点
                });
            }
            echo_session::SessionEvent::ToolCall(call) => {
                let started_secs = call
                    .started_at_ms
                    .map(|ms| ms / 1000)
                    .filter(|secs| *secs > 0);
                let time = started_secs.unwrap_or(last_time);
                if let Some(secs) = started_secs {
                    last_time = last_time.max(secs);
                }
                messages.push(TimelineMessage::tool(
                    call.name.clone(),
                    session_id,
                    time,
                    TimelineTool {
                        name: call.name.clone(),
                        input: summarize_timeline_value(&call.arguments, 160),
                        output: None,
                        failed: false,
                        tool_call_id: call.id.clone(),
                        timed_out: false,
                        started_at_ms: call.started_at_ms,
                        elapsed_ms: None,
                    },
                ));
                if !call.id.is_empty() {
                    open.insert(call.id.clone(), messages.len() - 1);
                }
            }
            echo_session::SessionEvent::ToolResult(result) => {
                let output = summarize_timeline_value(&result.result, 200);
                let failed = result.result.trim_start().starts_with("error:");
                match open.remove(&result.tool_call_id) {
                    Some(index) => {
                        // 命中未完成的工具条目：就地补丁（镜像实时投影的
                        // update_timeline_tool）。`timed_out` 无日志依据，
                        // 保持 false。
                        if let Some(tool) = messages
                            .get_mut(index)
                            .and_then(|message| message.tool.as_mut())
                        {
                            tool.output = Some(output);
                            tool.failed = failed;
                            tool.elapsed_ms = result.elapsed_ms;
                        }
                    }
                    None => {
                        // 未命中（旧版日志缺 ToolCall 事件 / 压缩吃掉了调用 /
                        // 重复结果）：追加已完成条目，镜像 update_timeline_tool
                        // 的 fallback。
                        //
                        // 名字用**中性占位**（2026-10 修正）：`ToolResult` 事件
                        // 不带工具名，此前拿 `tool_call_id` 顶上，界面上就出现
                        // 一个叫 `tool_N1QCMouhKj5KR7WuskLdDcYa` 的"工具"——
                        // 既不是工具名也不是给人看的字。`id` 仍原样保留在
                        // `tool_call_id` 里供配对，展示层不需要它。
                        let name = "tool".to_string();
                        messages.push(TimelineMessage::tool(
                            name.clone(),
                            session_id,
                            last_time,
                            TimelineTool {
                                name,
                                input: String::new(),
                                output: Some(output),
                                failed,
                                tool_call_id: result.tool_call_id.clone(),
                                timed_out: false,
                                started_at_ms: None,
                                elapsed_ms: result.elapsed_ms,
                            },
                        ));
                    }
                }
            }
            // 模型往返观测（2026-10-19）：审计用事实，不是对话内容——
            // 显示时间线不为它造条目（与实时投影保持一致：实时也不发）。
            echo_session::SessionEvent::ModelStep(_) => {}
            echo_session::SessionEvent::Compaction(compaction) => {
                // 归档文件名（`CompactionEvent.archive` 是完整路径，这里只留
                // 文件名：线上形状要稳定，且面板只按名字回查，不碰路径）。
                let archive = compaction
                    .archive
                    .as_deref()
                    .and_then(|path| std::path::Path::new(path).file_name())
                    .and_then(|name| name.to_str())
                    .map(str::to_string);
                let mut message = TimelineMessage::system(
                    format!(
                        "上下文已压缩 · 替换 {} 条事件{}",
                        compaction.replaced_count,
                        if archive.is_some() {
                            "（原件已归档，可查看压缩前的历史）"
                        } else {
                            ""
                        }
                    ),
                    session_id,
                    last_time,
                );
                message.archive = archive;
                messages.push(message);
            }
        }
    }
    let total = messages.len();
    let truncated = total > limit;
    if truncated {
        // 取**最后** limit 条（最近的活动），保持时间升序。
        messages.drain(..total - limit);
    }
    (messages, total, truncated)
}

/// 单次展开请求允许解析的归档份数（= 层数）。
///
/// 一份归档 18–23 MB、一层 500–1500 条消息，而压缩链实测可达 6 层
/// （`echo-agent/v` 2026-11 就是 6 层、合计约 125 MB）。一次请求只解析
/// **一层**：用户看到分隔行再点下一层，比"等六份 JSON 全解析完"好得多。
pub const EXPAND_DEPTH_PER_REQUEST: usize = 1;

/// 单次返回的条目硬上限（防病态数据把一帧塞进几十万条）。
///
/// 实测最长链：`echo-agent/v` 2026-11 全展开 6 层 = 4895 条（每层 700–900
/// 条）。所以上限必须高于"最长的真实链"，否则用户点到底反而看到被截断的
/// 视图、还会以为"展开失败"。20000 是安全阀，正常数据永远碰不到。
pub const EXPAND_HARD_CAP: usize = 20000;

/// 「展开压缩前归档」需要的事件来源（由 store 实现：读文件 + 解析 + 切片）。
///
/// 之所以做成 trait：派生规则（本模块）与文件 I/O（`archives` 模块）分开，
/// 本模块的测试可以喂假数据，不必造 20 MB JSON。
pub trait ArchiveEventSource {
    /// 读 `archive` 里 `session_id` 的**前** `count` 条事件。
    ///
    /// 返回 `(events, error)`：`error` 非空 = 读不到（归档被轮转清理 /
    /// 解析失败），此时 `events` 为空。
    fn archive_prefix_events(
        &self,
        archive: &str,
        session_id: &str,
        count: usize,
    ) -> (Vec<echo_session::SessionEvent>, Option<String>);
}

/// 一次展开的上下文。
pub struct ExpandContext<'a> {
    pub source: &'a dyn ArchiveEventSource,
    /// 本次请求还能**解析**几份归档（0 = 只标 `has_older`，不真读）。
    pub depth: usize,
}

/// 派生结果 + 展开状态。
#[derive(Debug, Clone, Default)]
pub struct ExtendedHistory {
    /// 时间升序的条目（展开时含压缩前的部分）。
    pub messages: Vec<TimelineMessage>,
    /// 截断前的总数。
    pub total: usize,
    /// `true` = 因 limit / 硬上限只含尾部一部分。
    pub truncated: bool,
    /// 上面还有更早的历史**没展开**（日志里存在压缩边界且归档还在）——
    /// 面板据此在分隔行上给「展开」，而不是「加载更早」。
    pub has_older: bool,
    /// 上面确实还有历史，但归档已被轮转清理（读不到了）。
    pub older_unavailable: bool,
}

/// 该会话**最近一个**压缩边界：`(归档文件名, 被替换的事件数)`。
///
/// 取最后一条（离尾部最近）：日志形如 `[C_最新, ...保留尾]`，`C_最新` 的
/// 归档里才是"压缩前的最近内容"。更早的压缩边界在那一层的 `events` 里，
/// 展开它之后自然出现。
fn newest_compaction(
    events: &[echo_session::SessionEvent],
    session_id: &str,
) -> Option<(String, usize)> {
    events.iter().rev().find_map(|event| match event {
        echo_session::SessionEvent::Compaction(compaction)
            if compaction.session.as_deref() == Some(session_id) =>
        {
            let archive = compaction
                .archive
                .as_deref()
                .and_then(|path| std::path::Path::new(path).file_name())
                .and_then(|name| name.to_str())
                .map(str::to_string)?;
            // replaced_count = 0 的压缩行没有可展开内容（历史遗留/异常）。
            (compaction.replaced_count > 0).then_some((archive, compaction.replaced_count))
        }
        _ => None,
    })
}

/// 派生显示历史，并**按需把压缩前归档接上去**（2026-11）。
///
/// ## 为什么在这里拼
///
/// 面板要的是"像普通上下文一样"的一条连续消息流：同一滚动区、同一套行模型、
/// 同一个「加载更早」。如果让前端把实时历史与归档回放拼起来，它就得自己
/// 复刻边界规则（事件级 `replaced_count`、多会话分组、递归层），还要处理
/// 重复条目——那是**投影知识**，应该只存在一处。
///
/// ## 边界怎么切（关键）
///
/// 归档是压缩**之前**的全量日志快照，`CompactionEvent.replaced_count` 是
/// **按会话数的事件数**（不是消息数：一次工具调用 + 其结果 = 2 事件 = 1 行）。
/// 所以取该会话事件的**前 `replaced_count` 条**，派生后拼在分隔行之前。
/// 拿消息数去切会切歪。
///
/// ## 一次一层
///
/// `expand.depth` 控制本次能解析几份归档（见 [`EXPAND_DEPTH_PER_REQUEST`]）；
/// 更深的层在结果里保留 Compaction 分隔行，用户再点一次即可（`has_older`）。
pub fn display_history_extended(
    events: &[echo_session::SessionEvent],
    session_id: &str,
    limit: usize,
    expand: Option<&ExpandContext<'_>>,
) -> ExtendedHistory {
    // 先全额派生：limit 的裁剪放到最后做（否则无法判断"还有多少没给"）。
    let (mut messages, _, _) = display_history_for_session(events, session_id, usize::MAX);
    // 未展开时：日志里存在压缩边界 = 上面还有历史可展开。
    let mut has_older = messages.iter().any(|message| message.archive.is_some());
    let mut older_unavailable = false;

    if let Some(context) = expand {
        if context.depth > 0 {
            if let Some((archive, replaced)) = newest_compaction(events, session_id) {
                let (prefix_events, error) =
                    context.source.archive_prefix_events(&archive, session_id, replaced);
                match error {
                    // 归档被轮转清理 / 解析失败：分隔行留着（让用户知道这里
                    // 曾经有历史），但不再提供「继续展开」。
                    Some(_) => {
                        older_unavailable = true;
                        has_older = false;
                    }
                    None => {
                        let inner = display_history_extended(
                            &prefix_events,
                            session_id,
                            usize::MAX,
                            Some(&ExpandContext {
                                source: context.source,
                                depth: context.depth - 1,
                            }),
                        );
                        let mut joined = inner.messages;
                        joined.extend(messages);
                        messages = joined;
                        // 最上面那一层决定"还能不能再展开"。
                        has_older = inner.has_older;
                        older_unavailable = inner.older_unavailable;
                    }
                }
            }
        }
    }

    // 截断：保留**最近**的一部分。
    //
    // 但压缩边界那一行必须留下（`pin`）：它是面板唯一的入口，而它恰好是最老
    // 的那一行——按"保留尾部"的规则第一个被丢掉（实测：某会话历史 895 条、
    // 面板只请求 300 条 → 入口直接消失，用户看不到任何"展开压缩前的历史"）。
    // 语义上也成立：那一行是**窗口顶端的标尺**，不是随窗口漂移的内容。
    let pin = messages
        .iter()
        .find(|message| message.archive.is_some())
        .cloned();
    let total = messages.len();
    let mut truncated = total > EXPAND_HARD_CAP;
    let mut kept = total;
    if truncated {
        kept = EXPAND_HARD_CAP;
        // 触顶说明"更早的没给全"，用户仍可继续展开（层数还能加）。
        has_older = true;
    } else if expand.is_none() && total > limit.max(1) {
        kept = limit.max(1);
        truncated = true;
    }
    if kept < total {
        messages.drain(..total - kept);
        if let Some(pin) = pin {
            // 尾窗里已经有同名边界（= 它本来就在保留范围内）就不重复插。
            let already = messages
                .iter()
                .any(|message| message.archive == pin.archive);
            if !already {
                messages.insert(0, pin);
            }
        }
    }
    ExtendedHistory {
        messages,
        total,
        truncated,
        has_older,
        older_unavailable,
    }
}

/// 会话历史合并水位线：`timeline` 中该会话、**派生会覆盖的 kind** 的最大 seq。
///
/// 前端拿它过滤实时条目（`seq <= 水位线` = 快照已覆盖）。
///
/// 推理**仍排除**（见模块注释）：派生现已包含推理，但旧日志里没有
/// `Reasoning` 事件；计入后那些会话的实时推理会被误判为已覆盖而整条消失。
/// 前端另有"推理永不按 seq 过滤"的独立防线，两处刻意互不依赖。
pub fn history_watermark(
    timeline: impl IntoIterator<Item = TimelineMessage>,
    session_id: &str,
) -> u64 {
    timeline
        .into_iter()
        .filter(|message| message.session_id == session_id)
        .filter(|message| message.kind != "reasoning")
        .map(|message| message.seq)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_session::{SessionEvent, UserMessage};

    fn user_event(session: &str, content: &str, timestamp: i64) -> SessionEvent {
        let mut event = UserMessage {
            content: content.into(),
            timestamp,
            message_sequence: None,
            source: None,
            images: Vec::new(),
            session: Some(session.into()),
        };
        event.timestamp = timestamp;
        SessionEvent::UserMessage(event)
    }

    /// 派生只取目标会话的事件（其它会话的条目不串进来）。
    #[test]
    fn derivation_is_scoped_to_one_session() {
        let events = vec![
            user_event("a", "甲的消息", 10),
            user_event("b", "乙的消息", 20),
            user_event("a", "甲的第二条", 30),
        ];
        let (messages, total, _) = display_history_for_session(&events, "a", 100);
        assert_eq!(total, 2, "只算 a 会话");
        assert!(messages.iter().all(|m| m.session_id == "a"));
        assert_eq!(messages[0].content, "甲的消息");
        assert_eq!(messages[1].content, "甲的第二条");
    }

    /// `limit` 取**最近**的若干条（时间升序返回，truncated 标记为真）。
    #[test]
    fn limit_keeps_the_newest_and_flags_truncation() {
        let events: Vec<SessionEvent> = (0..10)
            .map(|index| user_event("a", &format!("第{index}条"), index as i64))
            .collect();
        let (messages, total, truncated) = display_history_for_session(&events, "a", 3);
        assert_eq!(total, 10);
        assert!(truncated, "超出 limit 必须标记截断");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].content, "第9条", "最新的必须保留");
    }

    /// 后台输入不派生（与实时投影一致：内部父上下文 hook 不是对话内容）。
    #[test]
    fn background_input_is_not_derived() {
        let mut event = UserMessage {
            content: "internal".into(),
            timestamp: 1,
            message_sequence: None,
            source: Some(echo_session::event::MessageSource {
                instance_name: "background".into(),
                platform: "local".into(),
                user_id: "u".into(),
                user_name: "u".into(),
                channel: "dm".into(),
                group_name: None,
                received_at_ms: 0,
                message_sequence: 1,
            }),
            images: Vec::new(),
            session: Some("a".into()),
        };
        event.timestamp = 1;
        let events = vec![SessionEvent::UserMessage(event)];
        let (messages, _, _) = display_history_for_session(&events, "a", 100);
        assert!(messages.is_empty(), "内部 hook 不该进显示历史");
    }

    /// 水位线排除推理（旧日志没有 Reasoning 事件的保守口径）。
    #[test]
    fn watermark_excludes_reasoning_entries() {
        let messages = vec![
            TimelineMessage {
                kind: "user".into(),
                content: "x".into(),
                session_id: "s".into(),
                time: 0,
                source: None,
                reasoning: None,
                tool: None,
                images: None,
                archive: None,
                seq: 5,
                ord: 0,
            },
            TimelineMessage {
                kind: "reasoning".into(),
                content: "想".into(),
                session_id: "s".into(),
                time: 0,
                source: None,
                reasoning: None,
                tool: None,
                images: None,
                archive: None,
                seq: 99,
                ord: 1,
            },
        ];
        assert_eq!(history_watermark(messages, "s"), 5, "推理不计入水位线");
    }

    fn user_event_with_source(
        session: &str,
        content: &str,
        timestamp: i64,
        instance_name: &str,
    ) -> SessionEvent {
        SessionEvent::UserMessage(UserMessage {
            content: content.into(),
            timestamp,
            message_sequence: None,
            source: Some(echo_session::event::MessageSource {
                instance_name: instance_name.into(),
                platform: "core".into(),
                user_id: String::new(),
                user_name: String::new(),
                channel: "direct".into(),
                group_name: None,
                received_at_ms: 0,
                message_sequence: 0,
            }),
            images: Vec::new(),
            session: Some(session.into()),
        })
    }

    fn reasoning_event(session: &str, content: &str, timestamp: i64) -> SessionEvent {
        SessionEvent::Reasoning(echo_session::event::ReasoningEvent {
            content: content.into(),
            session: Some(session.into()),
            timestamp,
        })
    }

    fn assistant_event(session: &str, content: &str, timestamp: i64) -> SessionEvent {
        SessionEvent::AssistantMessage(echo_session::event::AssistantMessage {
            timestamp,
            content: content.into(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            session: Some(session.into()),
        })
    }

    /// 回答条目的时间 = **回答自身的落盘时刻**（2026-10 显示顺序根治）。
    ///
    /// 事故形态：派生给 backend 用 carry-forward（= 上一个工具的开始时刻），
    /// 实时投影给的是 `Utc::now()`。同一个回合两侧相差数十分钟，面板拿
    /// `(role, time)` 对齐时认不出是同一条 → 同一条回答显示两次、其中一份被
    /// 插到列表靠后（用户报「会话区显示顺序出了很大的问题」）。
    #[test]
    fn backend_entry_uses_its_own_timestamp_not_carry_forward() {
        let target = "local:workspace:systemx/base:local_user";
        let events = vec![
            user_event(target, "跑一下", 1_700_000_000),
            SessionEvent::ToolCall(echo_session::event::ToolCallEvent {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"sleep 600"}"#.into(),
                session: Some(target.into()),
                // 工具在 t+10 开始，跑了 10 分钟
                started_at_ms: Some(1_700_000_010_000),
            }),
            // 回答在 t+610 落盘（工具开始后 10 分钟）
            assistant_event(target, "跑完了", 1_700_000_610),
        ];
        let (messages, _, _) = display_history_for_session(&events, target, 500);
        let backend = messages
            .iter()
            .find(|m| m.kind == "backend")
            .expect("缺少 backend 条目");
        assert_eq!(
            backend.time, 1_700_000_610,
            "回答必须用自身落盘时刻，不能沿用工具开始时刻（实得 {}）",
            backend.time
        );
    }

    /// 旧日志没有 `timestamp`（=0）：仍按 carry-forward，行为不变（向后兼容）。
    #[test]
    fn backend_entry_without_timestamp_falls_back_to_carry_forward() {
        let target = "s";
        let events = vec![
            user_event(target, "问", 1_700_000_000),
            assistant_event(target, "答", 0),
        ];
        let (messages, _, _) = display_history_for_session(&events, target, 500);
        let backend = messages
            .iter()
            .find(|m| m.kind == "backend")
            .expect("缺少 backend 条目");
        assert_eq!(backend.time, 1_700_000_000, "缺时间戳时沿用最近已知时间");
    }

    /// 孤儿工具结果（压缩吃掉了 ToolCall）：名字用中性占位，**不能**把
    /// `tool_call_id` 当工具名显示（界面上曾出现 `tool_N1QCMouhKj5...`）。
    #[test]
    fn orphan_tool_result_uses_neutral_name_and_keeps_the_id() {
        let target = "s";
        let events = vec![SessionEvent::ToolResult(
            echo_session::event::ToolResultEvent {
                tool_call_id: "tool_N1QCMouhKj5KR7WuskLdDcYa".into(),
                result: "done".into(),
                images: Vec::new(),
                session: Some(target.into()),
                elapsed_ms: Some(3),
            },
        )];
        let (messages, _, _) = display_history_for_session(&events, target, 500);
        let tool = messages[0].tool.as_ref().expect("缺少 tool 载荷");
        assert_eq!(tool.name, "tool", "展示名必须是中性占位");
        assert_eq!(
            tool.tool_call_id, "tool_N1QCMouhKj5KR7WuskLdDcYa",
            "id 仍保留供配对"
        );
    }

    /// 单会话派生：user → tool（就地补丁结果）→ backend；旁观会话被过滤。
    #[test]
    fn derives_user_tool_and_backend_entries_for_one_session() {
        let target = "qq:dm::123";
        let events = vec![
            user_event(target, "你好", 1_700_000_000),
            SessionEvent::ToolCall(echo_session::event::ToolCallEvent {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
                session: Some(target.into()),
                started_at_ms: Some(1_700_000_000_500),
            }),
            SessionEvent::ToolResult(echo_session::event::ToolResultEvent {
                tool_call_id: "call-1".into(),
                result: "ok output".into(),
                images: Vec::new(),
                session: Some(target.into()),
                elapsed_ms: Some(12),
            }),
            SessionEvent::AssistantMessage(echo_session::event::AssistantMessage {
                timestamp: 0,
                content: "最终回复".into(),
                reasoning_content: Some("推理不派生".into()),
                tool_calls: Vec::new(),
                session: Some(target.into()),
            }),
            user_event("qq:group:9:456", "旁观会话", 1_700_000_001),
        ];
        let (messages, total, truncated) = display_history_for_session(&events, target, 500);
        assert_eq!(total, 3, "只取目标会话：{messages:?}");
        assert!(!truncated);
        assert_eq!(messages[0].kind, "user");
        assert_eq!(messages[0].content, "你好");
        assert!(messages[0].source.is_none());
        assert_eq!(messages[1].kind, "tool");
        assert_eq!(
            messages[1].tool.as_ref().and_then(|t| t.output.as_deref()),
            Some("ok output"),
            "工具结果必须就地补丁到调用条目上"
        );
        assert_eq!(messages[2].kind, "backend");
        assert_eq!(messages[2].content, "最终回复");
    }

    /// 定时任务 → system 条目；后台 hook 跳过。
    #[test]
    fn timer_becomes_system_entry_and_background_is_skipped() {
        let events = vec![
            user_event_with_source("s", "plain", 10, "qq"),
            user_event_with_source(
                "s",
                r#"<timer_event>{"task":"提醒喝水"}</timer_event>"#,
                11,
                "timer",
            ),
            user_event_with_source("s", "internal", 12, "background"),
        ];
        let (messages, _, _) = display_history_for_session(&events, "s", 100);
        assert_eq!(messages.len(), 2, "后台 hook 不派生：{messages:?}");
        assert_eq!(messages[0].kind, "user");
        assert_eq!(messages[1].kind, "system");
        assert!(
            messages[1].content.contains("提醒喝水"),
            "{}",
            messages[1].content
        );
    }

    /// 压缩事件 → system 条目（含替换条数）。
    #[test]
    fn compaction_renders_a_system_entry() {
        let events = vec![SessionEvent::Compaction(
            echo_session::event::CompactionEvent {
                replaced_count: 5,
                summary: "摘要体".into(),
                archive: None,
                session: Some("s".into()),
            },
        )];
        let (messages, total, _) = display_history_for_session(&events, "s", 500);
        assert_eq!(total, 1);
        assert_eq!(messages[0].kind, "system");
        assert!(
            messages[0].content.contains("替换 5"),
            "{}",
            messages[0].content
        );
    }

    /// 推理与工具**真实交错**（按到达顺序，不是全堆到末尾）。
    #[test]
    fn reasoning_events_derive_interleaved_with_tools() {
        let events = vec![
            user_event("s", "问", 1),
            reasoning_event("s", "想一", 2),
            SessionEvent::ToolCall(echo_session::event::ToolCallEvent {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
                session: Some("s".into()),
                started_at_ms: Some(3_000),
            }),
            reasoning_event("s", "想二", 4),
        ];
        let (messages, _, _) = display_history_for_session(&events, "s", 100);
        let kinds: Vec<&str> = messages.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(kinds, vec!["user", "reasoning", "tool", "reasoning"]);
    }

    /// 推理缺时间戳时沿用 carry-forward 游标（不是 0）。
    #[test]
    fn reasoning_without_timestamp_falls_back_to_carry_forward() {
        let events = vec![
            user_event("s", "问", 1_700_000_000),
            reasoning_event("s", "想", 0),
        ];
        let (messages, _, _) = display_history_for_session(&events, "s", 100);
        assert_eq!(messages[1].time, 1_700_000_000, "必须沿用最近已知时间");
    }

    /// 空白推理与其它会话的推理都不派生。
    #[test]
    fn blank_and_foreign_reasoning_is_skipped() {
        let events = vec![
            reasoning_event("s", "   ", 1),
            reasoning_event("other", "别人的思考", 2),
        ];
        let (messages, _, _) = display_history_for_session(&events, "s", 100);
        assert!(messages.is_empty(), "{messages:?}");
    }

    /// 水位线按会话过滤。
    #[test]
    fn watermark_filters_by_session() {
        let mut mine = TimelineMessage::backend("x".to_string(), "a".to_string(), 0, None);
        mine.seq = 3;
        let mut theirs = TimelineMessage::backend("y".to_string(), "b".to_string(), 0, None);
        theirs.seq = 9;
        assert_eq!(history_watermark(vec![mine, theirs], "a"), 3);
    }

    // ── 压缩前归档展开（2026-11）─────────────────────────────────────
    //
    // 这批测试锁的是**边界切分**：`replaced_count` 是事件数、不是消息数，
    // 拿消息数去切会切歪（一次工具调用 + 它的结果 = 2 事件 = 1 行）。

    fn compaction_event(session: &str, replaced: usize, archive: &str) -> SessionEvent {
        SessionEvent::Compaction(echo_session::event::CompactionEvent {
            replaced_count: replaced,
            summary: "摘要".into(),
            archive: Some(format!("/tmp/archives/{archive}")),
            session: Some(session.into()),
        })
    }

    /// 假事件源：`归档名 -> 该归档里该会话的事件`。
    struct FakeSource(Vec<(String, Vec<SessionEvent>)>);

    impl FakeSource {
        fn new(entries: Vec<(&str, Vec<SessionEvent>)>) -> Self {
            Self(
                entries
                    .into_iter()
                    .map(|(name, events)| (name.to_string(), events))
                    .collect(),
            )
        }
    }

    impl ArchiveEventSource for FakeSource {
        fn archive_prefix_events(
            &self,
            archive: &str,
            _session_id: &str,
            count: usize,
        ) -> (Vec<SessionEvent>, Option<String>) {
            match self.0.iter().find(|(name, _)| name == archive) {
                Some((_, events)) => (
                    events.iter().take(count).cloned().collect(),
                    None,
                ),
                None => (Vec::new(), Some(format!("该归档已被清理：{archive}"))),
            }
        }
    }

    /// 未展开：只标 `has_older`，不碰归档（默认折叠的数据来源）。
    #[test]
    fn without_expand_only_flags_has_older() {
        let events = vec![
            compaction_event("s", 2, "a.json"),
            user_event("s", "压缩后", 3),
        ];
        let history = display_history_extended(&events, "s", 500, None);
        // 分隔行 + 压缩后那条。
        assert_eq!(history.messages.len(), 2);
        assert!(history.has_older);
        assert!(!history.older_unavailable);
        assert_eq!(
            history.messages[0].archive.as_deref(),
            Some("a.json"),
            "分隔行必须带归档名，否则面板没有入口"
        );
    }

    /// 展开一层：归档里的**前 replaced_count 条**事件派生后拼在分隔行之前。
    #[test]
    fn expand_prepends_the_archived_prefix() {
        let events = vec![
            compaction_event("s", 2, "a.json"),
            user_event("s", "压缩后", 3),
        ];
        // 归档里有 3 条该会话事件，但 replaced_count = 2 → 只取前 2 条。
        let source = FakeSource::new(vec![(
            "a.json",
            vec![
                user_event("s", "压缩前1", 1),
                user_event("s", "压缩前2", 2),
                user_event("s", "已经在日志里", 3),
            ],
        )]);
        let history = display_history_extended(
            &events,
            "s",
            500,
            Some(&ExpandContext {
                source: &source,
                depth: EXPAND_DEPTH_PER_REQUEST,
            }),
        );
        let contents: Vec<&str> = history
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(
            contents,
            vec![
                "压缩前1",
                "压缩前2",
                "上下文已压缩 · 替换 2 条事件（原件已归档，可查看压缩前的历史）",
                "压缩后",
            ],
            "前缀必须拼在分隔行之前，且不能把保留尾重复进来"
        );
        assert!(!history.has_older, "该层没有更早的压缩边界了");
    }

    /// 边界按**事件**切：工具调用 + 结果 = 2 事件 = 1 行，`replaced_count`
    /// 若被当成消息数会少切一半。
    #[test]
    fn expand_slices_by_events_not_messages() {
        let call = SessionEvent::ToolCall(echo_session::event::ToolCallEvent {
            id: "c1".into(),
            name: "echo".into(),
            arguments: "{}".into(),
            session: Some("s".into()),
            started_at_ms: None,
        });
        let result = SessionEvent::ToolResult(echo_session::event::ToolResultEvent {
            tool_call_id: "c1".into(),
            result: "done".into(),
            images: Vec::new(),
            session: Some("s".into()),
            elapsed_ms: None,
        });
        let events = vec![
            compaction_event("s", 2, "a.json"),
            user_event("s", "压缩后", 3),
        ];
        let source = FakeSource::new(vec![(
            "a.json",
            vec![call, result, user_event("s", "更早", 0)],
        )]);
        let history = display_history_extended(
            &events,
            "s",
            500,
            Some(&ExpandContext {
                source: &source,
                depth: EXPAND_DEPTH_PER_REQUEST,
            }),
        );
        // 2 条事件 = 工具行 1 条（调用与结果配对）；第 3 条事件不该进来。
        assert_eq!(history.messages[0].kind, "tool");
        assert_eq!(history.messages[0].tool.as_ref().unwrap().output.as_deref(), Some("done"));
        assert!(
            !history.messages.iter().any(|m| m.content == "更早"),
            "replaced_count 之外的事件不能带进来"
        );
    }

    /// 归档被轮转清理：标 `older_unavailable`，不再说"还能展开"。
    #[test]
    fn missing_archive_is_reported_not_silently_empty() {
        let events = vec![
            compaction_event("s", 2, "gone.json"),
            user_event("s", "压缩后", 3),
        ];
        let source = FakeSource::new(vec![]);
        let history = display_history_extended(
            &events,
            "s",
            500,
            Some(&ExpandContext {
                source: &source,
                depth: EXPAND_DEPTH_PER_REQUEST,
            }),
        );
        assert!(history.older_unavailable);
        assert!(!history.has_older);
        assert_eq!(history.messages.len(), 2, "分隔行与保留尾都还在");
    }

    /// 一次只展开一层：更深的边界留着让用户再点（`has_older` 保持 true）。
    #[test]
    fn one_layer_per_request() {
        let events = vec![
            compaction_event("s", 1, "b.json"),
            user_event("s", "压缩后", 2),
        ];
        // b.json 里还有一条更早的压缩边界（指向 c.json）。
        let source = FakeSource::new(vec![("b.json", vec![compaction_event("s", 1, "c.json")])]);
        let history = display_history_extended(
            &events,
            "s",
            500,
            Some(&ExpandContext {
                source: &source,
                depth: EXPAND_DEPTH_PER_REQUEST,
            }),
        );
        // 展开一层：b 里的前缀（那条更早的分隔行）+ 原来的分隔行 + 保留尾。
        assert_eq!(history.messages.len(), 3);
        assert!(history.has_older, "更深的层要留给下一次点击");
        assert_eq!(
            history.messages[0].archive.as_deref(),
            Some("c.json"),
            "更深的分隔行也带自己的归档名"
        );

        // 给两层的预算 → 连 c 也展开。
        let source2 = FakeSource::new(vec![
            ("b.json", vec![compaction_event("s", 1, "c.json")]),
            ("c.json", vec![user_event("s", "最早", 0)]),
        ]);
        let deep = display_history_extended(
            &events,
            "s",
            500,
            Some(&ExpandContext {
                source: &source2,
                depth: 2,
            }),
        );
        assert!(deep.messages.iter().any(|m| m.content == "最早"));
        assert!(!deep.has_older);
    }

    /// 展开时忽略 limit（一次给完整层），未展开时按 limit 取尾部。
    #[test]
    fn expand_ignores_limit_but_unexpanded_truncates() {
        let events = vec![
            compaction_event("s", 2, "a.json"),
            user_event("s", "压缩后", 3),
        ];
        let source = FakeSource::new(vec![(
            "a.json",
            vec![user_event("s", "压缩前1", 1), user_event("s", "压缩前2", 2)],
        )]);
        let expanded = display_history_extended(
            &events,
            "s",
            1,
            Some(&ExpandContext {
                source: &source,
                depth: EXPAND_DEPTH_PER_REQUEST,
            }),
        );
        assert_eq!(expanded.messages.len(), 4, "展开时 limit 不生效");
        assert!(!expanded.truncated);

        // 未展开：按 limit 取尾部，**但压缩边界那一行被钉在窗口顶端**
        // （它是面板唯一的入口，被尾部截断丢掉就再也点不到了）。
        let plain = display_history_extended(&events, "s", 1, None);
        assert_eq!(plain.messages.len(), 2);
        assert!(plain.truncated);
        assert_eq!(plain.total, 2, "total 是截断前的数量");
        assert_eq!(
            plain.messages[0].archive.as_deref(),
            Some("a.json"),
            "边界行必须留在窗口里"
        );
        assert_eq!(plain.messages[1].content, "压缩后");
    }

    /// 边界行在尾部窗口内时**不重复**插入（窗口够大 → 原样返回）。
    #[test]
    fn boundary_row_is_not_duplicated_when_it_fits() {
        let events = vec![
            compaction_event("s", 2, "a.json"),
            user_event("s", "压缩后", 3),
        ];
        let history = display_history_extended(&events, "s", 500, None);
        assert_eq!(history.messages.len(), 2, "不该多出一条边界副本");
        assert!(!history.truncated);
    }

    /// 空输入 / 无可派生条目 → 水位线 0。
    #[test]
    fn watermark_is_zero_for_empty_input() {
        assert_eq!(history_watermark(vec![], "s"), 0);
    }
}

/// `TrunkStore` 就是归档事件的来源：读文件 + 解析 + 按会话切片都在
/// `store::archives` 里（I/O），本模块只管投影。
impl ArchiveEventSource for TrunkStore {
    fn archive_prefix_events(
        &self,
        archive: &str,
        session_id: &str,
        count: usize,
    ) -> (Vec<echo_session::SessionEvent>, Option<String>) {
        self.archive_prefix_events(archive, session_id, count)
    }
}

impl TrunkStore {
    /// 从事件日志为单个会话派生显示历史（最近 `limit` 条，时间升序）。
    ///
    /// 返回 `(messages, total, truncated)`。规则全在上面的自由函数里，这里
    /// 只把日志交出去——**一行转发**（2026-11 外置前它在宿主 `session/`）。
    pub fn display_history_for_session(
        &self,
        session_id: &str,
        limit: usize,
    ) -> (Vec<TimelineMessage>, usize, bool) {
        display_history_for_session(&self.event_log(), session_id, limit)
    }

    /// 派生显示历史，并按需把**压缩前归档**接上去（粘贴进同一条消息流）。
    ///
    /// `expand_depth`：允许本次解析几层归档（`None`/`Some(0)` = 不展开，
    /// 只在结果里标 `has_older`）。一次一层见
    /// [`EXPAND_DEPTH_PER_REQUEST`]。
    ///
    /// 注意：本方法会在调用线程里**同步解析 JSON**（一份可达 20+ MB）。
    /// 调用方要么在 `spawn_blocking` 里，要么接受这次阻塞。
    pub fn display_history_extended(
        &self,
        session_id: &str,
        limit: usize,
        expand_depth: Option<usize>,
    ) -> ExtendedHistory {
        let events = self.event_log();
        let context = expand_depth.filter(|depth| *depth > 0).map(|depth| ExpandContext {
            source: self as &dyn ArchiveEventSource,
            depth,
        });
        display_history_extended(&events, session_id, limit, context.as_ref())
    }
}
