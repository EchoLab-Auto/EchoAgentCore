//! Structured input markers — the single source of truth for the
//! `<platform>_message_hook` / `<timer_event>` envelope convention.
//!
//! The agent loop receives inbound platform messages wrapped in a marked
//! envelope (`<qq_message_hook>JSON</qq_message_hook>` for QQ, and the same
//! shape for any other platform), and backend events (timers) similarly. These
//! markers carry the structured payload's provenance and `message_sequence`;
//! the envelope keeps the actual content JSON-escaped so it can never be
//! confused with source metadata.
//!
//! **Nothing here knows a platform name**: the open/close marker is derived
//! from whatever platform string an instance reports, and detection accepts any
//! `<X_message_hook>` (`X` = word characters). Everything that constructs,
//! detects, or parses these envelopes lives here — one definition, no scattered
//! literals.

/// Open marker for a platform message hook, e.g. `<qq_message_hook>`.
pub fn hook_open(platform: &str) -> String {
    format!("<{platform}_message_hook>")
}

/// Close marker for a platform message hook, e.g. `</qq_message_hook>`.
pub fn hook_close(platform: &str) -> String {
    format!("</{platform}_message_hook>")
}

/// Wrap a structured payload in a platform message hook envelope.
pub fn wrap_hook(platform: &str, json: &str) -> String {
    format!("{}\n{json}\n{}", hook_open(platform), hook_close(platform))
}

/// Serialize a JSON value (pretty) and wrap it in a platform hook envelope.
///
/// Single entry point for building structured hook inputs — instance bridges
/// and the frontend `SendMessage` command both use this so the wire format
/// cannot drift.
pub fn wrap_hook_value(platform: &str, value: &serde_json::Value) -> String {
    let json = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    wrap_hook(platform, &json)
}

/// The timer-event envelope opener.
pub const TIMER_EVENT_OPEN: &str = "<timer_event>";

/// The timer-event envelope closer.
pub const TIMER_EVENT_CLOSE: &str = "</timer_event>";

/// The fixed (non-platform) structured input markers.
///
/// Platform hooks are **not** listed here — they are matched positionally by
/// [`platform_hook_of`], so a new platform needs no change to this module.
pub const FIXED_INPUT_MARKERS: [&str; 2] = ["<backend_message_hook>", TIMER_EVENT_OPEN];

/// Suffix that marks a platform message-hook opener (`<qq_message_hook>`).
const HOOK_SUFFIX: &str = "_message_hook>";

/// If `content` opens with a platform message hook, return that platform.
///
/// Matches `<X_message_hook>` where `X` is a non-empty run of word characters
/// at the very start of the content, so ordinary text that merely mentions a
/// marker (or contains JSON braces) is never misread as structured input.
pub fn platform_hook_of(content: &str) -> Option<&str> {
    // 固定标记（`<backend_message_hook>` / `<timer_event>`）不是平台 hook——
    // 它们虽然同形，但语义是「宿主内部输入」，平台名那一段是保留字。
    if FIXED_INPUT_MARKERS
        .iter()
        .any(|marker| content.starts_with(marker))
    {
        return None;
    }
    let rest = content.strip_prefix('<')?;
    let end = rest.find(HOOK_SUFFIX)?;
    let platform = &rest[..end];
    if platform.is_empty()
        || !platform
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some(platform)
}

/// Whether `content` starts with a structured input marker (platform hook,
/// backend message hook, or timer event).
pub fn is_structured_input(content: &str) -> bool {
    platform_hook_of(content).is_some()
        || FIXED_INPUT_MARKERS
            .iter()
            .any(|marker| content.starts_with(marker))
}

/// Extract the JSON payload (between the first `{` and last `}`) of a
/// structured input, if it is one.
pub fn hook_payload(content: &str) -> Option<String> {
    if !is_structured_input(content) {
        return None;
    }
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    Some(content[start..=end].to_string())
}

/// Extract the `message_sequence` from a structured input's JSON payload, if
/// present.
pub fn structured_message_sequence(content: &str) -> Option<u64> {
    let payload = hook_payload(content)?;
    serde_json::from_str::<serde_json::Value>(&payload).ok()?["message_sequence"].as_u64()
}

