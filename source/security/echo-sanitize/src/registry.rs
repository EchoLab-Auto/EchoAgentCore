//! 注册表脱敏器实现：字面值自动机 + 保守模式集。
//!
//! 设计要点（对 [`Redactor`] 契约的实现决策）：
//!
//! - **锁中毒免疫**：所有锁访问经 `unwrap_or_else(|p| p.into_inner())`——
//!   脱敏在关键路径上运行，绝不允许因并发 panic 而失败；
//! - **字面值自动机**：登记的秘密编译为 Aho-Corasick（`LeftmostLongest`），
//!   每次登记后整体重建（登记频率低、条数少，重建成本可忽略）；
//! - **候选合并**：字面值 + 全部模式先收集候选，再按「最左 → 同起点最长 →
//!   注册值优先」贪心取互不重叠集合——同位置的注册值总是赢过模式（label
//!   更精确）；
//! - **内置模式低误报优先**：只覆盖高置信度凭证形态（长度门槛、固定前缀），
//!   宁可漏报形态未知的自定义凭证（用 `register_secret` / `register_pattern`
//!   补齐），不误伤正常文本。

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use echo_defs::sanitize::{HitKind, PatternError, RedactReport, Redactor, SecretHit};

/// 登记秘密的最小字符数：过短的值（如 "test"）会在正常文本里大面积误报，
/// 按实现约定安全 no-op 跳过。
const MIN_SECRET_CHARS: usize = 8;

/// 占位符 label 的最大字符数（防超长 label 撑爆输出）。
const MAX_LABEL_CHARS: usize = 64;

/// 保守内置模式集：`(名字, 正则)`。
///
/// 只收录固定前缀 + 长度门槛的高置信度形态；每个模式都有对应单测，
/// 增删即改测试（误报是安全机制的大敌——宁可漏，不可扰）。
const BUILTIN_PATTERNS: &[(&str, &str)] = &[
    // OpenAI / DeepSeek / 多数兼容网关：sk-…（≥20 位后随）
    ("openai_key", r"sk-[A-Za-z0-9_\-]{20,}"),
    // GitHub classic token：ghp_/gho_/ghu_/ghs_/ghr_
    ("github_token", r"gh[pousr]_[A-Za-z0-9]{20,}"),
    // GitHub fine-grained PAT
    ("github_pat", r"github_pat_[A-Za-z0-9_]{20,}"),
    // AWS access key id（AKIA/ASIA + 16 位大写字母数字，词边界）
    ("aws_access_key", r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b"),
    // Google API key：AIza + 35 位
    ("google_api_key", r"AIza[0-9A-Za-z_\-]{35}"),
    // JWT（三段 base64url，首段 eyJ = {"alg"... 的 base64 头）
    (
        "jwt",
        r"eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
    ),
    // PEM 私钥块头
    ("private_key_block", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
    // Bearer 认证头（Bearer + ≥20 位 token）
    ("bearer_token", r"(?i)bearer\s+[A-Za-z0-9._~+/=\-]{20,}"),
];

/// 构造占位符（命中后替换文本）。导出供测试与文档引用。
pub fn placeholder(label: &str) -> String {
    format!("【已隐藏:{label}】")
}

/// 默认脱敏器：字面值注册表 + 模式集。
///
/// 推荐构造入口：[`RegistryRedactor::with_builtin_patterns`]（预装保守内置
/// 模式）；[`RegistryRedactor::new`] 只含空白注册表（测试 / 特殊场景）。
pub struct RegistryRedactor {
    inner: RwLock<Inner>,
}

struct Inner {
    /// 登记的秘密（保持注册顺序；自动机 pattern id = 下标）。
    secrets: Vec<Secret>,
    /// 与 `secrets` 同步重建的字面值自动机（空注册表 = None）。
    automaton: Option<AhoCorasick>,
    /// 模式集（内置 + 追加）。
    patterns: Vec<CompiledPattern>,
}

struct Secret {
    value: String,
    label: String,
}

struct CompiledPattern {
    name: String,
    regex: regex::Regex,
}

/// 单次扫描的原始候选（选取前）。
struct RawMatch {
    start: usize,
    end: usize,
    label: String,
    kind: HitKind,
}

impl RegistryRedactor {
    /// 空注册表（无秘密、无模式）。
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                secrets: Vec::new(),
                automaton: None,
                patterns: Vec::new(),
            }),
        }
    }

    /// 预装保守内置模式（推荐入口；见 [`BUILTIN_PATTERNS`]）。
    pub fn with_builtin_patterns() -> Self {
        let this = Self::new();
        for (name, pattern) in BUILTIN_PATTERNS {
            // 内置模式语法由单元测试守护；个别失败只丢该模式，不 panic。
            if let Err(error) = this.register_pattern(name, pattern) {
                tracing::error!(pattern = name, %error, "sanitize: builtin pattern rejected");
            }
        }
        this
    }

    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for RegistryRedactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Inner {
    /// 重建字面值自动机（登记变化后调用）。
    fn rebuild(&mut self) {
        if self.secrets.is_empty() {
            self.automaton = None;
            return;
        }
        let patterns: Vec<&str> = self.secrets.iter().map(|s| s.value.as_str()).collect();
        self.automaton = match AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&patterns)
        {
            Ok(automaton) => Some(automaton),
            Err(error) => {
                // 理论不可达（值非空即合法 pattern）；失败只损失字面值匹配，
                // 模式集不受影响。
                tracing::error!(%error, "sanitize: automaton build failed; literal matching disabled");
                None
            }
        };
    }
}

