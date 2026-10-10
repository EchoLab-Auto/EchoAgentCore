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

// ── Plugins ────────────────────────────────────────────────────────────────

/// Core 插件配置。系统提示词是第一个插件化注入项。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PluginsSection {
    pub system_prompt: SystemPromptPlugin,
}

/// 系统提示词插件：`text` 为空时不做基础系统提示词注入。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SystemPromptPlugin {
    pub text: String,
}

impl Default for SystemPromptPlugin {
    fn default() -> Self {
        Self {
            text: "请你使用中文".to_string(),
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
    pub plugins: PluginsSection,
    pub core: CoreSection,
    /// Core↔Core 联邦链路（Phase 1；缺省关闭）。
    pub federation: FederationSection,
    /// New adapter configuration.
    #[serde(alias = "adapters")]
    pub adapters_section: Option<AdaptersSection>,
    /// Resolved QQ adapter config (populated in `load()`).
    #[serde(skip)]
    pub qq_adapter: echo_adapter_qq::QqAdapterConfig,
    /// QQ 实例表（多实例；缺省 = 单实例 `qq`）。
    #[serde(skip)]
    pub qq_instances: std::collections::BTreeMap<String, QqInstanceSection>,
    /// `[security]`：敏感信息防护（脱敏服务，2026-10）。
    pub security: SecuritySection,
}

/// `[security]`：敏感信息防护总节。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SecuritySection {
    pub sanitize: SanitizeSection,
}

/// `[security.sanitize]`：脱敏服务（默认开启）。
///
/// 三层出口卡口（工具结果 / LLM 请求 / QQ 外发）+ 入站与事件落盘脱敏，
/// 详见 `document/security-redaction-design.md`。`enabled=false` 需要重启
/// 生效，且**不再提供运行期一键开关**（安全不变量：脱敏器不可被模型或
/// 误操作禁用）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SanitizeSection {
    /// 是否启用脱敏（缺省 true）。
    pub enabled: bool,
    /// QQ 出站命中策略：`"block"`（缺省，拒绝发送 + 报错）或
    /// `"redact"`（替换为占位符后照发）。
    pub outbound: String,
    /// 追加登记的精确值（label 自动编号 `extra:<序号>`）。
    pub extra_secrets: Vec<String>,
    /// 追加检测正则：名字 → regex（命中替换为 `【已隐藏:pattern:<名字>】`）。
    pub extra_patterns: std::collections::BTreeMap<String, String>,
}

impl Default for SanitizeSection {
    fn default() -> Self {
        Self {
            enabled: true,
            outbound: "block".to_string(),
            extra_secrets: Vec::new(),
            extra_patterns: std::collections::BTreeMap::new(),
        }
    }
}

/// Container for the `[adapters]` TOML section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AdaptersSection {
    pub qq: Option<QqSection>,
}

/// `[adapters.qq]`：共享默认 + 多实例表。
///
/// - 顶层字段是**共享默认值**（镜像、auto_start、路径模板…）；
/// - `instances.<id>` 覆盖单实例差异（persona 归属、端口、owner…）。
/// - 未配置 `instances` 时按「默认值 → 唯一实例 `qq`」解析（legacy 行为）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QqSection {
    #[serde(flatten)]
    pub shared: echo_adapter_qq::QqAdapterConfig,
    /// 实例表：实例 id → 覆盖项。
    pub instances: std::collections::BTreeMap<String, QqInstanceSection>,
}

/// 单个 QQ 实例的配置覆盖项（缺省继承 `[adapters.qq]` 共享默认）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QqInstanceSection {
    /// 归属人格 id（缺省 = 迁移期的旧单实例语义：第一个已启用人格）。
    pub persona: Option<String>,
    /// 该实例独立端口（多容器多通道）；缺省由分配器填写并持久化。
    pub ports: QqPorts,
    /// 仅覆盖需要差异化的子字段（None = 继承共享默认）。
    pub server: Option<echo_adapter_qq::QqServerConfig>,
    pub owner_qq: Option<i64>,
    pub napcat_container: Option<String>,
    pub napcat_webui_url: Option<String>,
    pub napcat_onebot_url: Option<String>,
    pub napcat_auto_start: Option<bool>,
    pub napcat_auto_stop: Option<bool>,
    /// 该实例是否启用（缺省跟随共享默认的 `enabled`）。
    pub enabled: Option<bool>,
}

