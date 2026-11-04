//! 显示文本的**共用工具**：定时信封解析、单行折叠、工具概要截断。
//!
//! ## 为什么在 `echo-protocol` 而不是某个插件里
//!
//! 这五个函数同时被两条显示路径用：实时投影（`echo-plugin-management` 的
//! `timeline`）与按需派生（`echo-plugin-session` 的 `store::history`）。
//! 2026-11 会话子系统外置时，派生侧搬进了会话插件，于是这份工具成了
//! **跨插件共用的词汇**——按依赖纪律（`scripts/deps-policy.txt` 第 4 条）
//! 不能一个插件 `use` 另一个插件，只能下沉到共同的下游：线格式词汇层。
//!
//! 它们操作的类型全是 `&str` / `serde_json::Value`，没有领域状态，下沉无损。

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

/// 推理条目的 wire 瘦身阈值（2026-09，刷新加速）：推理正文占时间线快照的
/// 约七成（实测 1024 条里 1MB+），而滚动历史里的思维链几乎不回看——最近
/// [`WIRE_REASONING_KEEP_FULL`] 条保留全文，更早的只留开头
/// [`WIRE_REASONING_HEAD_CHARS`] 字 + 截断标注。持久化/内存中的原文不动，
/// 只瘦身发往前端的快照（`RequestTrunkTimeline` 的两种路径共用）。
pub const WIRE_REASONING_KEEP_FULL: usize = 40;
/// 被截断条目保留的开头字符数。
pub const WIRE_REASONING_HEAD_CHARS: usize = 240;

/// 按"新近程度"瘦身推理条目（见上方常量说明）。非推理条目原样保留。
pub fn elide_reasoning_for_wire(
    messages: Vec<crate::event::TimelineMessage>,
) -> Vec<crate::event::TimelineMessage> {
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