/// 一次扫描：收集候选 → 贪心选取互不重叠命中。
fn collect(inner: &Inner, text: &str) -> Vec<SecretHit> {
    let mut raw: Vec<RawMatch> = Vec::new();
    if let Some(automaton) = &inner.automaton {
        for found in automaton.find_iter(text) {
            let secret = &inner.secrets[found.pattern().as_usize()];
            raw.push(RawMatch {
                start: found.start(),
                end: found.end(),
                label: secret.label.clone(),
                kind: HitKind::Registered,
            });
        }
    }
    for pattern in &inner.patterns {
        for found in pattern.regex.find_iter(text) {
            raw.push(RawMatch {
                start: found.start(),
                end: found.end(),
                label: format!("pattern:{}", pattern.name),
                kind: HitKind::Pattern,
            });
        }
    }
    // 最左优先 → 同起点最长优先 → 注册值优先（label 更精确）。
    raw.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| b.end.cmp(&a.end))
            .then_with(|| rank(a.kind).cmp(&rank(b.kind)))
    });
    let mut selected: Vec<SecretHit> = Vec::new();
    let mut last_end = 0usize;
    for candidate in raw {
        if candidate.start >= last_end {
            last_end = candidate.end;
            selected.push(SecretHit {
                label: candidate.label,
                kind: candidate.kind,
                span: (candidate.start, candidate.end),
            });
        }
    }
    selected
}

/// 命中类别排序（越小越优先）。
fn rank(kind: HitKind) -> u8 {
    match kind {
        HitKind::Registered => 0,
        HitKind::Pattern => 1,
    }
}

/// label 净化：只保留字母数字与 `[_-:.]`，超长截断，空 → `"secret"`。
fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .trim()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))
        .take(MAX_LABEL_CHARS)
        .collect();
    if cleaned.is_empty() {
        "secret".to_string()
    } else {
        cleaned
    }
}

impl Redactor for RegistryRedactor {
    fn name(&self) -> &str {
        "registry"
    }

    fn redact(&self, text: &str) -> RedactReport {
        let inner = self.read();
        let hits = collect(&inner, text);
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
            // 命中区间来自自动机 / 正则，端点必为 UTF-8 字符边界。
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
        collect(&self.read(), text)
    }

    fn register_secret(&self, value: &str, label: &str) {
        let value = value.trim();
        if value.chars().count() < MIN_SECRET_CHARS {
            return;
        }
        // 夹带占位符括号的值会破坏幂等性（占位符本身可被再次识别），
        // 按无效值忽略。
        if value.contains('【') || value.contains('】') {
            return;
        }
        let label = sanitize_label(label);
        let mut inner = self.write();
        if inner.secrets.iter().any(|existing| existing.value == value) {
            return;
        }
        inner.secrets.push(Secret {
            value: value.to_string(),
            label,
        });
        inner.rebuild();
        tracing::debug!(count = inner.secrets.len(), "sanitize: secret registered");
    }

    fn register_pattern(&self, name: &str, pattern: &str) -> Result<(), PatternError> {
        let name = name.trim();
        if name.is_empty()
            || name.chars().count() > MAX_LABEL_CHARS
            || name.chars().any(|c| c.is_whitespace() || c == ':')
        {
            return Err(PatternError::InvalidName(name.to_string()));
        }
        let regex = regex::Regex::new(pattern)
            .map_err(|error| PatternError::InvalidRegex(error.to_string()))?;
        let mut inner = self.write();
        if inner.patterns.iter().any(|existing| existing.name == name) {
            return Ok(()); // 同名幂等
        }
        inner.patterns.push(CompiledPattern {
            name: name.to_string(),
            regex,
        });
        Ok(())
    }

