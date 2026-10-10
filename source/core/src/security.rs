//! 敏感信息防护的组合根辅助（2026-10 脱敏服务装配）。
//!
//! - [`build_redactor`]：从 `[security.sanitize]` 与配置中的凭证字段构建
//!   进程级脱敏器（内置模式 + 全部已知密钥 + 用户追加值 / 模式）；
//! - [`OutboundPolicy`]：QQ 外发命中策略（`block` / `redact`）；
//! - [`sensitive_dirs`]：外发文件需要保护的敏感目录列表（配置目录、
//!   `~/.ssh` 等——`send_file` 类工具命中即拒发）。
//!
//! 脱敏器的消费点（卡口）见 `document/security-redaction-design.md`：
//! 工具结果出口、LLM 请求出口、QQ 外发出口、入站与事件落盘。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use echo_defs::sanitize::{HitKind, Redactor};
use echo_sanitize::RegistryRedactor;
use serde_json::{json, Value};

use crate::config::CoreConfig;
/// QQ 外发命中敏感信息时的策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundPolicy {
    /// 拒绝发送（默认）：给模型明确报错，不泄漏任何片段。
    Block,
    /// 替换为占位符后照发。
    Redact,
}

impl OutboundPolicy {
    /// 解析配置字符串（未知值回退 `Block` 并告警）。
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "block" => OutboundPolicy::Block,
            "redact" => OutboundPolicy::Redact,
            other => {
                tracing::warn!(
                    value = other,
                    "unknown security.sanitize.outbound policy; falling back to \"block\""
                );
                OutboundPolicy::Block
            }
        }
    }
}

/// 宿主服务适配器：把脱敏器暴露给子进程插件（HostCall `service = "sanitizer"`）。
///
/// 方法契约（与 SDK 侧 `HostClient` 配合）：
/// - `redact {text}` → `{text, changed, hits}`（hits = 命中部数）；
/// - `scan {text}` → `{count, hits:[{label, kind}]}`（kind: registered/pattern）；
/// - `register_secret {value, label}` → `{registered}`（插件登记自己的凭证）。
pub struct SanitizerHostService {
    redactor: Arc<dyn Redactor>,
}

impl SanitizerHostService {
    /// 以共享脱敏器构造（进程级同一实例）。
    pub fn new(redactor: Arc<dyn Redactor>) -> Self {
        Self { redactor }
    }
}

impl echo_plugin_host::HostService for SanitizerHostService {
    fn call(&self, method: &str, payload: Value) -> Result<Value, (String, String)> {
        fn required_text(payload: &Value) -> Result<String, (String, String)> {
            payload
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    (
                        "invalid_payload".to_string(),
                        "此方法需要 {text: string}".to_string(),
                    )
                })
        }
        match method {
            "redact" => {
                let report = self.redactor.redact(&required_text(&payload)?);
                Ok(json!({
                    "text": report.text,
                    "changed": report.changed,
                    "hits": report.hits.len(),
                }))
            }
            "scan" => {
                let hits: Vec<Value> = self
                    .redactor
                    .scan(&required_text(&payload)?)
                    .into_iter()
                    .map(|hit| {
                        json!({
                            "label": hit.label,
                            "kind": match hit.kind {
                                HitKind::Registered => "registered",
                                HitKind::Pattern => "pattern",
                            },
                        })
                    })
                    .collect();
                Ok(json!({ "count": hits.len(), "hits": hits }))
            }
            "register_secret" => {
                let value = payload
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        (
                            "invalid_payload".to_string(),
                            "register_secret 需要 {value, label}".to_string(),
                        )
                    })?;
                let label = payload
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("plugin");
                let before = self.redactor.registered_count();
                self.redactor.register_secret(value, label);
                Ok(json!({ "registered": self.redactor.registered_count() > before }))
            }
            other => Err((
                "method_not_found".to_string(),
                format!("unknown sanitizer method: {other}"),
            )),
        }
    }
}

/// 构建进程级脱敏器：内置保守模式 + 全部已知密钥 + 用户追加配置。
///
/// 登记来源：
/// - `[agent]` 顶层 `api_key` 与 `[agent.api_profiles.*]` 的 `api_key`；
/// - env 回退（`DEEPSEEK_API_KEY` / `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`
///   ——`effective_api_key` 的同口径）；
/// - QQ 适配器 `access_token`（OneBot 正/反向 WS 鉴权用）；
/// - `[security.sanitize]` 的 `extra_secrets`（label `extra:<序号>`）与
///   `extra_patterns`（名字 → regex）。
pub fn build_redactor(cfg: &CoreConfig) -> Arc<dyn Redactor> {
    let redactor = RegistryRedactor::with_builtin_patterns();
    let agent = &cfg.agent;
    if !agent.api_key.trim().is_empty() {
        redactor.register_secret(&agent.api_key, "api_key");
    }
    for profile in &agent.api_profiles {
        if !profile.api_key.trim().is_empty() {
            redactor.register_secret(&profile.api_key, &format!("api_key:{}", profile.name));
        }
    }
    for var in ["DEEPSEEK_API_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        if let Ok(value) = std::env::var(var) {
            if !value.trim().is_empty() {
                redactor.register_secret(&value, &format!("env:{var}"));
            }
        }
    }

    // QQ OneBot 服务端 access_token（正/反向 WS 鉴权；结构见
    // echo_adapter_qq::QqServerConfig）。
    if !cfg.qq_adapter.server.access_token.trim().is_empty() {
        redactor.register_secret(&cfg.qq_adapter.server.access_token, "qq:access_token");
    }
    for (index, value) in cfg.security.sanitize.extra_secrets.iter().enumerate() {
        redactor.register_secret(value, &format!("extra:{index}"));
    }
    for (name, pattern) in &cfg.security.sanitize.extra_patterns {
        if let Err(error) = redactor.register_pattern(name, pattern) {
            tracing::warn!(
                pattern = name,
                %error,
                "security.sanitize.extra_patterns: pattern rejected"
            );
        }
    }
    tracing::info!(
        entries = redactor.registered_count(),
        "sanitize: redactor ready (builtin patterns + config secrets)"
    );
    Arc::new(redactor)
}

