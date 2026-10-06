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
    /// QQ 实例表（多实例；缺省 = 单实例 `qq`）。
    #[serde(skip)]
    pub qq_instances: std::collections::BTreeMap<String, QqInstanceSection>,
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
    /// 人类可读节点别名（Hello 中携带；缺省仅 node_id）。
    pub node_name: Option<String>,
    /// 静态对等节点表。
    pub peers: std::collections::BTreeMap<String, FederationPeerSection>,
}

impl Default for FederationSection {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:3133".into(),
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

/// 加载期插件名单迁移（内存迁移，保存自愈）：在 `CoreConfig::load` 反序列化后
/// 调用，覆盖 per-persona 白名单/黑名单与全局 `[agent].disabled_plugins`。
///
/// 1. **循环模式插件 id 归一化**：旧编排模式 id（`branch.reply` /
///    `session.global` / `chatbot.sessions`）→ `echo-agent.loop.parallel`；
///    旧驱动 id `loop.runner` 与已移除的 `menu` / `checklist` 插件 id 剔除
///    （见 [`echo_agent::plugins::normalize_mode_plugins`]）——旧 id 不再注册，
///    不迁移则 `apply_disabled` 静默失效；
/// 2. **per-persona 插件黑名单移除**（2026-09-11）：`disabled_plugins` 的
///    语义物化进 `enabled_plugins` 白名单（见
///    [`echo_agent::plugins::convert_plugin_blacklist_to_whitelist`]），既有
///    配置的门控与循环模式行为不变；黑名单字段自此不再参与判定。
///
/// 返回迁移/警告说明（load 期打印；纯函数便于测试断言）。
///
/// 历史：本函数曾名 `migrate_orchestration_mode_plugins`，于 54f1bb3
/// （删除后台任务/并行分支）连同调用点被误删——加载期迁移自此缺失，
/// 旧配置的名单归一化与黑名单物化静默失效（2026-09-29 恢复并改名）。
pub fn migrate_plugin_lists(agent: &mut echo_agent::AgentConfig) -> Vec<String> {
    use echo_agent::plugins::{
        convert_plugin_blacklist_to_whitelist, normalize_mode_plugins, PARALLEL_LOOP_PLUGIN_ID,
        SINGLE_LOOP_PLUGIN_ID,
    };
    let mut notes = Vec::new();

    let normalize = |list: &mut Vec<String>, scope: &str, notes: &mut Vec<String>| {
        if normalize_mode_plugins(list) {
            notes.push(format!(
                "migrated legacy loop plugin ids → {SINGLE_LOOP_PLUGIN_ID}/{PARALLEL_LOOP_PLUGIN_ID} ({scope})"
            ));
        }
    };

    for (id, member) in agent.teams.iter_mut() {
        normalize(
            &mut member.enabled_plugins,
            &format!("teams.{id}.enabled_plugins"),
            &mut notes,
        );
        if convert_plugin_blacklist_to_whitelist(
            &mut member.enabled_plugins,
            &member.disabled_plugins,
        ) {
            notes.push(format!(
                "migrated teams.{id}.disabled_plugins（插件黑名单已移除）→ enabled_plugins 白名单物化；该字段不再参与门控"
            ));
            member.disabled_plugins.clear();
        }
        // 白名单同时含 single + parallel：互斥循环模式按 parallel 优先（推导单一来源）。
        if member
            .enabled_plugins
            .iter()
            .any(|p| p == PARALLEL_LOOP_PLUGIN_ID)
            && member
                .enabled_plugins
                .iter()
                .any(|p| p == SINGLE_LOOP_PLUGIN_ID)
        {
            notes.push(format!(
                "warning: teams.{id}.enabled_plugins 同时含 single 与 parallel 循环插件，互斥按 parallel 优先"
            ));
        }
    }
    normalize(
        &mut agent.disabled_plugins,
        "agent.disabled_plugins",
        &mut notes,
    );
    notes
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

        // 插件名单迁移：旧编排模式 id → 循环模式插件 id、per-persona 黑名单
        // 物化进白名单。内存迁移，首次 SaveTeam 落盘自愈（与 legacy
        // [server]/[bot] 迁移同范式）。
        for note in migrate_plugin_lists(&mut config.agent) {
            eprintln!("note: {note}");
        }

        // ---- Build QQ adapter config ----
        if let Some(ref adapters) = config.adapters_section {
            if let Some(ref qq_cfg) = adapters.qq {
                // New-format config takes priority（共享默认层）。
                config.qq_adapter = qq_cfg.shared.clone();
                config.qq_instances = qq_cfg.instances.clone();
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

    /// 联邦已取消 `enabled` 开关：旧配置里的 `enabled = false` 必须被**忽略**
    /// 而不是解析失败（否则升级后 Core 起不来）。
    #[test]
    fn legacy_federation_enabled_field_is_ignored() {
        let path = std::env::temp_dir().join("echo-agent-federation-legacy-test.toml");
        std::fs::write(
            &path,
            r#"
[federation]
enabled = false
listen = "0.0.0.0:3133"

[federation.peers.gpu]
url = "ws://localhost:3999"
token = "t"
"#,
        )
        .unwrap();
        let config = CoreConfig::load(&path).expect("legacy enabled field must be ignored");
        assert_eq!(config.federation.listen, "0.0.0.0:3133");
        assert!(config.federation.peers.contains_key("gpu"));
    }

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
    fn migrate_plugin_lists_normalizes_legacy_ids_and_materializes_blacklists() {
        use echo_agent::config::TeamMember;
        use echo_agent::plugins::{
            LEGACY_CHATBOT_MODE_IDS, LEGACY_LOOP_RUNNER_PLUGIN_ID, PARALLEL_LOOP_PLUGIN_ID,
        };

        let mut agent = echo_agent::AgentConfig::default();
        let member = TeamMember {
            enabled_plugins: vec![
                "echo-agent.tools.builtin".into(),
                LEGACY_CHATBOT_MODE_IDS[1].into(),
                LEGACY_LOOP_RUNNER_PLUGIN_ID.into(),
            ],
            disabled_plugins: vec![LEGACY_CHATBOT_MODE_IDS[2].into()],
            ..Default::default()
        };
        agent.teams.insert("bot".into(), member);
        agent.disabled_plugins = vec![LEGACY_CHATBOT_MODE_IDS[1].into()];

        let notes = super::migrate_plugin_lists(&mut agent);

        let m = &agent.teams["bot"];
        // 白名单里的旧编排 id → loop.parallel；但黑名单含旧并行特性 id
        // （session.global）：历史上黑名单优先（推导单会话），物化后白名单
        // 不得再含 parallel，行为保持不变。
        assert_eq!(
            m.enabled_plugins,
            vec!["echo-agent.tools.builtin".to_string()]
        );
        // 黑名单已物化并清空（字段不再参与门控，也不再写回）
        assert!(m.disabled_plugins.is_empty());
        assert_eq!(m.loop_mode(), echo_agent::config::LoopMode::Single);
        // 全局层旧 id 同样被清理映射（否则 apply_disabled 静默失效）
        assert_eq!(
            agent.disabled_plugins,
            vec![PARALLEL_LOOP_PLUGIN_ID.to_string()]
        );
        // 迁移报告覆盖 teams 与全局层
        assert!(notes
            .iter()
            .any(|n| n.contains("teams.bot.enabled_plugins")));
        assert!(notes
            .iter()
            .any(|n| n.contains("teams.bot.disabled_plugins")));
        assert!(notes.iter().any(|n| n.contains("agent.disabled_plugins")));
        // 幂等：二次运行无新报告
        assert!(super::migrate_plugin_lists(&mut agent).is_empty());
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
