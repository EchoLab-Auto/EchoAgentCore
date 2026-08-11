//! Core 侧 TOML 配置加载（含环境变量覆盖）。
//!
//! [`CoreConfig`] — `config/echo-agent-core.toml`，Core 启动时加载
//! （`[logging]` / `[agent]` / `[adapters.qq]` / `[core]` 中的
//! management 监听地址）。支持旧的 `[server]`/`[bot]` 格式自动迁移。
//! Panel 侧配置由 EchoAgentPanel 仓库自行定义。

use std::path::Path;

use anyhow::{Context as _, Result};
use serde::Deserialize;

// ── 共享 section ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoggingSection {
    pub level: String,
    pub format: String,
    pub log_file: String,
    pub max_file_size_mb: u64,
    pub max_files: usize,
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: "text".to_string(),
            log_file: String::new(),
            max_file_size_mb: 20,
            max_files: 5,
        }
    }
}

// ── Core 配置 ──────────────────────────────────────────────────────────────

/// Core 侧配置：Agent、适配器与 management 服务。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CoreConfig {
    pub logging: LoggingSection,
    pub agent: echo_agent::AgentConfig,
    pub core: CoreSection,
    /// Legacy server section — auto-migrated to `[adapters.qq.server]`.
    pub server: ServerSection,
    /// Legacy bot section — auto-migrated to `[adapters.qq]`.
    pub bot: BotSection,
    /// New adapter configuration.
    #[serde(alias = "adapters")]
    pub adapters_section: Option<AdaptersSection>,
    /// Resolved QQ adapter config (populated in `load()`).
    #[serde(skip)]
    pub qq_adapter: echo_adapter_qq::QqAdapterConfig,
}

