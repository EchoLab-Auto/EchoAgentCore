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
    /// Core↔Core 联邦链路（零配置缺省开启：缺省监听 `0.0.0.0:3133`，详见 [`FederationSection`]）。
    pub federation: FederationSection,
    /// New adapter configuration.
    #[serde(alias = "adapters")]
    pub adapters_section: Option<AdaptersSection>,
    /// Resolved QQ 配置视图（populated in `load()`；P1.9 起为透传宿主视图——
    /// 完整配置以 TOML 表下发插件，宿主只读校验 / 登记所需字段）。
    #[serde(skip)]
    pub qq_adapter: QqHostConfig,
    /// QQ 实例表（多实例；缺省 = 单实例 `qq`）。
    #[serde(skip)]
    pub qq_instances: std::collections::BTreeMap<String, QqInstanceSection>,
    /// `[adapters.qq] mode`：`"builtin"`（缺省）| `"plugin"`。
    #[serde(skip)]
    pub qq_adapter_mode: String,
    /// `[adapters.qq.plugin]`：mode = "plugin" 时的插件进程启动配置。
    #[serde(skip)]
    pub qq_plugin_launch: QqPluginLaunchSection,
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
/// 详见 `document/core/security-redaction-design.md`。`enabled=false` 需要重启
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
///
/// **平台中立**：`[adapters.<name>]` 里的任意平台名都原样收进
/// [`platforms`](Self::platforms)——新增平台不需要改本结构体，宿主也不认识
/// 具体平台（平台专属键透传给插件）。此前是硬编码的 `qq: Option<QqSection>`，
/// 配 `[adapters.telegram]` 会被 serde 静默丢弃。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AdaptersSection {
    /// 平台名 → 该平台的适配器段（`shared` 透传 / `plugin` 启停 / `instances` 表）。
    #[serde(flatten)]
    pub platforms: std::collections::BTreeMap<String, QqSection>,
}

impl AdaptersSection {
    /// 取某个平台的段（`None` = 该平台未配置）。
    pub fn platform(&self, name: &str) -> Option<&QqSection> {
        self.platforms.get(name)
    }

    /// 已配置的平台名（按字典序）。
    ///
    /// 目前只被解析回归测试消费（bin 构建下会报 dead_code）；B 阶段泛化
    /// 多平台装配时进入生产路径。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn names(&self) -> Vec<&String> {
        self.platforms.keys().collect()
    }
}

/// `[adapters.qq]`：共享默认 + 多实例表。
///
/// - 顶层字段是**共享默认值**（镜像、auto_start、路径模板…）；
/// - `instances.<id>` 覆盖单实例差异（persona 归属、端口、owner…）。
/// - 未配置 `instances` 时按「默认值 → 唯一实例 `qq`」解析（legacy 行为）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QqSection {
    /// 共享默认层（透传：原样进 `Hello.config.config`；宿主只读 `enabled` /
    /// `command_prefix` / `server.access_token` 等少量字段）。
    #[serde(flatten)]
    pub shared: toml::Table,
    /// 适配器宿主模式（历史字段，2026-10 适配器插件化；builtin 模式已移除，
    /// 见 `load()` 的归一化告警）。
    pub mode: String,
    /// 插件进程启动配置（`[adapters.qq.plugin]`）。
    pub plugin: QqPluginLaunchSection,
    /// 实例表：实例 id → 覆盖项。
    pub instances: std::collections::BTreeMap<String, QqInstanceSection>,
}

