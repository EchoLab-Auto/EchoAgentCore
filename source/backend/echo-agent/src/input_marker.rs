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
//! from whatever platform string an adapter reports, and detection accepts any
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
/// Single entry point for building structured hook inputs — adapter bridges
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

/// Wrap a payload in the timer-event envelope.
pub fn wrap_timer(payload: impl std::fmt::Display) -> String {
    format!("{TIMER_EVENT_OPEN}{payload}{TIMER_EVENT_CLOSE}")
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_wrap_roundtrip() {
        let wrapped = wrap_hook("qq", r#"{"a":1}"#);
        assert!(wrapped.starts_with("<qq_message_hook>"));
        assert!(wrapped.ends_with("</qq_message_hook>"));
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
