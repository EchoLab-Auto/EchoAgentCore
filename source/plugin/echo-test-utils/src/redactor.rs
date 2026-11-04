//! A minimal in-memory `Redactor` for tests that need to *exercise* the
//! sanitize seams (tool-result gate, outbound gate, …) without depending on a
//! concrete redactor implementation.
//!
//! ## Why this exists (2026-11)
//!
//! `echo-tool` / `echo-plugin-tools` used to instantiate
//! `echo_sanitize::RegistryRedactor` directly in their `#[cfg(test)]` modules.
//! Two problems with that:
//!
//! 1. `echo-sanitize` no longer exists as a crate — its implementation moved
//!    into `echo-plugin-sanitize` (its only production consumer was that
//!    plugin's `build()`).
//! 2. Even before the move it was a smell: a kernel/plugin crate's tests
//!    reaching for **one specific implementation** means the seam
//!    (`echo_defs::sanitize::Redactor`) was not actually what was under test.
//!
//! This mock keeps the same *observable contract* the tests rely on
//! (`【已隐藏:<label>】` placeholders, leftmost-longest selection, registered
//! values winning over pattern hits, idempotence) with ~80 lines and zero
//! regex/automaton dependencies.
//!
//! It is **not** a replacement for [`echo_plugin_sanitize::RegistryRedactor`]
//! in production — it only understands the fixtures it is handed plus one
//! builtin `sk-…` shape.

use echo_defs::sanitize::{HitKind, PatternError, RedactReport, Redactor, SecretHit};

/// Placeholder shape (must stay byte-identical to the real implementation —
/// several tests assert on it literally).
pub fn placeholder(label: &str) -> String {
    format!("【已隐藏:{label}】")
}

/// Builtin pattern name emitted for `sk-…`-shaped hits.
const BUILTIN_SK_LABEL: &str = "pattern:openai_key";

/// Registered literals are matched verbatim; anything shorter is ignored,
/// mirroring the real implementation's "too short = mass false positives" rule.
const MIN_SECRET_CHARS: usize = 8;

#[derive(Default)]
struct Inner {
    secrets: Vec<(String, String)>,
    /// `true` → also match the builtin `sk-…` shape (>= 20 chars after `sk-`).
    builtin_sk: bool,
}

/// Minimal `Redactor` for tests.
pub struct FakeRedactor {
    inner: std::sync::RwLock<Inner>,
}

impl FakeRedactor {
    /// Empty registry, no builtin patterns (literal matching only).
    pub fn new() -> Self {
        Self {
            inner: std::sync::RwLock::new(Inner::default()),
        }
    }

