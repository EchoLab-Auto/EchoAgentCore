//! # echo-plugin-provider-llm — LLM provider 插件（解耦计划 Phase 3 ①）
//!
//! 原 `echo-agent::packages::provider_llm` 的独立 crate 化。职责：
//!
//! - [`create_provider`]：按 [`ProviderConfig`] 构建具体 provider
//!   （OpenAI 兼容 / Anthropic Messages / Ollama），含 DeepSeek 的
//!   `/anthropic` 端点自动路由与 Kimi For Coding 的 base_url 归一化；
//! - [`wrap_metering`]：token 用量计量装饰器（余额/用量图表数据源）；
//! - [`wrap_redacting`]：LLM 请求出口脱敏装饰器（秘密不出本机）。
//!
//! **依赖纪律**：本 crate 只依赖定义层（`echo-defs`）与三个 provider 实现
//! crate——不依赖 `echo-agent`、不依赖配置/指标等业务 crate。宿主把
//! `[agent]` 配置投影成 [`ProviderConfig`]，把指标存储实现成
//! [`UsageRecorder`]，两者都是 5 行 glue，不进本 crate。
//!
//! 消息词汇与 [`LlmProvider`] 接缝仍由 `echo-defs` 定义；本 crate 一并
//! re-export，使调用方只需 `use echo_plugin_provider_llm::…`（原
//! `echo_agent::llm::…` 路径经宿主 shim 保持兼容）。

#![warn(missing_docs)]

use std::sync::Arc;

use echo_defs::llm::LlmError;

pub use echo_defs::llm::{LlmError as ProviderError, LlmProvider};
pub use echo_defs::message::{
    ChatChunk, ChatMessage, ChatRequest, ChatResponse, ChatRole, ToolCall, ToolCallDelta, Usage,
};
pub use echo_defs::mode::{ReasoningEffort, ThinkingMode};
pub use echo_defs::token::{
    estimate_history_tokens, estimate_message_tokens, estimate_tokens, truncate,
    truncate_message_to_tokens, truncate_text_to_tokens,
};
pub use echo_defs::tool::ToolDefinition;

/// 全局供应商池的读取助手（`list_api_profiles` 工具与子代理 profile 解析共用）。
pub mod admin;
pub mod api_pool;
pub mod balance;
/// 供应商 / API 命令域（全插件化 S5：13 条命令的归属关系）。
pub mod host_keys;
pub mod commands;
pub mod metrics;

mod metering;
mod redacting;

pub use metering::{wrap_metering, UsageRecorder};
pub use redacting::wrap_redacting;

/// 插件 id（`plugins.toml` / Panel 插件清单同一口径）。
pub const PLUGIN_ID: &str = "echo-agent.provider.llm";

/// provider 实例的服务键（**别名**，真源在定义层）。
///
/// `Ctx["llm"]` = `Arc<dyn LlmProvider>`：由**组合根**注册，由
/// `echo-plugin-loop` 消费。本插件**不提供**它——提供的是命令域接缝
/// [`host_keys::PROVIDER_HOST`]。
///
/// ## 2026-11 更正
///
/// 这个常量此前叫 `SERVICE_KEY`、文档写"本插件贡献的服务键"。名字与文档把
/// ownership 指向了插件，而实际在宿主；`loop` 侧的 `Inject::new(["llm"])` 也
/// 是一份独立的字面量。三处各写一遍，只是这次三处恰好一致。
///
/// 现在键在 [`echo_defs::llm::LLM_PROVIDER_SERVICE_KEY`]，这里留别名是为了
/// "读本 crate 就能看到它依赖/涉及哪些键"的局部性。
pub const PROVIDER_KEY: &str = echo_defs::llm::LLM_PROVIDER_SERVICE_KEY;