/// 宿主侧 QQ 配置视图（P1.9 配置本地化）：透传为主、宿主只读少量字段。
///
/// `[adapters.qq]` 的完整内容（共享层 + 实例覆盖）以 **TOML 表**原样下发
/// 给适配器插件（`Hello.config.config`）——宿主不重复插件配置 schema，
/// 新字段自动透传。本视图仅承载宿主自身需要的读点：
/// - [`enabled`](Self::enabled)：实例解析 / 自启停门控；
/// - [`command_prefix`](Self::command_prefix)：命令域（`/echo` 等）；
/// - [`access_token`](Self::access_token)：脱敏器密钥登记。
#[derive(Debug, Clone, Default)]
pub struct QqHostConfig {
    /// 共享层配置表（归一化后：`command_prefix` 空 → `/`、
    /// `ECHO_ACCESS_TOKEN` 覆盖已应用）。
    pub shared: toml::Table,
    /// `[adapters.qq].enabled`（缺省 false）。
    pub enabled: bool,
    /// 归一化后的命令前缀。
    pub command_prefix: String,
    /// `[adapters.qq.server].access_token`（空 = 未设置）。
    pub access_token: String,
}

impl QqHostConfig {
    /// 从共享层表构造（读取宿主读点；`load()` 归一化后调用）。
    pub fn from_shared(shared: toml::Table) -> Self {
        let enabled = shared
            .get("enabled")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        let command_prefix = shared
            .get("command_prefix")
            .and_then(toml::Value::as_str)
            .unwrap_or("/")
            .to_string();
        let access_token = shared
            .get("server")
            .and_then(toml::Value::as_table)
            .and_then(|server| server.get("access_token"))
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_string();
        Self {
            shared,
            enabled,
            command_prefix,
            access_token,
        }
    }
}

/// `[adapters.qq.plugin]`：适配器插件进程（stdio）启动配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QqPluginLaunchSection {
    /// 插件可执行文件（绝对路径 / PATH 名）。
    pub command: String,
    /// 追加命令行参数。
    pub args: Vec<String>,
    /// 注入子进程的环境变量（宿主环境不继承，仅透传 PATH）。
    pub env: std::collections::BTreeMap<String, String>,
}

impl Default for QqPluginLaunchSection {
    fn default() -> Self {
        Self {
            command: "echo-adapter-qq".to_string(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }
    }
}

/// 单个 QQ 实例的配置覆盖项（缺省继承 `[adapters.qq]` 共享默认）。
///
/// - `persona` / `ports` / `enabled` = 宿主字段（实例归属 / 端口分配 / 启停）；
/// - 其余键（`server` / `owner_qq` / `napcat_*` / 插件新增键…）进
///   [`config`](Self::config) 覆盖片段，与共享层深合并后原样下发插件
///   （宿主不重复插件 schema，新增字段自动透传）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QqInstanceSection {
    /// 归属人格 id（缺省 = 迁移期的旧单实例语义：第一个已启用人格）。
    pub persona: Option<String>,
    /// 该实例独立端口（多容器多通道）；缺省由分配器填写并持久化。
    pub ports: QqPorts,
    /// 该实例是否启用（缺省跟随共享默认的 `enabled`）。
    pub enabled: Option<bool>,
    /// 实例级配置覆盖片段（浅表保留；合并语义见
    /// `qq_instances::merge_instance`）。
    #[serde(flatten)]
    pub config: toml::Table,
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
/// **零配置默认开启**（2026-10）：缺省监听——**与管理面合并**（单端口按
/// 令牌命名空间分流，见 federation.md 端口合并；2026-10 缺省合并语义）。
/// 任何 Core 开箱即可连出也可被连入（邀请串配对开箱可用）；未配 peer 时
/// 不接受任何已知链路（accept 侧靠 per-peer token 认证）。显式
/// `listen = ""` 退回纯连出。一旦配置 `[federation.peers.*]`，互信边界即
/// 生效（≈ SSH 免密），务必用 per-peer token 并限制
/// `allow_tools`/`allow_queries`。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FederationSection {
    /// 联邦监听地址（**缺省 = 跟随管理地址合并**，显式地址 = 独立监听，
    /// 显式 `""` = 不监听纯连出；`listen_explicit` 区分缺省与显式）。
    pub listen: String,
    /// `listen` 是否为配置文件中**显式书写**（serde 缺省填充后二者无法
    /// 区分，而「缺省合并」与「显式独立」语义不同；`load()` 从原始 TOML
    /// 探测填充，直接构造的结构体为 `false` = 按缺省处理）。
    #[serde(skip)]
    pub listen_explicit: bool,
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
            listen_explicit: false,
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