/// Container for the `[adapters]` TOML section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AdaptersSection {
    pub qq: Option<echo_adapter_qq::QqAdapterConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerSection {
    pub bind_address: String,
    pub access_token: String,
    pub heartbeat_interval: u64,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0:3131".to_string(),
            access_token: String::new(),
            heartbeat_interval: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BotSection {
    pub owner_qq: i64,
    pub command_prefix: String,
}

impl Default for BotSection {
    fn default() -> Self {
        Self {
            owner_qq: 0,
            command_prefix: "/".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CoreSection {
    /// WebSocket address for Panel connections (default: 127.0.0.1:3132).
    pub management_address: String,
}

impl Default for CoreSection {
    fn default() -> Self {
        Self {
            management_address: "127.0.0.1:3132".into(),
        }
    }
}

impl CoreConfig {
    /// Load the Core config file and apply environment-variable overrides.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let mut config: CoreConfig = toml::from_str(&raw)
            .with_context(|| format!("invalid config in {}", path.display()))?;

        validate_logging_level(&config.logging)?;

        // Empty command prefix would make bare words trigger command handlers unexpectedly.
        if config.bot.command_prefix.is_empty() {
            eprintln!("warning: [bot] command_prefix is empty, falling back to \"/\"");
            config.bot.command_prefix = "/".to_string();
        }

        // Deduplicate api_profiles by name (self-healing for a historical persistence bug).
        {
            let mut seen = std::collections::HashSet::new();
            config
                .agent
                .api_profiles
                .retain(|p| seen.insert(p.name.clone()));
        }

        // ---- Build QQ adapter config ----
        if let Some(ref adapters) = config.adapters_section {
            if let Some(ref qq_cfg) = adapters.qq {
                // New-format config takes priority.
                config.qq_adapter = qq_cfg.clone();
            }
        }

        // Backward compat: if [adapters.qq] is not enabled, try old [server]/[bot].
        // We only migrate if at least one explicit (non-default) value is present:
        // a non-empty access_token, a non-default heartbeat, or a non-zero owner_qq.
        // bind_address always has a default so it alone is not a sufficient signal.
        let has_explicit_server =
            !config.server.access_token.is_empty() || config.server.heartbeat_interval != 30;
        let has_explicit_bot = config.bot.owner_qq != 0;

        if !config.qq_adapter.enabled && (has_explicit_server || has_explicit_bot) {
            if has_explicit_server {
                eprintln!(
                    "note: using legacy [server] config — consider migrating to [adapters.qq.server]"
                );
            }
            config.qq_adapter.enabled = true;
            config.qq_adapter.server.bind_address = config.server.bind_address.clone();
            config.qq_adapter.server.access_token = config.server.access_token.clone();
            config.qq_adapter.server.heartbeat_interval = config.server.heartbeat_interval;
            config.qq_adapter.owner_qq = config.bot.owner_qq;
            config.qq_adapter.command_prefix = config.bot.command_prefix.clone();
        }

        // ECHO_ACCESS_TOKEN env override.
        if let Ok(token) = std::env::var("ECHO_ACCESS_TOKEN") {
            if config.qq_adapter.enabled {
                config.qq_adapter.server.access_token = token.clone();
            }
            // Also apply to legacy for consistency.
            config.server.access_token = token;
        }

        // Agent API key overrides: explicit config wins, otherwise env var.
        if config.agent.api_key.is_empty() {
            let env_var = match config.agent.provider.as_str() {
                "anthropic" | "claude" => "ANTHROPIC_API_KEY",
                _ => "OPENAI_API_KEY",
            };
            if let Ok(key) = std::env::var(env_var) {
                config.agent.api_key = key;
            }
        }

        Ok(config)
    }
}

fn validate_logging_level(logging: &LoggingSection) -> Result<()> {
    const LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];
    if !LOG_LEVELS.contains(&logging.level.as_str()) {
        anyhow::bail!(
            "[logging] level invalid: \"{}\" (options: {})",
            logging.level,
            LOG_LEVELS.join("/")
        );
    }
    if logging.max_file_size_mb == 0 {
        anyhow::bail!("[logging] max_file_size_mb must be greater than zero");
    }
    if logging.max_files == 0 {
        anyhow::bail!("[logging] max_files must be greater than zero");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_dedupes_duplicate_profiles() {
        let dir = std::env::temp_dir();
        let path = dir.join("echo-agent-dedupe-test.toml");
        std::fs::write(
            &path,
            r#"
[server]
bind_address = "0.0.0.0:3131"

[agent]
provider = "deepseek"
model = "deepseek-v4-flash"

[[agent.api_profiles]]
name = "deepseek"
provider = "deepseek"
model = "deepseek-v4-flash"
base_url = ""
api_key = ""

[[agent.api_profiles]]
name = "deepseek"
provider = "deepseek"
model = "deepseek-v4-flash"
base_url = ""
api_key = ""

[[agent.api_profiles]]
name = "deepseek"
provider = "deepseek"
model = "deepseek-v4-flash"
base_url = ""
api_key = ""
"#,
        )
        .unwrap();
        let config = CoreConfig::load(&path).unwrap();
        assert_eq!(
            config.agent.api_profiles.len(),
            1,
            "duplicate profiles should be deduped"
        );
        assert_eq!(config.agent.api_profiles[0].name, "deepseek");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn legacy_config_migrates_to_qq_adapter() {
        let dir = std::env::temp_dir();
        let path = dir.join("echo-legacy-test.toml");
        std::fs::write(
            &path,
            r#"
[server]
bind_address = "0.0.0.0:8080"
access_token = "secret"

[bot]
owner_qq = 12345
command_prefix = "!"

[agent]
provider = "openai"
model = "gpt-4o"
"#,
        )
        .unwrap();
        let config = CoreConfig::load(&path).unwrap();
        assert!(config.qq_adapter.enabled);
        assert_eq!(config.qq_adapter.server.bind_address, "0.0.0.0:8080");
        assert_eq!(config.qq_adapter.server.access_token, "secret");
        assert_eq!(config.qq_adapter.owner_qq, 12345);
        assert_eq!(config.qq_adapter.command_prefix, "!");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn new_adapter_config_takes_priority() {
        let dir = std::env::temp_dir();
        let path = dir.join("echo-new-adapter-test.toml");
        std::fs::write(
            &path,
            r#"
[server]
bind_address = "0.0.0.0:3131"

[bot]
owner_qq = 111

[agent]
provider = "openai"
model = "gpt-4o"

[adapters.qq]
enabled = true

[adapters.qq.server]
bind_address = "0.0.0.0:9999"
access_token = "new_token"
heartbeat_interval = 60

[adapters.qq.trigger]
dm_auto_reply = false
group_at_reply = true
"#,
        )
        .unwrap();
        let config = CoreConfig::load(&path).unwrap();
        assert!(config.qq_adapter.enabled);
        assert_eq!(config.qq_adapter.server.bind_address, "0.0.0.0:9999");
        assert_eq!(config.qq_adapter.server.access_token, "new_token");
        assert!(!config.qq_adapter.trigger.dm_auto_reply);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn adapter_less_config_defaults_qq_disabled() {
        let dir = std::env::temp_dir();
        let path = dir.join("echo-adapterless-test.toml");
        std::fs::write(
            &path,
            r#"
[agent]
provider = "openai"
model = "gpt-4o"
"#,
        )
        .unwrap();
        let config = CoreConfig::load(&path).unwrap();
        assert!(
            !config.qq_adapter.enabled,
            "QQ adapter should be disabled by default"
        );
        std::fs::remove_file(&path).ok();
    }
}
