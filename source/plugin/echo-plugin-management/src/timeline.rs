//! 显示时间线的**投影策略**：把后端事件翻成面板可见的对话条目。
//!
//! ## 为什么归管理面插件
//!
//! 时间线是**面板显示的东西**：哪些事件该出现（用户消息 / 定时任务摘要 /
//! 工具调用与结果 / 最终回复 / 推理），哪些是后台噪声（`background` 输入、
//! 生命周期事件），字段怎么映射（来源标签、图片规范化、工具概要截断）。这些
//! 全是**显示投影策略**——与 management 插件（WS 桥接、事件扇出）同一层。
//!
//! 此前它住在宿主 `timeline.rs`（590 行），只因为要往 `TrunkStore` 里写。
//!
//! ## 宿主端口只有三件事（[`TimelineHost`]）
//!
//! | 端口 | 为什么在宿主 |
//! |---|---|
//! | `push` | 时间线存在 `TrunkStore` 里（受限长度、落盘、dirty 标记） |
//! | `next_seq` | 单调序号是存储的一部分（增量同步游标靠它） |
//! | `complete_running_tool` | 就地更新"运行中"的工具条目 + 推进 seq |
//!
//! **刻意不给 `timeline_mut()`**：那会让插件认识宿主的锁类型，"seam 变影子"
//! 就是这么发生的。就地更新被收敛成一个语义方法（按 `tool_call_id` 找运行中
//! 条目并打完成态）。
//!
//! ## 一条硬规则
//!
//! 时间线是**投影**，不是第二事实源：它只从事件派生，永不回写事件日志。所以
//! 投影器可以随时重建（`session::history` 就走同一批规则）。

use echo_protocol::{BackendEvent, TimelineMessage, TimelineSource, TimelineTool};

/// 宿主端口：投影器需要的三件事。
pub trait TimelineHost: Send + Sync {
    /// 追加一条时间线条目（宿主负责赋 seq/ord、限长、标脏）。
    fn push(&self, message: TimelineMessage);
    /// 取下一个单调序号（就地更新时用）。
    fn next_seq(&self) -> u64;
    /// 把"运行中"的工具条目打成完成态。
    ///
    /// 返回 `true` = 找到了并就地更新（宿主已推进 seq）；`false` = 没找到
    /// 运行中的条目，调用方应直接 `push` 一条完成态条目。
    ///
    /// 匹配规则（与文档一致）：`tool_call_id` 非空时按 id 匹配，否则按
    /// 「同会话 + 同工具名 + 尚无 output」退化匹配——2026-09 前的工具事件
    /// 不带 call id。
    fn complete_running_tool(&self, patch: ToolCompletion<'_>) -> bool;
}

/// 就地更新一条工具完成态的补丁。
#[derive(Debug, Clone, Copy)]
pub struct ToolCompletion<'a> {
    pub session_id: &'a str,
    pub tool_name: &'a str,
    pub tool_call_id: &'a str,
    /// 已截断的概要（见 [`summarize_timeline_value`]）。
    pub output: &'a str,
    pub failed: bool,
    pub timed_out: bool,
    pub elapsed_ms: Option<u64>,
}

/// 投影器：订阅事件总线，把事件写进时间线。
pub struct TimelineProjector<H: TimelineHost + ?Sized> {
    host: std::sync::Arc<H>,
}

