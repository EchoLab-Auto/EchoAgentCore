//! LLM provider 接缝 —— 宿主 glue（实现已外迁）。
//!
//! 解耦计划 Phase 3 ①：provider 工厂、计量与脱敏装饰器的**实现**已迁至独立
//! crate [`echo_plugin_provider_llm`]（只依赖定义层）。本模块只做两件宿主
//! 侧的事，除此之外不承载任何 provider 逻辑：
//!
//! 1. **配置投影**：把 [`AgentConfig`] 的子集映射成 `ProviderConfig`
//!    （配置文件的形状——profiles / teams / pricing——不进插件）；
//! 2. **指标出口**：[`MetricsStore`] 实现 [`UsageRecorder`]，供计量装饰器
//!    上报 token 用量（存储形状与成本估算留在宿主）。
//!
//! 公开路径与函数签名保持原样（`echo_agent::llm::create_provider` 等），
//! 既有调用点零改动。

pub use echo_defs::llm::{LlmError, LlmProvider};
pub use echo_defs::message::{
    ChatChunk, ChatMessage, ChatRequest, ChatResponse, ChatRole, ToolCall, ToolCallDelta, Usage,
};
pub use echo_defs::token::{
    estimate_history_tokens, estimate_message_tokens, estimate_tokens, truncate,
    truncate_message_to_tokens, truncate_text_to_tokens,
};
pub use echo_defs::tool::ToolDefinition;
pub use echo_plugin_provider_llm::{kimi_base_url, ProviderConfig, UsageRecorder};

use std::sync::Arc;

use crate::config::AgentConfig;
use crate::metrics::MetricsStore;

/// 把 `AgentConfig` 投影成插件侧的 provider 配置（只取 provider 相关字段）。
fn project(cfg: &AgentConfig) -> ProviderConfig {
    ProviderConfig {
        provider: cfg.provider.clone(),
        base_url: cfg.base_url.clone(),
        model: cfg.model.clone(),
        api_key: cfg.api_key.clone(),
        thinking: cfg.thinking,
        reasoning_effort: cfg.reasoning_effort,
    }
}

/// 按配置构建 provider（Phase 3 ① 之后为宿主 glue + 插件实现）。
///
/// base_url 以 `/anthropic` 结尾时自动走 Anthropic Messages 客户端，
/// 即使 provider 名为 openai/deepseek——判定逻辑在插件 crate 内。
pub fn create_provider(cfg: &AgentConfig) -> Result<Box<dyn LlmProvider>, LlmError> {
    echo_plugin_provider_llm::create_provider(&project(cfg))
}

/// 用计量装饰器包装 provider（token 用量记录；余额/用量图表数据源）。
///
/// 外壳与脱敏装饰器同一装配点：禁止裸装配（见 `agent/gating.rs`）。
pub fn wrap_metering(
    inner: Arc<dyn LlmProvider>,
    store: Arc<MetricsStore>,
    profile: String,
) -> Arc<dyn LlmProvider> {
    echo_plugin_provider_llm::wrap_metering(inner, store, profile)
}

/// 用脱敏器包装 provider（LLM 请求出口的敏感信息卡口）。
pub use echo_plugin_provider_llm::wrap_redacting;

/// 指标存储即用量出口：计量装饰器只认 [`UsageRecorder`]，
/// 不认 `echo-balances.jsonl` / 保留期 / 定价表（那些属宿主的指标子系统）。
impl UsageRecorder for MetricsStore {
    fn record_usage(
        &self,
        profile: &str,
        model: &str,
        prompt: u32,
        completion: u32,
        cache_hit: u32,
    ) {
        MetricsStore::record_usage(self, profile, model, prompt, completion, cache_hit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ReasoningEffort, ThinkingMode};

    fn cfg(provider: &str, base_url: &str) -> AgentConfig {
        AgentConfig {
            provider: provider.into(),
            base_url: base_url.into(),
            model: "test-model".into(),
            api_key: "test-key".into(),
            ..Default::default()
        }
    }

    /// 投影是**逐字段拷贝**，不漏思考模式/推理强度（否则 DeepSeek 思考
    /// 会在外迁后静默失效）。
    #[test]
    fn projection_carries_reasoning_fields() {
        let mut c = cfg("deepseek", "https://api.deepseek.com");
        c.thinking = ThinkingMode::Disabled;
        c.reasoning_effort = ReasoningEffort::High;
        let p = project(&c);
        assert_eq!(p.provider, "deepseek");
        assert_eq!(p.base_url, "https://api.deepseek.com");
        assert_eq!(p.model, "test-model");
        assert_eq!(p.api_key, "test-key");
        assert_eq!(p.thinking, ThinkingMode::Disabled);
        assert_eq!(p.reasoning_effort, ReasoningEffort::High);
    }

    #[test]
    fn create_provider_routes_by_endpoint_through_glue() {
        assert_eq!(
            create_provider(&cfg("deepseek", "https://api.deepseek.com/anthropic"))
                .unwrap()
                .name(),
            "anthropic"
        );
        assert_eq!(
            create_provider(&cfg("deepseek", "https://api.deepseek.com"))
                .unwrap()
                .name(),
            "openai"
        );
    }

    #[test]
    fn empty_provider_reports_actionable_config_error() {
        let err = match create_provider(&cfg("", "")) {
            Ok(_) => panic!("empty provider must be a config error"),
            Err(err) => err,
        };
        assert!(format!("{err}").contains("no LLM provider configured"));
    }

    /// 指标存储经 trait 出口记录（外迁后计量仍落到同一 JSONL 语义）。
    #[test]
    fn metrics_store_implements_usage_recorder() {
        let dir = std::env::temp_dir().join(format!(
            "echo-agent-llm-shim-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store: Arc<dyn UsageRecorder> = Arc::new(MetricsStore::new(&dir));
        store.record_usage("profile-a", "model-a", 10, 5, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