/// 构建一个 provider 所需的配置投影。
///
/// 与宿主 `AgentConfig` 的同名子集一一对应，但**不含**任何业务字段
/// （无 pricing / profiles / teams）——本 crate 不认识配置文件的形状。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderConfig {
    /// provider 种类：`openai` / `deepseek` / `third-party` / `anthropic` /
    /// `claude` / `kimi` / `moonshot-coding` / `opencode-go` / `ollama`。
    pub provider: String,
    /// API base url（`/anthropic` 结尾时自动走 Anthropic Messages 客户端）。
    pub base_url: String,
    /// 模型名。
    pub model: String,
    /// API key。
    pub api_key: String,
    /// 思考模式（DeepSeek 专有字段；词汇归定义层 `echo-defs`）。
    pub thinking: ThinkingMode,
    /// 推理强度（词汇归定义层 `echo-defs`）。
    pub reasoning_effort: ReasoningEffort,
}

/// 构造插件：命令域（13 条 API / provider 命令）。
///
/// **零参数**（全插件化 S6）：宿主能力经服务表解析（[`host_keys::PROVIDER_HOST`]），
/// 不再由组合根把 `Arc<dyn ProviderCommandHost>` 塞进来。于是"哪个插件需要
/// 宿主的什么"这件事，从**构造点**搬进了**插件自己的 mount**——目录那边只剩
/// `plugin(version)`。
///
/// `mount` 不注册任何运行期资源：provider 的**构造**仍由宿主在配置变更时调用
/// [`create_provider`]（它要 `AgentConfig`、要重建链、要广播——那些是宿主的
/// 动作）。本插件"启用"的含义是**它的命令域可以被派发**；禁用后这些命令回落
/// 到宿主的兜底分支并明确报"插件未启用"。
pub fn plugin(version: &str) -> Arc<dyn echo_plugin::Plugin> {
    use echo_plugin::{BuiltinPlugin, Inject, PluginManifest, ServiceSlot};

    let host = ServiceSlot::<Arc<dyn commands::ProviderCommandHost>>::new();
    let bind = host.clone();
    let serve = host.clone();
    let manifest = PluginManifest::builtin(
        PLUGIN_ID,
        "LLM Provider",
        version,
        "provider",
        "llm",
        "LLM 提供方：provider 工厂 + 计量/脱敏装饰器 + 供应商池规则与 API 命令域",
    );
    Arc::new(
        BuiltinPlugin::new(manifest, move |mctx| {
            // 缺宿主 = 挂载失败（比"挂着但每条命令都回不出东西"好）。
            let keep = bind.bind(mctx, &host_keys::PROVIDER_HOST)?;
            Ok(vec![keep])
        })
        .with_inject(Inject::new([
            host_keys::PROVIDER_HOST.name().to_string(),
        ]))
        .with_commands(commands::COMMANDS)
        .with_command_handler_async(move |name, payload| {
            let serve = serve.clone();
            Box::pin(async move {
                let Some(host) = serve.get() else {
                    return Err(format!(
                        "命令 {name} 到 {PLUGIN_ID} 时插件尚未挂载（宿主接缝未绑定）"
                    ));
                };
                commands::ProviderCommandHandler::new(host)
                    .handle(&name, payload)
                    .await
                    .unwrap_or_else(|| {
                        Err(format!("命令 {name} 被 {PLUGIN_ID} 声明，但 provider 处理器没有认领它"))
                    })
            })
        }),
    )
}