        // 端口合并「缺省 vs 显式」判定：原始 TOML 里是否写有
        // [federation].listen 键（serde 填充后无法区分「缺省」与「显式
        // 同值」；两者语义不同——缺省=自动合并管理端口、显式=按值独立/
        // 合并）。raw 已成功解析为 CoreConfig，此处的二次解析不会失败。
        if let Ok(table) = raw.parse::<toml::Table>() {
            config.federation.listen_explicit = table
                .get("federation")
                .and_then(|section| section.get("listen"))
                .is_some();
        }

        validate_logging_level(&config.logging)?;

        // Deduplicate api_profiles by name (self-healing for a historical persistence bug).
        {
            let mut seen = std::collections::HashSet::new();
            config
                .agent
                .api_profiles
                .retain(|p| seen.insert(p.name.clone()));
        }

        // ---- Build QQ adapter config（P1.9 配置本地化：TOML 表透传）----
        if let Some(ref adapters) = config.adapters_section {
            if let Some(qq_cfg) = adapters.platform("qq") {
                // New-format config takes priority（共享默认层）。
                config.qq_adapter = QqHostConfig::from_shared(qq_cfg.shared.clone());
                config.qq_instances = qq_cfg.instances.clone();
                config.qq_adapter_mode = qq_cfg.mode.trim().to_string();
                config.qq_plugin_launch = qq_cfg.plugin.clone();
            }
        }
        // builtin 模式已移除（P1.9，2026-10 适配器插件化收尾）：QQ 适配器
        // 只作为独立进程插件运行（`[adapters.qq.plugin]`）。
        if config.qq_adapter_mode == "builtin" {
            eprintln!(
                "warning: [adapters.qq] mode = \"builtin\" 已移除（适配器插件化收尾）；\
                 按 plugin 模式运行，请在 [adapters.qq.plugin] 配置 command"
            );
            config.qq_adapter_mode.clear();
        }

        // Empty command prefix would make bare words trigger command handlers unexpectedly.
        {
            let prefix_empty = config
                .qq_adapter
                .shared
                .get("command_prefix")
                .and_then(toml::Value::as_str)
                .map(|prefix| prefix.is_empty())
                .unwrap_or(true);
            if prefix_empty {
                if config.qq_adapter.shared.contains_key("command_prefix") {
                    eprintln!(
                        "warning: [adapters.qq] command_prefix is empty, falling back to \"/\""
                    );
                }
                config
                    .qq_adapter
                    .shared
                    .insert("command_prefix".into(), toml::Value::String("/".into()));
            }
            config.qq_adapter.command_prefix = config
                .qq_adapter
                .shared
                .get("command_prefix")
                .and_then(toml::Value::as_str)
                .unwrap_or("/")
                .to_string();
        }

