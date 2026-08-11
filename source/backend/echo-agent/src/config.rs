//! Agent configuration.

use serde::{Deserialize, Serialize};

// `ThinkingMode` / `ReasoningEffort` are part of the frontend wire contract
// and are defined in the standalone `echo-protocol` crate; re-exported here
// so existing `echo_agent::config::…` paths keep working.
pub use echo_protocol::{ReasoningEffort, ThinkingMode};

/// A named LLM API profile (multi-API storage, switchable from the TUI).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ApiProfile {
    /// 配置名，如 "openai-main" / "deepseek"。
    pub name: String,
    pub provider: String,
    pub model: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// DeepSeek thinking mode. Defaults to enabled for old profiles.
    #[serde(default)]
    pub thinking: ThinkingMode,
    /// DeepSeek reasoning effort. Defaults to max for old profiles.
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
}

/// Policy for the privileged framework self-update tool.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SelfUpdateConfig {
    /// Register the `framework_update` tool. Disabled for source/dev runs.
    pub enabled: bool,
    /// Allow requests originating from the local TUI session.
    pub allow_local: bool,
    /// Additional QQ user IDs allowed to request an update.
    pub allowed_qq_users: Vec<u64>,
}

impl Default for SelfUpdateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_local: true,
            allowed_qq_users: Vec::new(),
        }
    }
}

impl ApiProfile {
    pub fn new(
        name: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            provider: provider.into(),
            model: model.into(),
            base_url: String::new(),
            api_key: String::new(),
            thinking: ThinkingMode::default(),
            reasoning_effort: ReasoningEffort::default(),
        }
    }
}

/// Default trunk token budget: 1M × 0.8 = 800,000 tokens.
pub const DEFAULT_MEMORY_LIMIT_TOKENS: usize = 1_000_000 * 8 / 10;

/// Configuration for the agent framework (the `[agent]` TOML section).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentConfig {
    /// 顶层默认 provider / model / base_url / api_key（active_api 为空时生效）。
    pub provider: String,
    pub model: String,
    pub base_url: String,
    /// Excluded from generic serialization. Agent persistence preserves the
    /// on-disk value or writes an explicitly supplied replacement.
    #[serde(skip_serializing, default)]
    pub api_key: String,
    /// DeepSeek thinking mode (enabled by default).
    pub thinking: ThinkingMode,
    /// DeepSeek reasoning effort (max by default).
    pub reasoning_effort: ReasoningEffort,
    /// 多 API 配置列表。
    pub api_profiles: Vec<ApiProfile>,
    /// 当前激活的 profile 名；为空使用顶层字段。
    pub active_api: String,
    /// System prompt describing the bot's personality and rules.
    pub system_prompt: String,
    /// Maximum number of tool-call iterations in one agent turn.
    pub max_tool_iterations: usize,
    /// Per-tool-call timeout in seconds. A tool that exceeds this is aborted
    /// and the LLM receives an error result. `None` disables the timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_secs: Option<u64>,
    /// Directory containing `SKILL.md` definitions.
    pub skills_dir: String,
    /// DEPRECATED — legacy message-count limit. Kept only for config
    /// compatibility (deserialization); no longer participates in any
    /// calculation. Use `memory_limit_tokens` instead.
    pub memory_limit: usize,
    /// Token budget for the global conversation trunk (the context fed to the
    /// LLM). `None` falls back to [`DEFAULT_MEMORY_LIMIT_TOKENS`]
    /// (1M × 0.8 = 800,000 tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit_tokens: Option<usize>,
    /// Model context window in tokens. When set, the effective trunk budget is
    /// capped at `window × 0.8` so the request always fits the model window
    /// (leaving room for the system prompt and the completion). `None` keeps
    /// the trunk budget unclamped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<usize>,
    /// Privileged framework update policy.
    pub self_update: SelfUpdateConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            // 默认 API 配置为空，由用户通过 TUI /api 或配置文件提供。
            provider: String::new(),
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            thinking: ThinkingMode::default(),
            reasoning_effort: ReasoningEffort::default(),
            api_profiles: Vec::new(),
            active_api: String::new(),
            system_prompt: "You are a helpful assistant. Respond concisely and clearly.".into(),
            max_tool_iterations: 5,
            tool_timeout_secs: Some(120),
            skills_dir: "skills".into(),
            memory_limit: 40,
            memory_limit_tokens: None,
            context_window_tokens: None,
            self_update: SelfUpdateConfig::default(),
        }
    }
}