/// QQ 实例的宿主端口三元组（自动分配并持久化）。
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct QqPorts {
    /// 反向 WS 监听端口（NapCat 连回 Core）。
    pub reverse_ws: u16,
    /// OneBot HTTP API 宿主端口。
    pub onebot_http: u16,
    /// NapCat WebUI 宿主端口。
    pub webui: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CoreSection {
    /// WebSocket address for Panel connections (default: 0.0.0.0:3132——
    /// 开箱即可被局域网 panel 连接；token 认证（首装自动生成）兜底安全。
    pub management_address: String,
    /// Optional bearer token for the management WebSocket. Empty preserves
    /// localhost-only legacy behavior; set this when exposing Core remotely.
    pub management_access_token: String,
    /// 运行区域的人类可读名（agent 的「运行区域」属性）。留空 = 回退
    /// `[federation].node_name` → 主机名 → `NodeId` 短码。区域 id 恒为 NodeId。
    pub region_name: String,
}

impl Default for CoreSection {
    fn default() -> Self {
        Self {
            management_address: "0.0.0.0:3132".into(),
            management_access_token: String::new(),
            region_name: String::new(),
        }
    }
}

/// `[federation]` 段（federation Phase 1）：Core↔Core 对等链路。
///
/// **零配置默认开启**（2026-10）：缺省监听 `0.0.0.0:3133`——任何 Core 开箱
/// 即可连出也可被连入（邀请串配对开箱可用，无需先手写 listen）；未配
/// peer 时不接受任何已知链路（accept 侧靠 per-peer token 认证）。显式
/// `listen = ""` 退回纯连出。一旦配置 `[federation.peers.*]`，互信边界即
/// 生效（≈ SSH 免密），务必用 per-peer token 并限制
/// `allow_tools`/`allow_queries`。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FederationSection {
    /// 联邦监听地址（缺省 `0.0.0.0:3133`；显式 `""` = 不监听纯连出）。
    pub listen: String,
    /// 邀请串**对外宣告**的地址（`host:port`；缺省 `""`）。
    ///
    /// 多网卡 / VPN 场景（如内网网卡 + tun0/WireGuard 叠加）下，`listen`
    /// 为 `0.0.0.0` 时无法推导"对端该连哪个地址"——缺省会取 `hostname -I`
    /// 首个非回环 IPv4，可能正是对端不可达的那个网卡。此处显式指定对端
    /// 可达地址（可省略端口 → 采用 `listen` 端口；IPv6 用 `[addr]:port`）。
    /// 纯对外连出（`listen = ""`）时忽略（无监听则邀请无意义）。
    pub advertise: String,
    /// 人类可读节点别名（Hello 中携带；缺省仅 node_id）。
    pub node_name: Option<String>,
    /// 静态对等节点表。
    pub peers: std::collections::BTreeMap<String, FederationPeerSection>,
}

impl Default for FederationSection {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:3133".into(),
            advertise: String::new(),
            node_name: None,
            peers: std::collections::BTreeMap::new(),
        }
    }
}

/// `[federation.peers.<name>]` 条目。
///
/// `allow_tools`/`require_confirm`/`allow_queries`/`allow_subagent` 均已在
/// 执行端消费（Phase 2/3/5 落地）：Invoke 按 allow_tools/require_confirm
/// 裁决（InvokeRouter::verdict_for）、Query 按 allow_queries 授权、
/// SubagentSpawn 按 allow_subagent 门控（未授予即拒并回 Failed 终态）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
#[allow(dead_code)] // peers 表保留完整 schema（部分字段经 ExecutorPolicy 物化消费）
pub struct FederationPeerSection {
    /// `ws://host:3133`；空 = 仅接受该 peer 连入（纯被动）。
    pub url: String,
    /// per-peer 共享密钥（Bearer；双向相同）。
    pub token: String,
    /// 允许对端调用的本机工具白名单（`*` = 全部；Invoke 执行端裁决消费）。
    #[serde(default)]
    pub allow_tools: Vec<String>,
    /// 是否接受对端的 subagent 委派（SubagentSpawn 执行端门控消费，
    /// false 即拒并回 `SubagentEvent(Failed)`）。
    #[serde(default)]
    pub allow_subagent: bool,
    /// 命中列表的调用需人工/门控确认（v1 简化为直接拒绝；确认通道
    /// （Panel 弹层）留待后续迭代）。
    #[serde(default)]
    pub require_confirm: Vec<String>,
    /// 允许对端的只读查询种类（Phase 5）：node_status 默认允许；
    /// session_snapshot / workspace_files 需显式开启（会话内容敏感）。
    #[serde(default)]
    pub allow_queries: Vec<String>,
}

impl CoreConfig {
    /// Load the Core config file and apply environment-variable overrides.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let mut config: CoreConfig = toml::from_str(&raw)
            .with_context(|| format!("invalid config in {}", path.display()))?;

        validate_logging_level(&config.logging)?;

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
                // New-format config takes priority（共享默认层）。
                config.qq_adapter = qq_cfg.shared.clone();
                config.qq_instances = qq_cfg.instances.clone();
            }
        }

        // Empty command prefix would make bare words trigger command handlers unexpectedly.
        if config.qq_adapter.command_prefix.is_empty() {
            eprintln!("warning: [adapters.qq] command_prefix is empty, falling back to \"/\"");
            config.qq_adapter.command_prefix = "/".to_string();
        }

        // ECHO_ACCESS_TOKEN env override.
        if let Ok(token) = std::env::var("ECHO_ACCESS_TOKEN") {
            if config.qq_adapter.enabled {
                config.qq_adapter.server.access_token = token;
            }
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
    fn federation_defaults_to_listen_3133_without_config() {
        let path = std::env::temp_dir().join("echo-agent-federation-default-test.toml");
        std::fs::write(
            &path,
            "[agent]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n",
        )
        .unwrap();
        let config = CoreConfig::load(&path).unwrap();
        assert_eq!(
            config.federation.listen, "0.0.0.0:3133",
            "federation zero-config default: listen must default to 0.0.0.0:3133"
        );
        std::fs::remove_file(&path).ok();
    }
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
    fn adapter_config_parses_and_overrides_defaults() {
        let dir = std::env::temp_dir();
        let path = dir.join("echo-new-adapter-test.toml");
        std::fs::write(
            &path,
            r#"
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