/// 按配置构建一个 provider。
///
/// base_url 以 `/anthropic` 结尾时（如 DeepSeek 的 Anthropic 兼容端点
/// `https://api.deepseek.com/anthropic`）自动走 Anthropic Messages 格式客户端，
/// 即使 provider 名为 openai/deepseek。
pub fn create_provider(cfg: &ProviderConfig) -> Result<Box<dyn LlmProvider>, LlmError> {
    let provider_kind = if cfg.provider.is_empty() {
        return Err(LlmError::Config(
            "no LLM provider configured — use /api in TUI or set [agent] provider in config".into(),
        ));
    } else {
        cfg.provider.as_str()
    };
    let anthropic_style = cfg.base_url.trim_end_matches('/').ends_with("/anthropic");
    let deepseek_reasoning = (provider_kind == "deepseek"
        || cfg.base_url.contains("api.deepseek.com"))
    .then_some((cfg.thinking, cfg.reasoning_effort));
    match provider_kind {
        // OpenCode Go / Zen（订阅制网关，OpenAI 兼容）：**比普通第三方便多两件
        // 事**——网关把身份与路由放在请求头里：
        //   * `x-opencode-session`：稳定会话 id。官方文档明说"每个对话发一个
        //     稳定 session id"，缺了直接 400 MissingSessionID（先于认证校验）；
        //   * `user-agent`：官方要求客户端**用自己的** UA 自报家门（而不是通用
        //     SDK 名），并声明"流量会被监控滥用"。伪装成 opencode 只会更容易
        //     被判定异常，所以这里如实报 echo-agent。
        "opencode-go" | "opencode" => {
            let base = opencode_base_url(&cfg.base_url);
            let provider = echo_llm_openai::OpenAiProvider::new(&base, &cfg.api_key, &cfg.model)
                .with_reasoning(cfg.thinking, cfg.reasoning_effort)
                .with_header("x-opencode-session", &new_session_id())
                .with_header("user-agent", &client_user_agent());
            Ok(Box::new(provider))
        }
        // Kimi For Coding（Kimi Code 订阅）：Anthropic 兼容端点
        //（`https://api.kimi.com/coding/v1/messages`），认证 = Kimi Code Console
        // 创建的 API Key（`x-api-key`），推理档位经 `output_config.effort`。
        "kimi" | "moonshot-coding" => {
            let base = kimi_base_url(&cfg.base_url);
            let provider =
                echo_llm_anthropic::AnthropicProvider::new(&base, &cfg.api_key, &cfg.model)
                    .with_reasoning(cfg.thinking, cfg.reasoning_effort);
            Ok(Box::new(provider))
        }
        "openai" | "deepseek" | "third-party" if anthropic_style => {
            // DeepSeek 的 /anthropic 端点只实现 Messages API（POST /v1/messages）
            let mut provider =
                echo_llm_anthropic::AnthropicProvider::new(&cfg.base_url, &cfg.api_key, &cfg.model);
            if let Some((thinking, effort)) = deepseek_reasoning {
                provider = provider.with_reasoning(thinking, effort);
            }
            Ok(Box::new(provider))
        }
        "openai" | "deepseek" | "third-party" => {
            let mut provider =
                echo_llm_openai::OpenAiProvider::new(&cfg.base_url, &cfg.api_key, &cfg.model);
            if let Some((thinking, effort)) = deepseek_reasoning {
                provider = provider.with_reasoning(thinking, effort);
            }
            Ok(Box::new(provider))
        }
        "anthropic" | "claude" => Ok(Box::new(echo_llm_anthropic::AnthropicProvider::new(
            &cfg.base_url,
            &cfg.api_key,
            &cfg.model,
        ))),
        "ollama" => Ok(Box::new(echo_llm_ollama::OllamaProvider::new(
            &cfg.base_url,
            &cfg.model,
        ))),
        other => Err(LlmError::Config(format!(
            "不支持的 provider: {other} (可选: openai/deepseek/third-party/anthropic/kimi/opencode-go/ollama)"
        ))),
    }
}

/// Kimi For Coding 的 base_url 归一化。
///
/// 官方文档给的是 `https://api.kimi.com/coding/`（Anthropic 兼容），而本项目
/// 的 Anthropic 客户端会自行拼接 `/v1/messages`——因此这里接受两种写法：
/// `https://api.kimi.com/coding` 与 `https://api.kimi.com/coding/v1`（也容忍
/// 结尾斜杠与 `/v1/messages`）。空值回退到官方端点。
pub fn kimi_base_url(configured: &str) -> String {
    const DEFAULT: &str = "https://api.kimi.com/coding";
    let trimmed = configured.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return DEFAULT.to_string();
    }
    let base = trimmed
        .strip_suffix("/v1/messages")
        .or_else(|| trimmed.strip_suffix("/v1"))
        .unwrap_or(trimmed);
    if base.is_empty() {
        DEFAULT.to_string()
    } else {
        base.to_string()
    }
}