/// Build the structured hook input for one inbound instance message.
///
/// **构造与解析必须同住一个模块。** [`structured_message_sequence`] 读的是
/// 这里的 `"message_sequence"` 字段；此前构造方在宿主
/// （`echo-agent/src/packages/instance_ops/bridge.rs`），解析方在这里——两侧
/// 靠字符串字面量对齐，改字段名不会有任何编译错误，只会静默丢掉序号。
///
/// ## 为什么这属于定义层
///
/// 它没有任何宿主知识：输入是定义层的 [`IncomingMessage`]，输出是定义层
/// 的 [`wrap_hook_value`] 信封。字段名（`event` / `instance` / `channel` /
/// `sender` / `at_me` / `message_sequence` …）是**跨边界的线格式**——平台
/// 实例发什么、agent 循环读什么、面板怎么显示，全按这一份。
///
/// ## 字段约定
///
/// - `content` 放在 JSON 里而不是拼接在信封外：内容因此永远不可能与
///   sender / group 这类来源元数据混淆（提示注入的第一道防线）；
/// - 文件附件（已下载到本机）：`path` 是绝对路径，工具可直接读；下载失败
///   时 `path = null` 并以 `error` 说明原因；
/// - `event` = `"{platform}_message"`，与信封的 `<{platform}_message_hook>`
///   同源（都从 `platform` 派生，不各写一遍）。
///
/// [`IncomingMessage`]: crate::chat::IncomingMessage
pub fn format_instance_hook(
    msg: &crate::chat::IncomingMessage,
    message_sequence: u64,
    received_at_ms: i64,
) -> String {
    use crate::chat::ChannelType;

    let channel = match &msg.channel {
        ChannelType::Direct => serde_json::json!({
            "type": "private"
        }),
        ChannelType::Group { group_id } => serde_json::json!({
            "type": "group",
            "group_id": group_id,
            "group_name": msg.group_name
        }),
    };
    let files: Vec<serde_json::Value> = msg
        .files
        .iter()
        .map(|f| {
            serde_json::json!({
                "name": f.name,
                "path": f.path,
                "size": f.size,
                "error": f.error,
            })
        })
        .collect();
    let payload = serde_json::json!({
        "event": format!("{}_message", msg.platform),
        "instance": msg.instance_name,
        "platform": msg.platform,
        "channel": channel,
        "sender": {
            "user_id": msg.user_id,
            "name": msg.user_name
        },
        "at_me": msg.at_me,
        "timestamp": msg.timestamp,
        "received_at_ms": received_at_ms,
        "message_sequence": message_sequence,
        "content": msg.content,
        "metadata": msg.metadata,
        "images": msg.images,
        "files": files
    });
    wrap_hook_value(&msg.platform, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_wrap_roundtrip() {
        let wrapped = wrap_hook("qq", r#"{"a":1}"#);
        assert!(wrapped.starts_with("<qq_message_hook>"));
        assert!(wrapped.ends_with("</qq_message_hook>"));
    }

    /// **构造与解析必须闭环**：这是本模块存在的核心理由。
    ///
    /// `format_instance_hook` 造的 envelope，`structured_message_sequence` 必须
    /// 能读出同一个序号。此前两侧分居两个 crate（构造在宿主、解析在这里），
    /// 靠字符串字面量对齐——改字段名不会有编译错误，只会静默丢序号。
    #[test]
    fn instance_hook_roundtrips_through_the_parser() {
        let msg = crate::chat::IncomingMessage {
            instance_name: "qq".into(),
            platform: "qq".into(),
            user_id: "u1".into(),
            user_name: "Alice".into(),
            channel: crate::chat::ChannelType::Group {
                group_id: "g1".into(),
            },
            group_name: Some("测试群".into()),
            content: "帮我看看这个文件".into(),
            timestamp: 1_700_000_000,
            at_me: true,
            metadata: serde_json::json!({}),
            images: vec![],
            files: vec![],
        };
        let hook = format_instance_hook(&msg, 42, 1_700_000_001_000);

        // ① 信封形状
        assert_eq!(platform_hook_of(&hook), Some("qq"));
        assert!(is_structured_input(&hook));
        // ② 序号闭环
        assert_eq!(structured_message_sequence(&hook), Some(42));
        // ③ 内容在 JSON 里（不与来源元数据混淆）
        let payload: serde_json::Value =
            serde_json::from_str(&hook_payload(&hook).expect("payload")).unwrap();
        assert_eq!(payload["content"], "帮我看看这个文件");
        assert_eq!(payload["sender"]["user_id"], "u1");
        assert_eq!(payload["at_me"], true);
        assert_eq!(payload["channel"]["type"], "group");
        assert_eq!(payload["channel"]["group_id"], "g1");
        // ④ `event` 与信封同源（都从 platform 派生）
        assert_eq!(payload["event"], "qq_message");
    }

    /// 私聊信道用 `type: "private"`，不带 group 字段。
    #[test]
    fn instance_hook_uses_private_channel_shape() {
        let msg = crate::chat::IncomingMessage {
            instance_name: "qq".into(),
            platform: "qq".into(),
            user_id: "u2".into(),
            user_name: "Bob".into(),
            channel: crate::chat::ChannelType::Direct,
            group_name: None,
            content: "hi".into(),
            timestamp: 1,
            at_me: false,
            metadata: serde_json::json!({}),
            images: vec![],
            files: vec![],
        };
        let hook = format_instance_hook(&msg, 1, 1000);
        let payload: serde_json::Value =
            serde_json::from_str(&hook_payload(&hook).expect("payload")).unwrap();
        assert_eq!(payload["channel"]["type"], "private");
        assert!(payload["channel"].get("group_id").is_none());
    }

    /// 文件附件的字段形状（`path = null` + `error` 表示下载失败）。
    #[test]
    fn instance_hook_carries_downloaded_files() {
        let msg = crate::chat::IncomingMessage {
            instance_name: "qq".into(),
            platform: "qq".into(),
            user_id: "u3".into(),
            user_name: "Carol".into(),
            channel: crate::chat::ChannelType::Direct,
            group_name: None,
            content: "看附件".into(),
            timestamp: 1,
            at_me: false,
            metadata: serde_json::json!({}),
            images: vec![],
            files: vec![
                crate::chat::IncomingFile {
                    name: "ok.pdf".into(),
                    path: Some("/tmp/ok.pdf".into()),
                    size: 1024,
                    error: None,
                },
                crate::chat::IncomingFile {
                    name: "big.zip".into(),
                    path: None,
                    size: 0,
                    error: Some("超过大小上限".into()),
                },
            ],
        };
        let hook = format_instance_hook(&msg, 2, 2000);
        let payload: serde_json::Value =
            serde_json::from_str(&hook_payload(&hook).expect("payload")).unwrap();
        assert_eq!(payload["files"][0]["path"], "/tmp/ok.pdf");
        assert_eq!(payload["files"][1]["path"], serde_json::Value::Null);
        assert_eq!(payload["files"][1]["error"], "超过大小上限");
    }

    #[test]
    fn markers_detect_structured_input() {
        assert!(is_structured_input("<qq_message_hook>{}"));
        assert!(is_structured_input("<timer_event>{}"));
        assert!(!is_structured_input("hello"));
    }

    #[test]
    fn platform_hook_is_platform_agnostic() {
        // 任何平台的标记都能识别——新增平台不需要改本模块。
        assert_eq!(platform_hook_of("<qq_message_hook>{}"), Some("qq"));
        assert_eq!(
            platform_hook_of("<telegram_message_hook>{}"),
            Some("telegram")
        );
        assert_eq!(
            platform_hook_of("<my-platform_message_hook>{}"),
            Some("my-platform")
        );
        // 固定标记不是平台 hook（`backend` 是语义保留字，不是平台名）。
        assert_eq!(platform_hook_of("<backend_message_hook>{}"), None);
        assert_eq!(platform_hook_of("<timer_event>{}"), None);
        assert!(is_structured_input("<backend_message_hook>{}"));
        // 不是标记就不认（含普通文本提到标记、空平台名、非法字符）。
        assert_eq!(platform_hook_of("note: <qq_message_hook>{}"), None);
        assert_eq!(platform_hook_of("<_message_hook>{}"), None);
        assert_eq!(platform_hook_of("<>"), None);
        assert_eq!(platform_hook_of("hello"), None);
    }

    #[test]
    fn sequence_parsed_from_structured_payload() {
        let content = "<timer_event>{\"message_sequence\":42,\"task\":\"x\"}</timer_event>";
        assert_eq!(structured_message_sequence(content), Some(42));
    }

    #[test]
    fn plain_text_never_yields_sequence() {
        assert_eq!(structured_message_sequence("const x = {n: 1};"), None);
        assert_eq!(
            structured_message_sequence(
                "note: <qq_message_hook>{\"message_sequence\":1}</qq_message_hook>"
            ),
            None,
            "marker must be at the very start"
        );
    }
}