/// 外发文件需要保护的敏感目录（`send_file` / `send_image` / `send_voice`
/// 的本地路径参数命中即拒发）。
///
/// 列表 = 配置所在目录（core.toml / 会话 / 存档都在这）+ 常见凭据目录。
/// 与"同 uid 可读"的威胁模型一致：允许读到文件内容（做不到阻止），
/// 但阻止**整文件外发**这一最直接的外泄形态。
pub fn sensitive_dirs(config_path: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(parent) = config_path.parent() {
        dirs.push(parent.to_path_buf());
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        for relative in [
            ".config/echo-agent-core",
            ".config/echo-agent-panel",
            ".ssh",
            ".gnupg",
            ".aws",
        ] {
            dirs.push(home.join(relative));
        }
    }
    dirs
}

/// 判断一个参数值是否是落在敏感目录内的本地文件路径。
///
/// 非路径形态（URL / base64 / data URI / 相对路径）返回 `None`。
/// 存在时优先 `canonicalize`（解析符号链接）；不存在时按词法规范化。
pub fn sensitive_local_path(raw: &str, protected: &[PathBuf]) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("base64://")
        || trimmed.starts_with("data:")
    {
        return None;
    }
    let path = if let Some(rest) = trimmed.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME")?).join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    if !path.is_absolute() {
        return None;
    }
    let resolved = path
        .canonicalize()
        .unwrap_or_else(|_| normalize_lexically(&path));
    protected
        .iter()
        .find(|dir| resolved.starts_with(dir))
        .cloned()
}

/// 词法路径规范化（不触盘）：折叠 `.` 与 `..`（`..` 在根处忽略）。
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_policy_parses_and_falls_back() {
        assert_eq!(OutboundPolicy::from_config(""), OutboundPolicy::Block);
        assert_eq!(OutboundPolicy::from_config("block"), OutboundPolicy::Block);
        assert_eq!(
            OutboundPolicy::from_config("REDACT"),
            OutboundPolicy::Redact
        );
        assert_eq!(
            OutboundPolicy::from_config("nonsense"),
            OutboundPolicy::Block
        );
    }

    /// 宿主服务适配器：子进程插件经 HostCall 调用的 redact / scan / 错误码。
    #[test]
    fn sanitizer_host_service_contract() {
        use echo_defs::sanitize::Redactor;
        use echo_plugin_host::HostService;
        const KEY: &str = "sk-hostservicehostservice01";
        let redactor = Arc::new(RegistryRedactor::new());
        redactor.register_secret(KEY, "api_key:test");
        let service = SanitizerHostService::new(redactor);

        let redacted = service
            .call("redact", json!({"text": format!("x {KEY} y")}))
            .expect("redact ok");
        assert_eq!(redacted["changed"], true);
        assert!(!redacted["text"].as_str().unwrap().contains(KEY));

        let scanned = service.call("scan", json!({"text": KEY})).expect("scan ok");
        assert_eq!(scanned["count"], 1);
        assert_eq!(scanned["hits"][0]["kind"], "registered");

        let registered = service
            .call(
                "register_secret",
                json!({"value": "plugin-secret-12345678", "label": "plugin:demo"}),
            )
            .expect("register ok");
        assert_eq!(registered["registered"], true);

        let missing = service.call("redact", json!({})).unwrap_err();
        assert_eq!(missing.0, "invalid_payload");
        let unknown = service
            .call("no_such_method", json!({"text": "x"}))
            .unwrap_err();
        assert_eq!(unknown.0, "method_not_found");
    }

    #[test]
    fn sensitive_local_path_matches_protected_dirs_only() {
        let protected = vec![PathBuf::from("/tmp/echo-protected")];
        assert!(sensitive_local_path("/tmp/echo-protected/core.toml", &protected).is_some());
        assert!(sensitive_local_path("/tmp/echo-protected/sub/../core.toml", &protected).is_some());
        assert!(sensitive_local_path("/tmp/other/file.txt", &protected).is_none());
        assert!(sensitive_local_path("https://example.com/x", &protected).is_none());
        assert!(sensitive_local_path("relative/file.txt", &protected).is_none());
        assert!(sensitive_local_path("", &protected).is_none());
    }

    #[test]
    fn build_redactor_registers_configured_secrets() {
        let mut cfg = CoreConfig::default();
        cfg.agent.api_key = "sk-configsecretsk-configsecret".into();
        cfg.security.sanitize.extra_secrets = vec!["corp-extra-secret-1".into()];
        cfg.security
            .sanitize
            .extra_patterns
            .insert("corp_id".into(), r"corp2-[A-Z0-9]{8}".into());
        let redactor = build_redactor(&cfg);
        let report = redactor.redact(
            "key=sk-configsecretsk-configsecret extra=corp-extra-secret-1 id=corp2-ABCDEFGH",
        );
        assert!(report.changed);
        assert!(!report.text.contains("sk-configsecret"));
        assert!(!report.text.contains("corp-extra-secret-1"));
        assert!(report.text.contains("【已隐藏:extra:0】"));
        assert!(report.text.contains("【已隐藏:pattern:corp_id】"));
    }
}