/// OpenCode Go 的 base_url 归一化。
///
/// 官方给的是「base + API 路径」两级拼接：
/// - 按量付费：`https://opencode.ai/inference` + `/openai/v1/chat/completions`
/// - Go 订阅：`https://opencode.ai/inference/go` + 同一路径
///
/// 本项目的 OpenAI 客户端会自行追加 `/chat/completions`，因此这里接受四种
/// 写法并统一到「`…/v1` 结尾」：空值、只有 `/inference`、只有
/// `/inference/go`、以及已经写全的 `/openai/v1`（含误贴进来的
/// `/chat/completions` 尾巴）。
pub fn opencode_base_url(configured: &str) -> String {
    const DEFAULT: &str = "https://opencode.ai/inference/go/openai/v1";
    let trimmed = configured.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return DEFAULT.to_string();
    }
    let base = trimmed
        .strip_suffix("/chat/completions")
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    if base.is_empty() {
        return DEFAULT.to_string();
    }
    if base.ends_with("/inference") {
        // 只写了根：按 Go 订阅补全（订阅是本项目的目标场景）。
        return format!("{base}/go/openai/v1");
    }
    if base.ends_with("/inference/go") {
        return format!("{base}/openai/v1");
    }
    base.to_string()
}

/// 客户端自报 UA（OpenCode 官方要求"用自己的名字"，而不是通用 SDK 名）。
fn client_user_agent() -> String {
    format!("echo-agent/{}", env!("CARGO_PKG_VERSION"))
}

/// 生成一个 OpenCode 网关的稳定会话 id（`ses_` 前缀 + 32 位十六进制）。
///
/// **稳定性口径**：provider 实例级——同一次进程内、同一 profile 的所有请求
/// 共用一个 id，重启后换新。官方要的是"每个对话一个"，本项目的
/// `ChatRequest` 不携带对话身份，无法做到逐对话；共用 id 只影响网关侧的
/// 路由与 prompt cache 复用率（功能不受影响），比把对话 id 一路穿到
/// 35 处构造函数里划算。
fn new_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seq = u128::from(SEQ.fetch_add(1, Ordering::Relaxed));
    let mixed = nanos ^ (u128::from(std::process::id()) << 64) ^ (seq << 32);
    format!("ses_echo_{mixed:032x}")
}