impl AgentConfig {
    /// 激活的 profile（`active_api` 命中的那个）。
    pub fn active_profile(&self) -> Option<&ApiProfile> {
        if self.active_api.is_empty() {
            return None;
        }
        self.api_profiles.iter().find(|p| p.name == self.active_api)
    }

    /// 将激活 profile 的值合入顶层字段。只有非空值才会覆盖顶层。
    pub fn apply_active_profile(&mut self) {
        let Some(p) = self.active_profile().cloned() else {
            return;
        };
        if !p.provider.is_empty() {
            self.provider = p.provider;
        }
        if !p.model.is_empty() {
            self.model = p.model;
        }
        if !p.base_url.is_empty() {
            self.base_url = p.base_url;
        }
        if !p.api_key.is_empty() {
            self.api_key = p.api_key;
        }
        self.thinking = p.thinking;
        self.reasoning_effort = p.reasoning_effort;
    }

    /// Resolve the effective API key: explicit config, then env var by provider.
    pub fn effective_api_key(&self) -> String {
        if !self.api_key.is_empty() {
            return self.api_key.clone();
        }
        let env_var = match self.provider.as_str() {
            "anthropic" | "claude" => "ANTHROPIC_API_KEY",
            "deepseek" => "DEEPSEEK_API_KEY",
            "ollama" => "",
            _ => "OPENAI_API_KEY",
        };
        if env_var.is_empty() {
            String::new()
        } else {
            std::env::var(env_var).unwrap_or_else(|_| {
                // deepseek 也允许用通用的 OPENAI_API_KEY
                if env_var == "DEEPSEEK_API_KEY" {
                    std::env::var("OPENAI_API_KEY").unwrap_or_default()
                } else {
                    String::new()
                }
            })
        }
    }

    /// Effective base URL, falling back to the provider default.
    pub fn effective_base_url(&self) -> String {
        if !self.base_url.is_empty() {
            return self.base_url.clone();
        }
        match self.provider.as_str() {
            "anthropic" | "claude" => "https://api.anthropic.com".into(),
            "deepseek" => "https://api.deepseek.com".into(),
            "ollama" => "http://localhost:11434".into(),
            _ => "https://api.openai.com/v1".into(),
        }
    }

    /// Resolve the effective token budget for trunk trimming.
    ///
    /// - Explicit `memory_limit_tokens` wins (minimum 200 tokens).
    /// - Otherwise the default budget is [`DEFAULT_MEMORY_LIMIT_TOKENS`]
    ///   (1M × 0.8 = 800,000 tokens). The legacy `memory_limit`
    ///   message-count field is deprecated and no longer participates.
    /// - When `context_window_tokens` is set, the result is capped at
    ///   `window × 0.8` so the prompt never exceeds the model window.
    pub fn effective_memory_limit_tokens(&self) -> usize {
        let limit = if let Some(tokens) = self.memory_limit_tokens {
            tokens.max(200)
        } else {
            DEFAULT_MEMORY_LIMIT_TOKENS
        };
        match self.context_window_tokens {
            Some(window) if window > 0 => limit.min(window * 8 / 10).max(200),
            _ => limit,
        }
    }

