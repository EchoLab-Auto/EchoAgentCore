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
pub mod api_pool;

mod metering;
mod redacting;

pub use metering::{wrap_metering, UsageRecorder};
pub use redacting::wrap_redacting;

/// 插件 id（`plugins.toml` / Panel 插件清单同一口径）。
pub const PLUGIN_ID: &str = "echo-agent.provider.llm";

/// 本插件贡献的服务键（`Controller` 服务定位命名空间）。
pub const SERVICE_KEY: &str = "llm";

/// 构建一个 provider 所需的配置投影。
///
/// 与宿主 `AgentConfig` 的同名子集一一对应，但**不含**任何业务字段
/// （无 pricing / profiles / teams）——本 crate 不认识配置文件的形状。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderConfig {
    /// provider 种类：`openai` / `deepseek` / `third-party` / `anthropic` /
    /// `claude` / `kimi` / `moonshot-coding` / `ollama`。
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
            "不支持的 provider: {other} (可选: openai/deepseek/third-party/anthropic/kimi/ollama)"
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