/// 便捷：把 provider 装箱为 `Arc<dyn LlmProvider>`。
pub fn create_provider_arc(cfg: &ProviderConfig) -> Result<Arc<dyn LlmProvider>, LlmError> {
    create_provider(cfg).map(Arc::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(provider: &str, base_url: &str) -> ProviderConfig {
        ProviderConfig {
            provider: provider.into(),
            base_url: base_url.into(),
            model: "test-model".into(),
            api_key: "test-key".into(),
            ..Default::default()
        }
    }

    #[test]
    fn deepseek_anthropic_endpoint_routes_to_anthropic_client() {
        let p = create_provider(&cfg("deepseek", "https://api.deepseek.com/anthropic")).unwrap();
        assert_eq!(p.name(), "anthropic");
    }

    #[test]
    fn deepseek_openai_endpoint_routes_to_openai_client() {
        let p = create_provider(&cfg("deepseek", "https://api.deepseek.com")).unwrap();
        assert_eq!(p.name(), "openai");
        let p = create_provider(&cfg("deepseek", "")).unwrap();
        assert_eq!(p.name(), "openai");
    }

    #[test]
    fn third_party_provider_uses_openai_compatible_client() {
        let p = create_provider(&cfg("third-party", "https://one-api.example.com/v1")).unwrap();
        assert_eq!(p.name(), "openai");
        // anthropic-style URL switches to the Messages client.
        let p =
            create_provider(&cfg("third-party", "https://one-api.example.com/anthropic")).unwrap();
        assert_eq!(p.name(), "anthropic");
    }

    #[test]
    fn kimi_provider_uses_anthropic_client_and_normalizes_base_url() {
        // 官方写法（带 /v1）与不带 /v1 都归一到同一 Anthropic 端点。
        assert_eq!(
            kimi_base_url("https://api.kimi.com/coding/v1"),
            "https://api.kimi.com/coding"
        );
        assert_eq!(
            kimi_base_url("https://api.kimi.com/coding/"),
            "https://api.kimi.com/coding"
        );
        assert_eq!(
            kimi_base_url("https://api.kimi.com/coding/v1/messages"),
            "https://api.kimi.com/coding"
        );
        assert_eq!(kimi_base_url(""), "https://api.kimi.com/coding");
        assert_eq!(
            kimi_base_url("https://api.kimi.com/coding"),
            "https://api.kimi.com/coding"
        );

        let p = create_provider(&cfg("kimi", "https://api.kimi.com/coding/v1")).unwrap();
        assert_eq!(p.name(), "anthropic");
    }

    #[test]
    fn opencode_go_uses_openai_client_and_normalizes_base_url() {
        // 各种写法都归一到「…/inference/go/openai/v1」。
        for configured in [
            "",
            "https://opencode.ai/inference/go/openai/v1",
            "https://opencode.ai/inference/go",
            "https://opencode.ai/inference",
            "https://opencode.ai/inference/go/openai/v1/chat/completions",
            "https://opencode.ai/inference/go/",
        ] {
            assert_eq!(
                opencode_base_url(configured),
                "https://opencode.ai/inference/go/openai/v1",
                "configured = {configured:?}"
            );
        }
        // 自建/镜像：原样保留（只去掉结尾斜杠）。
        assert_eq!(
            opencode_base_url("https://mirror.example.com/v1/"),
            "https://mirror.example.com/v1"
        );
        // 按量付费（Zen）走同一分支，只差 base_url。
        assert_eq!(
            opencode_base_url("https://opencode.ai/inference/openai/v1"),
            "https://opencode.ai/inference/openai/v1"
        );
        let p = create_provider(&cfg("opencode-go", "")).unwrap();
        assert_eq!(p.name(), "openai");
    }

    /// session id 必须带 `ses_` 前缀（网关按前缀识别）、长度固定、且每次不同。
    #[test]
    fn session_ids_are_well_formed_and_unique() {
        let a = new_session_id();
        let b = new_session_id();
        assert!(a.starts_with("ses_"), "id: {a}");
        assert_eq!(a.len(), "ses_echo_".len() + 32);
        assert!(
            a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "id: {a}"
        );
        assert_ne!(a, b);
    }

    /// UA 报自己的名字（官方要求"用自己的 user agent"），绝不自称 opencode。
    #[test]
    fn user_agent_identifies_the_client() {
        let ua = client_user_agent();
        assert!(ua.starts_with("echo-agent/"), "ua: {ua}");
        assert!(!ua.contains("opencode"), "不得伪装成 opencode: {ua}");
    }

    #[test]
    fn unknown_provider_is_config_error() {
        assert!(matches!(
            create_provider(&cfg("gpt-x", "")),
            Err(LlmError::Config(_))
        ));
    }

    #[test]
    fn empty_provider_is_config_error() {
        assert!(matches!(
            create_provider(&cfg("", "")),
            Err(LlmError::Config(_))
        ));
    }

    #[test]
    fn ollama_provider_is_ollama() {
        let p = create_provider(&cfg("ollama", "http://127.0.0.1:11434")).unwrap();
        assert_eq!(p.name(), "ollama");
    }

    #[test]
    fn provider_config_defaults_are_thinking_on_max_effort() {
        // 与定义层默认一致：启用思考、推理强度 max（宿主 AgentConfig 同口径）。
        let d = ProviderConfig::default();
        assert_eq!(d.thinking, ThinkingMode::Enabled);
        assert_eq!(d.reasoning_effort, ReasoningEffort::Max);
    }
}