impl<H: TimelineHost + ?Sized + 'static> TimelineProjector<H> {
    pub fn new(host: std::sync::Arc<H>) -> Self {
        Self { host }
    }

    /// 订阅事件总线（observe 语义：不影响其它监听者）。返回撤销句柄。
    pub fn subscribe(
        self: &std::sync::Arc<Self>,
        bus: &std::sync::Arc<echo_context::EventBus>,
    ) -> echo_context::Disposer {
        let projector = std::sync::Arc::clone(self);
        bus.observe::<BackendEvent, _>(move |event| {
            projector.record(event);
        })
    }

    /// 记一条事件进时间线。
    pub fn record(&self, event: &BackendEvent) {
        match event {
            BackendEvent::MessageReceived {
                session_id,
                instance_name,
                platform,
                user_id,
                user_name,
                channel,
                group_name,
                content,
                images,
                timestamp,
                received_at_ms,
                message_sequence,
                ..
            } => {
                if instance_name == "background" {
                    return; // 内部父上下文 hook，不是对话内容
                }
                if instance_name == "timer" {
                    let task = timeline_timer_task(content);
                    let content = match task {
                        Some(task) => format!(
                            "定时任务触发 · {}",
                            truncate_one_line(&task, 120)
                        ),
                        None => "定时任务触发".into(),
                    };
                    self.host.push(TimelineMessage::system(
                        content,
                        session_id.clone(),
                        *timestamp,
                    ));
                    return;
                }
                // 图片规范化（2026-09-24）：媒体引用（`/media/<id>`）原样
                // 保留（轻量，面板懒加载）；遗留的内嵌 data URI 落盘后改写
                // 为引用（落盘失败则省略占位）——时间线是"显示历史"，不因
                // 一张图写不进去而丢整条消息。
                let images: Vec<String> = images
                    .iter()
                    .map(|image| echo_defs::media_store::spill_or_keep(image))
                    .collect();
                self.host.push(TimelineMessage::user(
                    content.clone(),
                    session_id.clone(),
                    *timestamp,
                    Some(TimelineSource {
                        instance_name: instance_name.clone(),
                        platform: platform.clone(),
                        user_id: user_id.clone(),
                        user_name: user_name.clone(),
                        channel: channel.clone(),
                        group_name: group_name.clone(),
                        received_at_ms: *received_at_ms,
                        message_sequence: *message_sequence,
                    }),
                    images,
                ));
            }
            BackendEvent::AgentReasoning {
                session_id,
                branch_id: _,
                team_id: _,
                content,
            } => {
                let content = content.trim().to_string();
                if content.is_empty() {
                    return;
                }
                // 按到达顺序直接落独立条目：推理与工具调用在时间线中真实交错。
                self.host.push(TimelineMessage {
                    kind: "reasoning".into(),
                    content,
                    session_id: session_id.clone(),
                    time: chrono::Utc::now().timestamp(),
                    source: None,
                    reasoning: None,
                    tool: None,
                    images: None,
                    seq: 0, // 由宿主赋递增序号
                    ord: 0, // 由宿主赋（分页锚点，此后不变）
                });
            }
            BackendEvent::AgentOutput {
                session_id,
                team_id: _,
                content,
                branch_id: _,
            } => {
                // 推理已作为独立 reasoning 条目落库（见 AgentReasoning 分支）；
                // backend 条目不再附加 reasoning（旧数据仍保留字段兼容）。
                self.host.push(TimelineMessage::backend(
                    content.clone(),
                    session_id.clone(),
                    chrono::Utc::now().timestamp(),
                    None,
                ));
            }
            BackendEvent::ToolCall {
                session_id,
                tool_name,
                arguments,
                tool_call_id,
                ..
            } => {
                self.host.push(TimelineMessage::tool(
                    tool_name.clone(),
                    session_id.clone(),
                    chrono::Utc::now().timestamp(),
                    TimelineTool {
                        name: tool_name.clone(),
                        input: summarize_timeline_value(arguments, 200),
                        output: None,
                        failed: false,
                        tool_call_id: tool_call_id.clone(),
                        timed_out: false,
                        started_at_ms: None,
                        elapsed_ms: None,
                    },
                ));
            }
            BackendEvent::ToolResult {
                session_id,
                tool_name,
                result,
                tool_call_id,
                timed_out,
                elapsed_ms,
                ..
            } => {
                let failed = result.starts_with("error:");
                let output = summarize_timeline_value(result, 200);
                // 优先就地更新"运行中"的条目（同一个 tool_call_id）：这样
                // 面板不必把一条工具拆成两行。找不到才追加完成态条目。
                let updated = self.host.complete_running_tool(ToolCompletion {
                    session_id,
                    tool_name,
                    tool_call_id,
                    output: &output,
                    failed,
                    timed_out: *timed_out,
                    elapsed_ms: *elapsed_ms,
                });
                if !updated {
                    self.host.push(TimelineMessage::tool(
                        tool_name.clone(),
                        session_id.clone(),
                        chrono::Utc::now().timestamp(),
                        TimelineTool {
                            name: tool_name.clone(),
                            input: String::new(),
                            output: Some(output),
                            failed,
                            tool_call_id: tool_call_id.clone(),
                            timed_out: *timed_out,
                            started_at_ms: None,
                            elapsed_ms: *elapsed_ms,
                        },
                    ));
                }
            }
            // 其余事件不进显示时间线：它们是生命周期 / 观测 / 状态广播，
            // 面板另有专门通道（会话列表、插件详情、实例状态）。
            _ => {}
        }
    }
}

/// 推理条目的 wire 瘦身阈值（2026-09，刷新加速）：推理正文占时间线快照的
/// 约七成（实测 1024 条里 1MB+），而滚动历史里的思维链几乎不回看——最近
/// [`WIRE_REASONING_KEEP_FULL`] 条保留全文，更早的只留开头
/// [`WIRE_REASONING_HEAD_CHARS`] 字 + 截断标注。持久化/内存中的原文不动，
/// 只瘦身发往前端的快照（`RequestTrunkTimeline` 的两种路径共用）。
pub const WIRE_REASONING_KEEP_FULL: usize = 40;
/// 被截断条目保留的开头字符数。
pub const WIRE_REASONING_HEAD_CHARS: usize = 240;

