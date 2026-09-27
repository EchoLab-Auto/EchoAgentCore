//! Agent configuration.

use serde::{Deserialize, Serialize};

// `ThinkingMode` / `ReasoningEffort` are part of the frontend wire contract
// and are defined in the standalone `echo-protocol` crate; re-exported here
// so existing `echo_agent::config::…` paths keep working.
/// 循环模式（single/parallel）——单一事实来源在 echo-defs，这里随 config 再导出，
/// 便于 core 等消费方引用 `echo_agent::config::LoopMode`（无需直接依赖定义层）。
pub use echo_defs::LoopMode;
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

/// A team member (`[agent.teams.{id}]`, legacy `[agent.profiles.{id}]`).
///
/// Each member is instantiated as an independent `Agent` with its own
/// trunk memory, session log and system prompt. Version 1: shared
/// provider/model; per-member LLM config is Phase 2.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TeamMember {
    /// Display name (e.g. "写作助理").
    pub name: String,
    /// Short description shown in the Panel agent picker.
    #[serde(default)]
    pub description: String,
    /// Persona system prompt (replaces the global prompt for this agent).
    #[serde(default)]
    pub system_prompt: String,
    /// Whether to instantiate this agent at startup (runtime toggling
    /// persists separately via `disabled_agents`).
    pub enabled: bool,
    /// 该 agent 系统提示词由哪些 skill 组成（名字引用，热重载即生效）。
    /// 非空时优先于 system_prompt 字段；为空回退 system_prompt（兼容）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_skills: Vec<String>,
    /// DEPRECATED（2026-09-11 移除）：per-persona 插件黑名单。
    /// 白名单（`enabled_plugins`）已能表达全部语义（空表 = 全部启用，前端
    /// 首次取消勾选即物化全量），黑名单不再参与任何门控判定、也不再写回
    /// 配置；字段仅保留反序列化能力，供加载期迁移
    /// （[`crate::plugins::convert_plugin_blacklist_to_whitelist`] 把它物化进
    /// 白名单，既有配置行为不变）。
    #[serde(default, skip_serializing)]
    pub disabled_plugins: Vec<String>,
    /// Per-persona disabled tools (by tool name, e.g. "bash").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_tools: Vec<String>,
    /// Per-persona disabled skills (by skill name).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_skills: Vec<String>,
    /// Per-persona **allowlist** of plugins: when non-empty, ONLY these
    /// plugins are enabled for this agent (everything else is hidden).
    /// Empty = all plugins enabled（唯一名单；黑名单已移除）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_plugins: Vec<String>,
    /// Per-persona allowlist of tools: non-empty = only these tools visible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_tools: Vec<String>,
    /// Per-persona allowlist of skills: non-empty = only these skills active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_skills: Vec<String>,
    /// Per-persona trunk token budget. `None` = 继承全局
    /// `[agent].memory_limit_tokens`（或默认 800k）。每个 agent 的上下文
    /// 裁剪互不影响。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit_tokens: Option<usize>,
    /// Per-persona model context window cap. `None` = 继承全局
    /// `[agent].context_window_tokens`。设置后该 agent 的有效预算被
    /// `window × 0.8` 封顶。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<usize>,
    /// Persona 级 API 供应商引用：`Some(name)` = 使用全局供应商池
    /// （`[agent].api_profiles`）中该名字的 profile（运行期即时生效，
    /// 重建该 persona 自己的 provider）；`None` = 跟随全局默认配置
    /// （顶层 + active_api）。供应商池、默认配置与 profile 的增删改
    /// 都在设置视图「API」分类维护；本字段只做引用，不内嵌值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_profile: Option<String>,
}

impl TeamMember {
    /// 循环模式推导（互斥插件 `echo-agent.loop.{single,parallel}`，
    /// single 为兜底，也是默认）：
    /// - 白名单含 `loop.parallel`（或旧编排模式 id）→ Parallel
    /// - 其余（含白名单为空 = 默认）→ Single
    ///
    /// 插件黑名单已移除（2026-09-11），本推导只看白名单。
    pub fn loop_mode(&self) -> echo_defs::LoopMode {
        let listed = self
            .enabled_plugins
            .iter()
            .any(|p| crate::plugins::PARALLEL_MODE_IDS.iter().any(|id| p == id));
        if listed {
            echo_defs::LoopMode::Parallel
        } else {
            echo_defs::LoopMode::Single
        }
    }
}

