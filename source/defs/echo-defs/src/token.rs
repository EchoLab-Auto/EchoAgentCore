//! Pure token-estimation and truncation helpers.
//!
//! Conservative (deliberately over-estimates) so that token-budget trimming
//! never leaves the context over the limit. These are pure functions with no
//! I/O, shared by the session projection and prompt assembly.

use crate::message::ChatMessage;

/// Char-safe truncation for error messages (avoids panicking mid-UTF-8).
pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// Whether a character is a CJK character (conservative superset: Han
/// ideographs, radicals, kana, CJK punctuation, full-width forms).
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x2E80..=0x303F   // radicals + CJK punctuation
        | 0x3040..=0x30FF // kana
        | 0x31C0..=0x31EF // CJK strokes
        | 0x3400..=0x9FFF // unified ideographs
        | 0xF900..=0xFAFF // compatibility ideographs
        | 0xFE30..=0xFE4F // compatibility forms
        | 0xFF00..=0xFFEF // full-width forms
        | 0x20000..=0x2FA1F // extension A–F
    )
}

/// Estimate the number of tokens a text consumes.
///
/// Conservative (deliberately over-estimates) so that token-budget trimming
/// never leaves the context over the limit:
/// - CJK characters count as 1 token each (real models average ~0.6–1).
/// - Everything else counts as 1 token per 3 characters (English is ~1/4).
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(3)
}

/// Estimate the tokens of one chat message, including the fixed per-message
/// structural overhead (role label, separators) charged by the APIs.
pub fn estimate_message_tokens(message: &ChatMessage) -> usize {
    estimate_tokens(&message.content)
        + message
            .reasoning_content
            .as_deref()
            .map(estimate_tokens)
            .unwrap_or_default()
        + 4
}

/// Estimate the total tokens of a history slice.
pub fn estimate_history_tokens(history: &[ChatMessage]) -> usize {
    history.iter().map(estimate_message_tokens).sum()
}

/// Truncate `text` so its estimated token count stays at or below `budget`.
///
/// Keeps the head of the text (structured inputs carry their sequence at the
/// start) and appends a truncation marker. When the original already fits, the
/// original is returned unchanged.
pub fn truncate_text_to_tokens(text: &str, budget: usize) -> String {
    const MARKER: &str = "\n…[内容过长已截断]";
    if estimate_tokens(text) <= budget {
        return text.to_string();
    }
    let marker_tokens = estimate_tokens(MARKER);
    let available = budget.saturating_sub(marker_tokens).max(1);
    let mut cost = 0usize;
    let mut non_cjk = 0usize;
    let mut cut = text.len();
    for (index, ch) in text.char_indices() {
        let inc = if is_cjk(ch) {
            1
        } else {
            non_cjk += 1;
            if non_cjk % 3 == 1 {
                1
            } else {
                0
            }
        };
        if cost + inc > available {
            cut = index;
            break;
        }
        cost += inc;
    }
    if cut == 0 {
        // Nothing fits even without the marker — keep a single char.
        return text
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_default();
    }
    format!("{}{}", &text[..cut], MARKER)
}

/// Truncate a single message to fit `budget` estimated tokens (content only).
pub fn truncate_message_to_tokens(message: &mut ChatMessage, budget: usize) {
    let current = estimate_message_tokens(message);
    if current <= budget {
        return;
    }
    // Content budget after accounting for the 4-token structural overhead and
    // any reasoning content.
    let reasoning_budget = message
        .reasoning_content
        .as_deref()
        .map(estimate_tokens)
        .unwrap_or(0);
    let content_budget = budget.saturating_sub(4 + reasoning_budget).max(1);
    message.content = truncate_text_to_tokens(&message.content, content_budget);
}