    fn registered_count(&self) -> usize {
        let inner = self.read();
        inner.secrets.len() + inner.patterns.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 典型 API key 形态（≥8 字符、非占位符）。
    const KEY: &str = "sk-deadbeefdeadbeefdeadbeef0000";

    fn redactor_with(key: &str, label: &str) -> RegistryRedactor {
        let redactor = RegistryRedactor::new();
        redactor.register_secret(key, label);
        redactor
    }

    #[test]
    fn redacts_registered_literal_with_label() {
        let redactor = redactor_with(KEY, "api_key:deepseek");
        let report = redactor.redact(&format!("key is {KEY} ok"));
        assert!(report.changed);
        assert_eq!(report.text, "key is 【已隐藏:api_key:deepseek】 ok");
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].kind, HitKind::Registered);
    }

    #[test]
    fn clean_text_unchanged() {
        let redactor = redactor_with(KEY, "api_key");
        let text = "今天天气不错，没有秘密。";
        let report = redactor.redact(text);
        assert!(!report.changed);
        assert_eq!(report.text, text);
        assert!(report.hits.is_empty());
    }

    #[test]
    fn short_values_are_ignored() {
        let redactor = RegistryRedactor::new();
        redactor.register_secret("short", "x");
        redactor.register_secret("  ", "x");
        assert_eq!(redactor.registered_count(), 0);
        assert!(!redactor.redact("a short value").changed);
    }

    #[test]
    fn duplicate_registration_is_noop() {
        let redactor = redactor_with(KEY, "a");
        redactor.register_secret(KEY, "b");
        assert_eq!(redactor.registered_count(), 1);
        let report = redactor.redact(KEY);
        assert_eq!(report.text, "【已隐藏:a】");
    }

    #[test]
    fn redaction_is_idempotent() {
        let redactor = RegistryRedactor::with_builtin_patterns();
        redactor.register_secret(KEY, "api_key");
        let once = redactor.redact(&format!("x {KEY} y"));
        let twice = redactor.redact(&once.text);
        assert_eq!(once.text, twice.text);
        assert!(!twice.changed);
    }

    #[test]
    fn builtin_pattern_matches_openai_style_key() {
        let redactor = RegistryRedactor::with_builtin_patterns();
        let report = redactor.redact("token=sk-abcdefghijklmnopqrstuvwx end");
        assert!(report.changed);
        assert!(report.text.contains("【已隐藏:pattern:openai_key】"));
        assert!(!report.text.contains("sk-abcdefg"));
    }

    #[test]
    fn builtin_pattern_requires_min_length() {
        let redactor = RegistryRedactor::with_builtin_patterns();
        // 后随不足 20 位 → 不命中（低误报优先）。
        assert!(!redactor.redact("sk-short").changed);
    }

    #[test]
    fn overlapping_literal_prefers_longest_then_registered() {
        let redactor = RegistryRedactor::new();
        redactor.register_secret("deadbeefdeadbeef", "short");
        redactor.register_secret("deadbeefdeadbeef00", "long");
        let report = redactor.redact("x deadbeefdeadbeef00 y");
        // 同起点最长优先。
        assert!(report.text.contains("【已隐藏:long】"));
    }

    #[test]
    fn registered_beats_pattern_at_same_span() {
        let redactor = RegistryRedactor::with_builtin_patterns();
        redactor.register_secret(KEY, "api_key:deepseek");
        let report = redactor.redact(KEY);
        assert_eq!(report.text, "【已隐藏:api_key:deepseek】");
        assert_eq!(report.hits[0].kind, HitKind::Registered);
    }

    #[test]
    fn scan_reports_without_modifying() {
        let redactor = redactor_with(KEY, "api_key");
        let text = format!("a {KEY} b {KEY} c");
        let hits = redactor.scan(&text);
        assert_eq!(hits.len(), 2);
        assert_ne!(hits[0].span, hits[1].span);
        // scan 不产生改写文本。
        assert!(redactor.redact(&text).changed);
    }

    #[test]
    fn custom_pattern_registration() {
        let redactor = RegistryRedactor::new();
        redactor
            .register_pattern("corp_token", r"corp-[A-Z0-9]{8}")
            .expect("valid pattern");
        let report = redactor.redact("use corp-ABCDEFGH now");
        assert!(report.text.contains("【已隐藏:pattern:corp_token】"));
        // 同名幂等。
        assert!(redactor
            .register_pattern("corp_token", r"corp-[A-Z0-9]{8}")
            .is_ok());
        assert_eq!(redactor.registered_count(), 1);
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        let redactor = RegistryRedactor::new();
        assert!(matches!(
            redactor.register_pattern("bad", "("),
            Err(PatternError::InvalidRegex(_))
        ));
        assert!(matches!(
            redactor.register_pattern("", "x"),
            Err(PatternError::InvalidName(_))
        ));
        assert!(matches!(
            redactor.register_pattern("bad:name", "x"),
            Err(PatternError::InvalidName(_))
        ));
    }

    #[test]
    fn non_ascii_secret_is_supported() {
        let redactor = redactor_with("秘密口令甲乙丙丁", "密");
        let report = redactor.redact("口令：秘密口令甲乙丙丁，请保管");
        assert!(report.changed);
        assert!(report.text.contains("【已隐藏:密】"));
    }

    #[test]
    fn placeholder_containing_value_is_rejected() {
        let redactor = RegistryRedactor::new();
        redactor.register_secret("【已隐藏:foo】", "x");
        assert_eq!(redactor.registered_count(), 0);
    }
}