/// 按"新近程度"瘦身推理条目（见上方常量说明）。非推理条目原样保留。
pub fn elide_reasoning_for_wire(messages: Vec<TimelineMessage>) -> Vec<TimelineMessage> {
    let reasoning: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.kind == "reasoning" && !message.content.is_empty())
        .map(|(index, _)| index)
        .collect();
    if reasoning.len() <= WIRE_REASONING_KEEP_FULL {
        return messages;
    }
    let mut messages = messages;
    let cutoff = reasoning.len() - WIRE_REASONING_KEEP_FULL;
    for &index in &reasoning[..cutoff] {
        let total = messages[index].content.chars().count();
        if total <= WIRE_REASONING_HEAD_CHARS {
            continue;
        }
        let head: String = messages[index]
            .content
            .chars()
            .take(WIRE_REASONING_HEAD_CHARS)
            .collect();
        messages[index].content = format!("{head}…\n（较早的推理已截断：原文 {total} 字）");
    }
    messages
}

/// 从 `<timer_event>` 载荷里取任务摘要（可解析时）。
pub fn timeline_timer_task(content: &str) -> Option<String> {
    let body = content
        .strip_prefix(echo_defs::input_marker::TIMER_EVENT_OPEN)?
        .strip_suffix(echo_defs::input_marker::TIMER_EVENT_CLOSE)?;
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value.get("task")?.as_str().map(str::to_string)
}

/// 折叠空白并截到单行显示宽度。
pub fn truncate_one_line(value: &str, max_chars: usize) -> String {
    let collapsed: String = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        collapsed.chars().take(max_chars).collect::<String>() + "…"
    }
}

/// 截断工具输入/输出供显示，折叠空白。
pub fn summarize_timeline_value(value: &str, max_chars: usize) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(value) {
        Ok(serde_json::Value::Object(map)) => {
            let summarized = map
                .iter()
                .take(8)
                .map(|(key, value)| {
                    let rendered = if is_sensitive_timeline_key(key) {
                        "[已隐藏]".into()
                    } else {
                        summarize_json_leaf(value)
                    };
                    format!("{key}={rendered}")
                })
                .collect::<Vec<_>>()
                .join(" · ");
            format!("{{{summarized}}}")
        }
        _ => truncate_one_line(value, max_chars),
    };
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let head: String = compact.chars().take(max_chars).collect();
    format!("{head}…")
}

fn summarize_json_leaf(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => truncate_one_line(text, 80),
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::Array(items) => format!("[{} 项]", items.len()),
        serde_json::Value::Object(map) => format!("{{{} 项}}", map.len()),
    }
}