    /// Effective per-tool timeout (seconds), defaulting to 120 when unset.
    pub fn effective_tool_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.tool_timeout_secs.unwrap_or(120))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_profile(name: &str, provider: &str, model: &str) -> AgentConfig {
        let mut cfg = AgentConfig::default();
        cfg.api_profiles
            .push(ApiProfile::new(name, provider, model));
        cfg
    }

    #[test]
    fn active_profile_returns_none_when_empty() {
        let cfg = AgentConfig::default();
        assert!(cfg.active_profile().is_none());
        assert_eq!(cfg.thinking, ThinkingMode::Enabled);
        assert_eq!(cfg.reasoning_effort, ReasoningEffort::Max);
    }

    #[test]
    fn legacy_config_defaults_to_enabled_max_reasoning() {
        let cfg: AgentConfig = toml::from_str(
            r#"
                provider = "deepseek"
                model = "deepseek-v4-flash"
                [[api_profiles]]
                name = "legacy"
                provider = "deepseek"
                model = "deepseek-v4-flash"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.thinking, ThinkingMode::Enabled);
        assert_eq!(cfg.reasoning_effort, ReasoningEffort::Max);
        assert_eq!(cfg.api_profiles[0].thinking, ThinkingMode::Enabled);
        assert_eq!(cfg.api_profiles[0].reasoning_effort, ReasoningEffort::Max);
    }

    #[test]
    fn active_profile_finds_by_name() {
        let mut cfg = cfg_with_profile("deepseek", "deepseek", "ds-v4");
        cfg.active_api = "deepseek".into();
        let p = cfg.active_profile().expect("profile found");
        assert_eq!(p.provider, "deepseek");
        assert_eq!(p.model, "ds-v4");
    }

    #[test]
    fn active_profile_miss_returns_none() {
        let mut cfg = cfg_with_profile("a", "openai", "m");
        cfg.active_api = "missing".into();
        assert!(cfg.active_profile().is_none());
    }

    #[test]
    fn apply_active_profile_merges_nonempty_values() {
        let mut cfg = AgentConfig {
            provider: "openai".into(),
            model: "old-model".into(),
            ..Default::default()
        };
        cfg.api_profiles.push(ApiProfile {
            name: "p".into(),
            provider: "deepseek".into(),
            model: "new-model".into(),
            base_url: String::new(), // empty — must not override
            api_key: "sk".into(),
            thinking: ThinkingMode::Disabled,
            reasoning_effort: ReasoningEffort::High,
        });
        cfg.active_api = "p".into();
        cfg.apply_active_profile();
        assert_eq!(cfg.provider, "deepseek");
        assert_eq!(cfg.model, "new-model");
        assert_eq!(cfg.base_url, ""); // untouched
        assert_eq!(cfg.api_key, "sk");
        assert_eq!(cfg.thinking, ThinkingMode::Disabled);
        assert_eq!(cfg.reasoning_effort, ReasoningEffort::High);
    }

    #[test]
    fn apply_active_profile_noop_without_active() {
        let mut cfg = cfg_with_profile("p", "deepseek", "m");
        cfg.apply_active_profile();
        assert!(cfg.provider.is_empty(), "no active profile → no merge");
    }

    #[test]
    fn effective_api_key_prefers_explicit_value() {
        let cfg = AgentConfig {
            api_key: "explicit".into(),
            ..Default::default()
        };
        assert_eq!(cfg.effective_api_key(), "explicit");
    }

    #[test]
    fn effective_api_key_empty_without_env() {
        // No env var is set in tests — must resolve to empty, not panic.
        let cfg = AgentConfig {
            provider: "openai".into(),
            ..Default::default()
        };
        assert_eq!(cfg.effective_api_key(), "");
    }

    #[test]
    fn effective_base_url_uses_provider_defaults() {
        for (provider, expected) in [
            ("deepseek", "https://api.deepseek.com"),
            ("ollama", "http://localhost:11434"),
            ("anthropic", "https://api.anthropic.com"),
            ("anything-else", "https://api.openai.com/v1"),
        ] {
            let cfg = AgentConfig {
                provider: provider.into(),
                ..Default::default()
            };
            assert_eq!(cfg.effective_base_url(), expected, "provider {provider}");
        }
    }

    #[test]
    fn effective_base_url_prefers_explicit() {
        let cfg = AgentConfig {
            base_url: "http://custom:8080".into(),
            ..Default::default()
        };
        assert_eq!(cfg.effective_base_url(), "http://custom:8080");
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let mut cfg = AgentConfig {
            provider: "deepseek".into(),
            model: "deepseek-v4-flash".into(),
            ..Default::default()
        };
        cfg.api_profiles
            .push(ApiProfile::new("p1", "openai", "gpt-4o"));
        let toml_str = toml::to_string(&cfg).unwrap();
        let parsed: AgentConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.provider, "deepseek");
        assert_eq!(parsed.api_profiles.len(), 1);
        assert_eq!(parsed.api_profiles[0].name, "p1");
        // api_key is skipped in serialisation but defaults to empty on load.
        assert!(parsed.api_key.is_empty());
    }

    #[test]
    fn self_update_is_disabled_by_default() {
        let config = AgentConfig::default();
        assert!(!config.self_update.enabled);
        assert!(config.self_update.allow_local);
        assert!(config.self_update.allowed_qq_users.is_empty());
    }

    #[test]
    fn legacy_shared_context_is_ignored_and_not_serialized() {
        let config: AgentConfig = toml::from_str("shared_context = true").unwrap();
        let serialized = toml::to_string(&config).unwrap();
        assert!(!serialized.contains("shared_context"));
    }

    #[test]
    fn effective_tokens_default_is_1m_times_0_8() {
        assert_eq!(DEFAULT_MEMORY_LIMIT_TOKENS, 800_000);
        assert_eq!(DEFAULT_MEMORY_LIMIT_TOKENS, 1_000_000 * 8 / 10);
        let default = AgentConfig::default();
        assert_eq!(default.effective_memory_limit_tokens(), 800_000);
        // Legacy message-count field no longer participates in the calculation.
        let legacy = AgentConfig {
            memory_limit: 20,
            ..Default::default()
        };
        assert_eq!(legacy.effective_memory_limit_tokens(), 800_000);
    }

    #[test]
    fn effective_tokens_floor_is_200() {
        let cfg = AgentConfig {
            memory_limit: 0,
            memory_limit_tokens: Some(0),
            ..Default::default()
        };
        assert_eq!(cfg.effective_memory_limit_tokens(), 200);
    }

    #[test]
    fn context_window_clamps_effective_budget() {
        // Explicit 800k budget + 128k window → capped at 128k × 0.8.
        let cfg = AgentConfig {
            memory_limit_tokens: Some(800_000),
            context_window_tokens: Some(128_000),
            ..Default::default()
        };
        assert_eq!(cfg.effective_memory_limit_tokens(), 102_400);

        // Window larger than the budget → budget wins.
        let cfg = AgentConfig {
            memory_limit_tokens: Some(10_000),
            context_window_tokens: Some(1_000_000),
            ..Default::default()
        };
        assert_eq!(cfg.effective_memory_limit_tokens(), 10_000);

        // No window → default budget.
        let cfg = AgentConfig {
            context_window_tokens: None,
            ..Default::default()
        };
        assert_eq!(cfg.effective_memory_limit_tokens(), 800_000);

        // Zero window is ignored (treat as unset).
        let cfg = AgentConfig {
            context_window_tokens: Some(0),
            ..Default::default()
        };
        assert_eq!(cfg.effective_memory_limit_tokens(), 800_000);

        // Tiny window still respects the 200-token floor.
        let cfg = AgentConfig {
            context_window_tokens: Some(100),
            ..Default::default()
        };
        assert_eq!(cfg.effective_memory_limit_tokens(), 200);
    }

    #[test]
    fn context_window_and_tool_timeout_roundtrip_through_toml() {
        let cfg = AgentConfig {
            context_window_tokens: Some(131_072),
            tool_timeout_secs: Some(30),
            ..Default::default()
        };
        let toml_str = toml::to_string(&cfg).unwrap();
        assert!(toml_str.contains("context_window_tokens = 131072"));
        assert!(toml_str.contains("tool_timeout_secs = 30"));
        let parsed: AgentConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.context_window_tokens, Some(131_072));
        assert_eq!(parsed.tool_timeout_secs, Some(30));
        let default_str = toml::to_string(&AgentConfig::default()).unwrap();
        assert!(!default_str.contains("context_window_tokens"));
    }

    #[test]
    fn tool_timeout_defaults_to_120_seconds() {
        let cfg = AgentConfig::default();
        assert_eq!(
            cfg.effective_tool_timeout(),
            std::time::Duration::from_secs(120)
        );
        let cfg = AgentConfig {
            tool_timeout_secs: Some(5),
            ..Default::default()
        };
        assert_eq!(
            cfg.effective_tool_timeout(),
            std::time::Duration::from_secs(5)
        );
    }

    #[test]
    fn memory_limit_tokens_roundtrips_through_toml() {
        let cfg = AgentConfig {
            memory_limit_tokens: Some(10_000),
            ..Default::default()
        };
        let toml_str = toml::to_string(&cfg).unwrap();
        assert!(toml_str.contains("memory_limit_tokens = 10000"));
        let parsed: AgentConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.memory_limit_tokens, Some(10_000));
        // None is not serialized at all.
        let none_str = toml::to_string(&AgentConfig::default()).unwrap();
        assert!(!none_str.contains("memory_limit_tokens"));
    }
}