impl Default for TeamMember {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            system_prompt: String::new(),
            enabled: true,
            system_skills: Vec::new(),
            disabled_plugins: Vec::new(),
            disabled_tools: Vec::new(),
            disabled_skills: Vec::new(),
            enabled_plugins: Vec::new(),
            enabled_tools: Vec::new(),
            enabled_skills: Vec::new(),
            memory_limit_tokens: None,
            context_window_tokens: None,
            api_profile: None,
        }
    }
}

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
    ///
    /// In Core this is owned by `[plugins.system_prompt]` and therefore
    /// excluded from generic agent-config serialization.
    #[serde(skip_serializing)]
    pub system_prompt: String,
    /// Maximum number of tool-call iterations in one agent turn.
    pub max_tool_iterations: usize,
    /// 单次请求输出预算：0 = 无上限（后端回退到 128K 实用上限）。
    pub max_tokens: usize,
    /// Per-tool-call timeout in seconds. A tool that exceeds this is aborted
    /// and the LLM receives an error result. `None` disables the timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_secs: Option<u64>,
    /// Directory containing `SKILL.md` definitions.
    pub skills_dir: String,
    /// Directory containing external plugin manifests (`plugin.toml`).
    /// Discovered data plugins (skills/tools) hot-reload here.
    #[serde(default)]
    pub plugins_dir: String,
    /// Skills disabled at runtime (survives restarts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_skills: Vec<String>,
    /// Tools disabled at runtime (survives restarts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_tools: Vec<String>,
    /// Plugins disabled at runtime (survives restarts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_plugins: Vec<String>,
    /// Team members. Empty = single default agent (legacy behavior).
    #[serde(default)]
    pub teams: std::collections::BTreeMap<String, TeamMember>,
    /// Team members disabled at runtime (survives restarts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_teams: Vec<String>,
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
            max_tokens: 0,
            tool_timeout_secs: Some(120),
            skills_dir: "skills".into(),
            plugins_dir: "plugins".into(),
            disabled_skills: Vec::new(),
            disabled_tools: Vec::new(),
            disabled_plugins: Vec::new(),
            teams: std::collections::BTreeMap::new(),
            disabled_teams: Vec::new(),
            memory_limit: 40,
            memory_limit_tokens: None,
            context_window_tokens: None,
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

    /// 将**指定名字**的 profile 值合入顶层字段（persona 级 API 用）。
    /// 语义与 [`Self::apply_active_profile`] 一致（非空覆盖），但不依赖
    /// `active_api`；找不到该 profile 时返回 false（配置保持不变）。
    pub fn apply_named_profile(&mut self, name: &str) -> bool {
        let Some(p) = self.api_profiles.iter().find(|p| p.name == name).cloned() else {
            return false;
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
        true
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
    ///   单次请求输出预算：0 = 无上限（None）；否则 Some(value)。
    ///   与 Anthropic/OpenAI 兼容端点的默认行为对齐（None 时后端回退 128K）。
    pub fn effective_max_tokens(&self) -> Option<u32> {
        if self.max_tokens == 0 {
            None
        } else {
            Some(self.max_tokens.min(u32::MAX as usize) as u32)
        }
    }

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

/// Back-compat alias: TeamMember was previously named AgentProfile.
pub type AgentProfile = TeamMember;

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
    fn team_member_budget_roundtrips_through_toml() {
        let mut cfg = AgentConfig::default();
        cfg.teams.insert(
            "alix".into(),
            TeamMember {
                name: "Alix".into(),
                memory_limit_tokens: Some(100_000),
                context_window_tokens: Some(64_000),
                ..Default::default()
            },
        );
        let toml_str = toml::to_string(&cfg).unwrap();
        assert!(toml_str.contains("memory_limit_tokens = 100000"));
        assert!(toml_str.contains("context_window_tokens = 64000"));
        let parsed: AgentConfig = toml::from_str(&toml_str).unwrap();
        let member = &parsed.teams["alix"];
        assert_eq!(member.name, "Alix");
        assert_eq!(member.memory_limit_tokens, Some(100_000));
        assert_eq!(member.context_window_tokens, Some(64_000));
    }

    #[test]
    fn team_member_budget_defaults_to_inherit_global() {
        // 未配置预算字段时 None 不序列化；反序列化后仍为 None（继承全局）。
        let cfg = AgentConfig::default();
        let toml_str = toml::to_string(&cfg).unwrap();
        assert!(!toml_str.contains("memory_limit_tokens"));
        let member = TeamMember::default();
        assert_eq!(member.memory_limit_tokens, None);
        assert_eq!(member.context_window_tokens, None);
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

    // ── 循环模式推导（loop_mode）──

    fn member_with_plugins(enabled: &[&str], disabled: &[&str]) -> TeamMember {
        TeamMember {
            enabled_plugins: enabled.iter().map(|s| s.to_string()).collect(),
            disabled_plugins: disabled.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn loop_mode_matrix() {
        use echo_defs::LoopMode::*;

        use crate::plugins::{
            LEGACY_CHATBOT_MODE_IDS, PARALLEL_LOOP_PLUGIN_ID, SINGLE_LOOP_PLUGIN_ID,
        };
        let parallel = PARALLEL_LOOP_PLUGIN_ID;
        let single = SINGLE_LOOP_PLUGIN_ID;
        // 空白名单（=默认）→ Single（单会话是默认，非"全部启用"）
        assert_eq!(member_with_plugins(&[], &[]).loop_mode(), Single);
        // 仅 parallel → Parallel；仅 single → Single
        assert_eq!(member_with_plugins(&[parallel], &[]).loop_mode(), Parallel);
        assert_eq!(member_with_plugins(&[single], &[]).loop_mode(), Single);
        // 任一旧编排模式 id → Parallel（向后兼容推导）
        for id in LEGACY_CHATBOT_MODE_IDS {
            assert_eq!(
                member_with_plugins(&[id], &[]).loop_mode(),
                Parallel,
                "legacy id {id} should derive parallel"
            );
        }
        // 旧 single id → Single
        assert_eq!(
            member_with_plugins(&["echo-agent.orchestration.single"], &[]).loop_mode(),
            Single
        );
        // single + parallel 并含 → Parallel（互斥优先）
        assert_eq!(
            member_with_plugins(&[single, parallel], &[]).loop_mode(),
            Parallel
        );
        // 非空白名单但无任何模式 id → Single（兜底）
        assert_eq!(
            member_with_plugins(
                &["echo-agent.tools.builtin", "echo-agent.orchestration"],
                &[]
            )
            .loop_mode(),
            Single
        );
        // 已废弃的插件黑名单字段不再影响推导（2026-09-11 移除，白名单单轨）：
        // 黑名单即使含 parallel/旧 id，也只看白名单。
        assert_eq!(member_with_plugins(&[], &[parallel]).loop_mode(), Single);
        assert_eq!(
            member_with_plugins(&[parallel], &[parallel]).loop_mode(),
            Parallel
        );
        assert_eq!(
            member_with_plugins(&[parallel], &[LEGACY_CHATBOT_MODE_IDS[1]]).loop_mode(),
            Parallel
        );
    }

    #[test]
    fn team_member_blacklist_is_deserialization_only() {
        // 反序列化仍接受旧字段（供加载期迁移读取），但序列化不再写回。
        let toml_str = r#"
disabled_plugins = ["echo-agent.tools.builtin"]
enabled_plugins = ["echo-agent.adapter.qq"]
"#;
        let member: TeamMember = toml::from_str(toml_str).unwrap();
        assert_eq!(
            member.disabled_plugins,
            vec!["echo-agent.tools.builtin".to_string()]
        );
        let out = toml::to_string(&member).unwrap();
        assert!(!out.contains("disabled_plugins"), "serialized: {out}");
    }
}
