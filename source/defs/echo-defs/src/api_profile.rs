//! LLM 供应商配置的**定义层词汇**（`ApiProfile`）。
//!
//! ## 为什么在定义层
//!
//! "一个具名供应商配置"（名字 / 服务商 / 模型 / base_url / key / 思考模式）
//! 是**跨边界词汇**：它写在 `core.toml` 的 `[agent.api_profiles]`、由管理面
//! 增删改、被 provider 插件消费、被 `list_api_profiles` 工具投影给模型、
//! 被 subagent 的 `profile` 参数解析。此前它住在 `echo-agent::config`——
//! 于是 `echo-plugin-provider-llm`（provider 工厂的实现方）无法独立读它，
//! API 管理面那一整块（1,208 行）也就搬不出宿主（全插件化 S3 的先决条件）。
//!
//! 定义层是这个词汇的正确位置：它不含任何行为（只有字段与一个构造器），
//! 也不引用宿主类型。`ProviderConfig`（provider 插件里的构建参数）与它是
//! **投影关系**而不是同一件事——前者是"建 provider 要什么"，后者是
//! "用户在配置里写了什么"（多了 `name`，少了 provider 侧的默认推导）。

use serde::{Deserialize, Serialize};

pub use crate::mode::{ReasoningEffort, ThinkingMode};

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

    /// 是否已配置密钥（**只回布尔**——工具输出与状态投影都不得回显明文）。
    pub fn key_set(&self) -> bool {
        !self.api_key.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 缺省反序列化：老配置里没有 thinking / reasoning_effort 时取默认，
    /// 不是报错（`#[serde(default)]` 是配置兼容性的关键）。
    #[test]
    fn missing_optional_fields_fall_back_to_defaults() {
        let parsed: ApiProfile =
            toml::from_str(r#"name = "a"
provider = "openai"
model = "m"
"#)
            .expect("老配置必须能解析");
        assert_eq!(parsed.name, "a");
        assert!(parsed.base_url.is_empty());
        assert!(!parsed.key_set());
        assert_eq!(parsed.thinking, ThinkingMode::default());
    }

    /// `key_set` 只看是否非空，不泄露内容。
    #[test]
    fn key_set_is_a_boolean_not_the_key() {
        let mut profile = ApiProfile::new("a", "openai", "m");
        assert!(!profile.key_set());
        profile.api_key = "sk-secret".into();
        assert!(profile.key_set());
    }
}