/// 敏感键判定（工具参数/结果里这些键的值不显示）。
///
/// 与出站脱敏同口径：`api_key` / `token` / `password` / `secret` / `authorization`
/// 一律隐藏。这里是**显示层**的第二道保险——脱敏器可能未接线（单测）。
fn is_sensitive_timeline_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    [
        "api_key",
        "apikey",
        "token",
        "password",
        "secret",
        "authorization",
        "access_key",
        "private_key",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeHost {
        entries: Mutex<Vec<TimelineMessage>>,
        seq: std::sync::atomic::AtomicU64,
    }

    impl TimelineHost for FakeHost {
        fn push(&self, message: TimelineMessage) {
            self.entries.lock().unwrap().push(message);
        }
        fn next_seq(&self) -> u64 {
            self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
        }
        fn complete_running_tool(&self, patch: ToolCompletion<'_>) -> bool {
            let mut entries = self.entries.lock().unwrap();
            let found = entries.iter_mut().rev().find(|entry| {
                entry.kind == "tool"
                    && entry.session_id == patch.session_id
                    && entry.tool.as_ref().is_some_and(|tool| {
                        tool.tool_call_id == patch.tool_call_id && tool.output.is_none()
                    })
            });
            match found {
                Some(entry) => {
                    if let Some(tool) = entry.tool.as_mut() {
                        tool.output = Some(patch.output.to_string());
                        tool.failed = patch.failed;
                        tool.timed_out = patch.timed_out;
                        tool.elapsed_ms = patch.elapsed_ms;
                    }
                    true
                }
                None => false,
            }
        }
    }

    fn projector() -> (std::sync::Arc<TimelineProjector<FakeHost>>, std::sync::Arc<FakeHost>) {
        let host = std::sync::Arc::new(FakeHost::default());
        (
            std::sync::Arc::new(TimelineProjector::new(host.clone())),
            host,
        )
    }

    fn tool_call(id: &str, name: &str) -> BackendEvent {
        BackendEvent::ToolCall {
            session_id: "s".into(),
            team_id: None,
            tool_name: name.into(),
            arguments: r#"{"path":"/tmp/x"}"#.into(),
            tool_call_id: id.into(),
            branch_id: "b".into(),
            started_at_ms: None,
        }
    }

    fn tool_result(id: &str, name: &str, result: &str) -> BackendEvent {
        BackendEvent::ToolResult {
            session_id: "s".into(),
            team_id: None,
            tool_name: name.into(),
            result: result.into(),
            tool_call_id: id.into(),
            timed_out: false,
            branch_id: "b".into(),
            elapsed_ms: Some(12),
        }
    }

    /// 工具调用与结果**合并成一条**（面板不必把一次工具拆成两行）。
    #[test]
    fn tool_result_completes_the_running_entry_in_place() {
        let (projector, host) = projector();
        projector.record(&tool_call("c1", "read_file"));
        projector.record(&tool_result("c1", "read_file", "ok"));
        let entries = host.entries.lock().unwrap();
        assert_eq!(entries.len(), 1, "完成态必须就地更新，不是追加");
        let tool = entries[0].tool.as_ref().expect("tool 条目");
        assert_eq!(tool.output.as_deref(), Some("ok"));
        assert!(!tool.failed);
    }

    /// 找不到运行中的条目时**追加**完成态（不是静默丢弃）。
    #[test]
    fn orphan_tool_result_is_appended() {
        let (projector, host) = projector();
        projector.record(&tool_result("c-missing", "read_file", "ok"));
        let entries = host.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].tool.as_ref().and_then(|t| t.output.as_deref()),
            Some("ok")
        );
    }

    /// `error:` 开头的结果标为失败（面板据此标红）。
    #[test]
    fn error_results_are_marked_failed() {
        let (projector, host) = projector();
        projector.record(&tool_call("c1", "bash"));
        projector.record(&tool_result("c1", "bash", "error: boom"));
        let entries = host.entries.lock().unwrap();
        assert!(entries[0].tool.as_ref().expect("tool").failed);
    }

    /// 后台输入不进时间线（它是内部父上下文 hook，不是对话内容）。
    #[test]
    fn background_input_is_skipped() {
        let (projector, host) = projector();
        projector.record(&BackendEvent::MessageReceived {
            session_id: "s".into(),
            instance_name: "background".into(),
            platform: "local".into(),
            user_id: "u".into(),
            user_name: "u".into(),
            channel: "dm".into(),
            group_name: None,
            content: "internal".into(),
            images: vec![],
            timestamp: 0,
            received_at_ms: 0,
            message_sequence: 0,
            team_id: None,
        });
        assert!(
            host.entries.lock().unwrap().is_empty(),
            "内部 hook 不该出现在对话时间线里"
        );
    }

    /// 敏感键在工具概要里显示为 `[已隐藏]`（显示层第二道保险）。
    #[test]
    fn sensitive_keys_are_hidden_in_tool_summaries() {
        let text = summarize_timeline_value(r#"{"api_key":"sk-secret","path":"/tmp"}"#, 200);
        assert!(text.contains("[已隐藏]"), "{text}");
        assert!(!text.contains("sk-secret"), "密钥不得出现在时间线：{text}");
    }

    /// 定时任务摘要可从信封里取出；坏载荷是 `None`（不是 panic）。
    #[test]
    fn timer_task_is_extracted_from_the_envelope_only() {
        assert_eq!(
            timeline_timer_task(
                r#"<timer_event>{"task":"提醒喝水","due_at":"2026-01-01T00:00:00Z"}</timer_event>"#
            )
            .as_deref(),
            Some("提醒喝水")
        );
        assert!(timeline_timer_task("<timer_event>not json</timer_event>").is_none());
        assert!(timeline_timer_task("plain text").is_none());
    }

    /// 单行折叠：空白压成一个空格，超长按字符截断（多字节安全）。
    #[test]
    fn truncate_one_line_collapses_whitespace_and_cuts_on_char_boundary() {
        assert_eq!(truncate_one_line("a\nb   c", 10), "a b c");
        let cut = truncate_one_line("一二三四五六七八九十", 5);
        assert_eq!(cut.chars().count(), 6, "5 字符 + 省略号");
        assert!(cut.ends_with('…'));
    }

    /// 敏感键**嵌套也不泄露**：嵌套对象整体收缩为 `{n 项}`，不下钻。
    ///
    /// 这条是原宿主的 2026-10 审计修复——插件实现更严格（顶层敏感键显示
    /// `[已隐藏]`，任何嵌套对象只报条目数）。
    #[test]
    fn nested_sensitive_values_never_reach_the_timeline() {
        let top = summarize_timeline_value(r#"{"api_key":"sk-top","cmd":"ls"}"#, 200);
        assert!(!top.contains("sk-top"), "顶层密钥泄露：{top}");
        assert!(top.contains("[已隐藏]"));
        let nested = summarize_timeline_value(
            r#"{"cmd":"env","env":{"API_KEY":"sk-nested","PATH":"/usr/bin"}}"#,
            200,
        );
        assert!(!nested.contains("sk-nested"), "嵌套密钥泄露：{nested}");
        let deep = summarize_timeline_value(r#"{"a":{"b":{"c":"deep-secret-value"}}}"#, 200);
        assert!(!deep.contains("deep-secret-value"), "深层泄露：{deep}");
    }

    /// 用户消息与最终回复各落一条，kind 正确（基本投影）。
    #[test]
    fn user_message_and_output_are_projected_as_their_own_kinds() {
        let (projector, host) = projector();
        projector.record(&BackendEvent::MessageReceived {
            session_id: "local:tui::u".into(),
            instance_name: "qq".into(),
            platform: "qq".into(),
            user_id: "1".into(),
            user_name: "alice".into(),
            channel: "direct".into(),
            group_name: None,
            content: "你好".into(),
            images: vec![],
            timestamp: 1_700_000_000,
            received_at_ms: 1_700_000_000_123,
            message_sequence: 1,
            team_id: None,
        });
        projector.record(&BackendEvent::AgentOutput {
            session_id: "local:tui::u".into(),
            team_id: None,
            content: "回复".into(),
            branch_id: None,
        });
        let entries = host.entries.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, "user");
        assert_eq!(entries[0].content, "你好");
        assert_eq!(entries[1].kind, "backend");
        assert_eq!(entries[1].content, "回复");
    }

    /// 订阅闭环：经事件总线发出的事件也进时间线（投影器是 observe 监听者）。
    #[test]
    fn bus_subscription_records_events() {
        use echo_context::DispatchMode;
        let (projector, host) = projector();
        let bus = std::sync::Arc::new(echo_context::EventBus::default());
        let _keep = projector.subscribe(&bus);
        bus.emit_sync(
            BackendEvent::AgentOutput {
                session_id: "local:tui::u".into(),
                team_id: None,
                content: "通过总线".into(),
                branch_id: None,
            },
            DispatchMode::Observe,
        );
        let entries = host.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "通过总线");
    }

    /// 短推理与非推理条目在 wire 瘦身里**原样保留**（只截长推理）。
    #[test]
    fn wire_elision_leaves_short_and_non_reasoning_entries_alone() {
        let reasonings: Vec<TimelineMessage> = (0..3)
            .map(|index| TimelineMessage {
                kind: "reasoning".into(),
                content: format!("短{index}"),
                session_id: "s".into(),
                time: index,
                source: None,
                reasoning: None,
                tool: None,
                images: None,
                seq: 0,
                ord: 0,
            })
            .collect();
        let backend = TimelineMessage::backend("正文".to_string(), "s".to_string(), 0, None);
        let mut messages = reasonings;
        messages.push(backend);
        let elided = elide_reasoning_for_wire(messages.clone());
        assert_eq!(elided.len(), messages.len());
        for (before, after) in messages.iter().zip(elided.iter()) {
            assert_eq!(before.content, after.content, "不该被改动");
        }
    }

    /// 大量推理时只瘦身较早的（最近 40 条保全文）。
    #[test]
    fn wire_elision_keeps_recent_reasoning_verbatim() {
        let mut messages = Vec::new();
        for index in 0..60 {
            messages.push(TimelineMessage {
                kind: "reasoning".into(),
                content: format!("{index}-{}", "x".repeat(400)),
                session_id: "s".into(),
                time: index,
                source: None,
                reasoning: None,
                tool: None,
                images: None,
                seq: index as u64,
                ord: index as u64,
            });
        }
        let elided = elide_reasoning_for_wire(messages);
        assert_eq!(
            elided[59].content,
            format!("59-{}", "x".repeat(400)),
            "最近一条必须保全文"
        );
        assert!(
            elided[0].content.contains("较早的推理已截断"),
            "最早的应被截断：{}",
            &elided[0].content[..40.min(elided[0].content.len())]
        );
    }
}
