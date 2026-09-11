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

/// Whether `ch` may appear in a base64 payload.
fn is_base64_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '+' || ch == '/' || ch == '='
}

/// Length of the contiguous base64-alphabet run at the start of `text`.
fn base64_run_len(text: &str) -> usize {
    text.len() - text.trim_start_matches(is_base64_char).len()
}

/// Only runs this long are treated as encoded payload rather than prose:
/// short "words" made of base64-ish letters tokenize like normal text.
const MIN_BASE64_RUN: usize = 64;

/// Estimate the number of tokens a text consumes.
///
/// Conservative (deliberately over-estimates) so that token-budget trimming
/// never leaves the context over the limit:
/// - CJK characters count as 1 token each (real models average ~0.6–1).
/// - Long base64 runs count as 1 token per character. Measured on the
///   DeepSeek `/anthropic` endpoint: 40k base64 chars ≈ 28k input tokens, so
///   the old 1-per-3 rule under-counted embedded image data by >2× — the reason
///   a 1.3MB screenshot could slip past an 800k budget and hit the 1M wall.
/// - Everything else counts as 1 token per 3 characters (English is ~1/4).
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    let mut tokens = 0usize;
    let mut base64 = 0usize;
    for ch in text.chars() {
        if is_base64_char(ch) {
            base64 += 1;
            continue;
        }
        if base64 > 0 {
            if base64 >= MIN_BASE64_RUN {
                tokens += base64;
            } else {
                other += base64;
            }
            base64 = 0;
        }
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    if base64 > 0 {
        if base64 >= MIN_BASE64_RUN {
            tokens += base64;
        } else {
            other += base64;
        }
    }
    tokens + cjk + other.div_ceil(3)
}

/// Estimate the tokens one attached image costs.
///
/// Vision models bill per pixel (Claude: ≈ pixels/750; OpenAI low-detail: a
/// flat 85). Only the encoded string is available here, so the density of the
/// worst case (lossless PNG, ≈3 bytes/pixel for photos) is assumed:
/// `bytes / 250 ≈ pixels / 750`. Bounded to a sane floor and ceiling so a
/// pathological payload cannot dominate the budget on its own.
pub fn estimate_image_tokens(image: &str) -> usize {
    const MIN_IMAGE_TOKENS: usize = 85;
    const MAX_IMAGE_TOKENS: usize = 8192;
    let Some((_, body)) = image.split_once(";base64,") else {
        // Remote URL: downloaded by the endpoint, still not free.
        return MIN_IMAGE_TOKENS;
    };
    if !image.starts_with("data:") {
        return MIN_IMAGE_TOKENS;
    }
    let run = base64_run_len(body);
    if run == 0 {
        return MIN_IMAGE_TOKENS;
    }
    let bytes = run.div_ceil(4) * 3;
    (bytes / 250).clamp(MIN_IMAGE_TOKENS, MAX_IMAGE_TOKENS)
}

/// Estimate the tokens of one chat message, including the fixed per-message
/// structural overhead (role label, separators) charged by the APIs, the tool
/// call payloads an assistant message carries, and any attached images (which
/// are sent as image blocks, not as text).
pub fn estimate_message_tokens(message: &ChatMessage) -> usize {
    let mut total = estimate_tokens(&message.content)
        + message
            .reasoning_content
            .as_deref()
            .map(estimate_tokens)
            .unwrap_or_default()
        + 4;
    if let Some(calls) = &message.tool_calls {
        for call in calls {
            total += estimate_tokens(&call.name) + estimate_tokens(&call.arguments) + 8;
        }
    }
    total
        + message
            .images
            .iter()
            .map(|i| estimate_image_tokens(i))
            .sum::<usize>()
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
///
/// The cut point is found by binary search over char boundaries against
/// [`estimate_tokens`] itself, so the result obeys the same cost model — a
/// hand-rolled per-char loop desynchronises on encoded payloads (base64), which
/// is exactly how an over-budget message used to slip through.
pub fn truncate_text_to_tokens(text: &str, budget: usize) -> String {
    const MARKER: &str = "\n…[内容过长已截断]";
    if estimate_tokens(text) <= budget {
        return text.to_string();
    }
    let available = budget.saturating_sub(estimate_tokens(MARKER)).max(1);
    let mut low = 0usize; // 最长已知可保留的字符数
    let mut high = text.chars().count(); // 已知超预算的字符数
    while high - low > 1 {
        let mid = low + (high - low) / 2;
        let cut = char_boundary(text, mid);
        if estimate_tokens(&text[..cut]) <= available {
            low = mid;
        } else {
            high = mid;
        }
    }
    if low == 0 {
        // Nothing fits even without the marker — keep a single char.
        return text
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_default();
    }
    format!("{}{}", &text[..char_boundary(text, low)], MARKER)
}

/// Byte offset of the `chars`-th char (the end of the string when past it).
fn char_boundary(text: &str, chars: usize) -> usize {
    match text.char_indices().nth(chars) {
        Some((index, _)) => index,
        None => text.len(),
    }
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
