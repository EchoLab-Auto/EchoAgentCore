//! Structured input markers — the single source of truth for the
//! `<platform_message_hook>` wire convention.
//!
//! The agent loop receives inbound platform messages wrapped in a marked
//! envelope (`<qq_message_hook>JSON</qq_message_hook>`), and backend events
//! (timers, background-task completions) similarly. These markers carry the
//! structured payload's provenance and `message_sequence`; the envelope keeps
//! the actual content JSON-escaped so it can never be confused with source
//! metadata.
//!
//! Everything that constructs, detects, or parses these envelopes lives here —
//! one definition, no scattered literals. (Protocol-level structuring of this
//! convention is tracked as Phase 5.5 bundled work; this module is its
//! consolidation step.)

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

/// The timer-event envelope opener.
pub const TIMER_EVENT_OPEN: &str = "<timer_event>";

/// The timer-event envelope closer.
pub const TIMER_EVENT_CLOSE: &str = "</timer_event>";

/// The background-task-event envelope opener.
pub const BACKGROUND_EVENT_OPEN: &str = "<background_task_event>";

/// The background-task-event envelope closer.
pub const BACKGROUND_EVENT_CLOSE: &str = "</background_task_event>";

/// Wrap a payload in the timer-event envelope.
pub fn wrap_timer(payload: impl std::fmt::Display) -> String {
    format!("{TIMER_EVENT_OPEN}{payload}{TIMER_EVENT_CLOSE}")
}

/// Wrap a payload in the background-task-event envelope.
pub fn wrap_background(payload: impl std::fmt::Display) -> String {
    format!("{BACKGROUND_EVENT_OPEN}{payload}{BACKGROUND_EVENT_CLOSE}")
}

/// The known structured input markers that carry a `message_sequence`.
///
/// Sequence parsing is deliberately restricted to these markers so ordinary
/// conversation text that happens to contain braces (JSON snippets, code,
/// math) can never be misread as structured input.
pub const STRUCTURED_INPUT_MARKERS: [&str; 4] = [
    "<qq_message_hook>",
    "<backend_message_hook>",
    TIMER_EVENT_OPEN,
    BACKGROUND_EVENT_OPEN,
];

/// Whether `content` starts with any known structured input marker.
pub fn is_structured_input(content: &str) -> bool {
    STRUCTURED_INPUT_MARKERS
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
    serde_json::from_str::<serde_json::Value>(&payload).ok()?["message_sequence"]
        .as_u64()
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
