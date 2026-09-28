//! LLM provider abstraction.
//!
//! The message vocabulary and the [`LlmProvider`] seam live in
//! [`echo_defs`](echo_defs); this module re-exports them (keeping the
//! `echo_agent::llm::…` paths) and owns the **composition root** factory
//! [`create_provider`] plus the concrete provider implementations.

pub use echo_defs::llm::{LlmError, LlmProvider};
pub use echo_defs::message::{
    ChatChunk, ChatMessage, ChatRequest, ChatResponse, ChatRole, ToolCall, ToolCallDelta, Usage,
};
pub use echo_defs::token::{
    estimate_history_tokens, estimate_message_tokens, estimate_tokens, truncate,
    truncate_message_to_tokens, truncate_text_to_tokens,
};
pub use echo_defs::tool::ToolDefinition;

/// Build a provider from configuration.
///
/// base_url 以 `/anthropic` 结尾时（如 DeepSeek 的 Anthropic 兼容端点
/// `https://api.deepseek.com/anthropic`）自动走 Anthropic Messages 格式客户端，
/// 即使 provider 名为 openai/deepseek。
pub fn create_provider(cfg: &crate::config::AgentConfig) -> Result<Box<dyn LlmProvider>, LlmError> {
    // 空 provider = 未配置 API，按 OpenAI 兼容处理（请求会因无 key 失败，
    // 等用户在 TUI /api 中配置后生效）。
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
        // 创建的 API Key（`x-api-key`），推理档位经 `output_config.effort`
        //（low/high/max，与 ReasoningEffort 一一对应）。
        // opencode / Claude Code 走的也是这条协议（见 Kimi Code 文档
        //「Use in Third-Party Tools」）。
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
fn kimi_base_url(configured: &str) -> String {
    const DEFAULT: &str = "https://api.kimi.com/coding";
    let trimmed = configured.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return DEFAULT.to_string();
    }
    let trimmed = trimmed
        .strip_suffix("/v1/messages")
        .or_else(|| trimmed.strip_suffix("/messages"))
        .unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    trimmed.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;

    fn cfg(provider: &str, base_url: &str) -> AgentConfig {
        AgentConfig {
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
        assert!(create_provider(&cfg("gpt-x", "")).is_err());
    }
}