        // ECHO_ACCESS_TOKEN env override.
        if let Ok(token) = std::env::var("ECHO_ACCESS_TOKEN") {
            if config.qq_adapter.enabled {
                let server = config
                    .qq_adapter
                    .shared
                    .entry("server")
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                if let Some(server) = server.as_table_mut() {
                    server.insert("access_token".into(), toml::Value::String(token.clone()));
                }
                config.qq_adapter.access_token = token;
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

    /// `[adapters.<name>]` 是平台中立的映射：任意平台名都能解析进来。
    ///
    /// 回归背景：此前 `AdaptersSection` 只有硬编码的 `qq` 字段，
    /// `[adapters.telegram]` 会被 serde 静默丢弃（既不报错也不生效）。
    #[test]
    fn adapters_section_accepts_any_platform_name() {
        let path = std::env::temp_dir().join("echo-agent-adapters-platforms-test.toml");
        std::fs::write(
            &path,
            r#"
[agent]
provider = "openai"
model = "gpt-4o"

[adapters.qq]
enabled = true
command_prefix = "!"

[adapters.qq.plugin]
command = "echo-adapter-qq"

[adapters.telegram]
enabled = true

[adapters.telegram.plugin]
command = "echo-adapter-telegram"
"#,
        )
        .unwrap();
        let config = CoreConfig::load(&path).unwrap();
        let section = config.adapters_section.as_ref().expect("adapters 段应存在");
        assert_eq!(
            section.names(),
            vec!["qq", "telegram"],
            "两个平台都应被收录"
        );
        assert_eq!(
            section.platform("telegram").unwrap().plugin.command,
            "echo-adapter-telegram",
            "未知平台段原样解析，不依赖宿主认识它"
        );
        // 宿主读点仍只取 qq（B 阶段才会泛化到多平台）
        assert!(config.qq_adapter.enabled);
        assert_eq!(config.qq_adapter.command_prefix, "!");
        std::fs::remove_file(&path).ok();
    }

    /// listen 字段的**缺省值**（回退地址）——零配置实际生效地址由
    /// `management::plan_federation_listen` 决定（缺省合并至管理地址；
    /// 保守场景才回退为这里的 0.0.0.0:3133）。
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
            "listen 字段缺省 = 回退地址 0.0.0.0:3133（缺省合并由 plan_federation_listen 决定）"
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
        // P1.9 透传视图：完整配置在 TOML 表内（原样下发插件），宿主只读视图字段。
        assert_eq!(
            config.qq_adapter.shared["server"]["bind_address"].as_str(),
            Some("0.0.0.0:9999")
        );
        assert_eq!(config.qq_adapter.access_token, "new_token");
        assert_eq!(
            config.qq_adapter.shared["trigger"]["dm_auto_reply"].as_bool(),
            Some(false)
        );
        std::fs::remove_file(&path).ok();
    }

    /// `[adapters.qq] mode` / `[adapters.qq.plugin]` 解析（P1.8）：
    /// flatten 共享默认与 mode/plugin 字段共存；缺省 = builtin（空串）。
    #[test]
    fn qq_plugin_mode_parses_alongside_shared_defaults() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "echo-qq-plugin-mode-test-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
[adapters.qq]
mode = "plugin"
enabled = true

[adapters.qq.plugin]
command = "/opt/echo/echo-adapter-qq"
args = ["--verbose"]

[adapters.qq.instances.alix-two]
persona = "alix"

[adapters.qq.server]
bind_address = "0.0.0.0:9999"
"#,
        )
        .unwrap();
        let config = CoreConfig::load(&path).unwrap();
        assert_eq!(config.qq_adapter_mode, "plugin");
        assert_eq!(config.qq_plugin_launch.command, "/opt/echo/echo-adapter-qq");
        assert_eq!(config.qq_plugin_launch.args, vec!["--verbose"]);
        // flatten 共享层仍生效（mode/plugin 不吞掉适配器配置字段）。
        assert!(config.qq_adapter.enabled);
        assert_eq!(
            config.qq_adapter.shared["server"]["bind_address"].as_str(),
            Some("0.0.0.0:9999")
        );
        assert!(config.qq_instances.contains_key("alix-two"));
        std::fs::remove_file(&path).ok();
    }

    /// 缺省 = builtin：mode 空串、plugin.command 为缺省二进制名。
    #[test]
    fn qq_plugin_mode_defaults_to_builtin() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "echo-qq-plugin-default-test-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
[adapters.qq]
enabled = true
"#,
        )
        .unwrap();
        let config = CoreConfig::load(&path).unwrap();
        assert_eq!(config.qq_adapter_mode, "");
        assert_eq!(config.qq_plugin_launch.command, "echo-adapter-qq");
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