    /// Registry + the builtin `sk-…` shape.
    pub fn with_builtin_patterns() -> Self {
        Self {
            inner: std::sync::RwLock::new(Inner {
                secrets: Vec::new(),
                builtin_sk: true,
            }),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for FakeRedactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Redactor for FakeRedactor {
    fn name(&self) -> &str {
        "fake"
    }

    fn redact(&self, text: &str) -> RedactReport {
        let hits = self.scan(text);
        if hits.is_empty() {
            return RedactReport {
                text: text.to_string(),
                changed: false,
                hits,
            };
        }
        let mut out = String::with_capacity(text.len() + hits.len() * 16);
        let mut cursor = 0usize;
        for hit in &hits {
            out.push_str(&text[cursor..hit.span.0]);
            out.push_str(&placeholder(&hit.label));
            cursor = hit.span.1;
        }
        out.push_str(&text[cursor..]);
        RedactReport {
            text: out,
            changed: true,
            hits,
        }
    }

    fn scan(&self, text: &str) -> Vec<SecretHit> {
        let inner = self.read();
        // (start, end, label, kind)
        let mut raw: Vec<(usize, usize, String, HitKind)> = Vec::new();

        for (value, label) in &inner.secrets {
            if value.len() < MIN_SECRET_CHARS {
                continue;
            }
            for (start, _) in text.match_indices(value.as_str()) {
                raw.push((
                    start,
                    start + value.len(),
                    label.clone(),
                    HitKind::Registered,
                ));
            }
        }
        if inner.builtin_sk {
            let mut offset = 0usize;
            while let Some(found) = text[offset..].find("sk-") {
                let start = offset + found;
                let body_start = start + 3;
                let body_len = text[body_start..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                    .unwrap_or(text.len() - body_start);
                if body_len >= 20 {
                    raw.push((
                        start,
                        body_start + body_len,
                        BUILTIN_SK_LABEL.to_string(),
                        HitKind::Pattern,
                    ));
                }
                offset = body_start + body_len.max(1);
            }
        }

        // 最左 → 同起点最长 → 注册值优先（与真实实现的选取规则一致）。
        raw.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| rank(a.3).cmp(&rank(b.3)))
        });
        let mut selected = Vec::new();
        let mut last_end = 0usize;
        for (start, end, label, kind) in raw {
            if start >= last_end {
                last_end = end;
                selected.push(SecretHit {
                    label,
                    kind,
                    span: (start, end),
                });
            }
        }
        selected
    }

    fn register_secret(&self, value: &str, label: &str) {
        if value.len() < MIN_SECRET_CHARS || value.contains("【已隐藏:") {
            return;
        }
        let mut inner = self.write();
        if inner.secrets.iter().any(|(v, _)| v == value) {
            return;
        }
        inner.secrets.push((value.to_string(), label.to_string()));
    }

    fn register_pattern(&self, name: &str, pattern: &str) -> Result<(), PatternError> {
        // Only the builtin `sk-…` shape is supported by this mock.
        if name.is_empty() || name.contains(char::is_whitespace) || name.contains(':') {
            return Err(PatternError::InvalidName(name.to_string()));
        }
        let sk_shaped = pattern.contains("sk-");
        if !sk_shaped {
            return Err(PatternError::InvalidRegex(format!(
                "FakeRedactor 只认识 sk- 形态的内置模式，收到 {name:?}"
            )));
        }
        self.write().builtin_sk = true;
        Ok(())
    }

    fn registered_count(&self) -> usize {
        let inner = self.read();
        inner.secrets.len() + usize::from(inner.builtin_sk)
    }
}

fn rank(kind: HitKind) -> u8 {
    match kind {
        HitKind::Registered => 0,
        HitKind::Pattern => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `sk-`-shaped fixture at runtime so this file contains no
    /// literal that a secret scanner (or the redaction pipeline reading it)
    /// would flag.
    fn sk_fixture(body_len: usize) -> String {
        format!("sk-{}", "a".repeat(body_len))
    }

    #[test]
    fn registered_secret_is_replaced_with_its_label() {
        let redactor = FakeRedactor::new();
        redactor.register_secret("ghp_abcdefghijklmnop", "gh_token");
        let report = redactor.redact("token ghp_abcdefghijklmnop here");
        assert!(report.changed);
        assert_eq!(report.text, "token 【已隐藏:gh_token】 here");
    }

    #[test]
    fn builtin_sk_pattern_applies_without_registration() {
        let redactor = FakeRedactor::with_builtin_patterns();
        let secret = sk_fixture(24);
        let report = redactor.redact(&format!("key={secret} end"));
        assert!(report.changed);
        assert_eq!(
            report.text,
            format!("key={} end", placeholder("pattern:openai_key"))
        );
    }

    #[test]
    fn registered_value_wins_over_the_builtin_pattern() {
        let redactor = FakeRedactor::with_builtin_patterns();
        let secret = sk_fixture(24);
        redactor.register_secret(&secret, "api_key:test");
        let report = redactor.redact(&secret);
        assert_eq!(report.text, placeholder("api_key:test"));
        assert_eq!(report.hits[0].kind, HitKind::Registered);
    }

    #[test]
    fn short_values_are_ignored_and_redaction_is_idempotent() {
        let redactor = FakeRedactor::new();
        redactor.register_secret("short", "x");
        assert_eq!(redactor.registered_count(), 0);
        redactor.register_secret("ghp_abcdefghijklmnop", "gh_token");
        let once = redactor.redact("ghp_abcdefghijklmnop").text;
        assert_eq!(once, redactor.redact(&once).text, "占位符不得被二次替换");
    }

    #[test]
    fn no_hit_leaves_text_byte_identical() {
        let redactor = FakeRedactor::with_builtin_patterns();
        let report = redactor.redact("nothing to see here");
        assert!(!report.changed);
        assert_eq!(report.text, "nothing to see here");
        assert!(report.hits.is_empty());
    }
}
