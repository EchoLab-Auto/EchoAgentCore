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

/// 供应商池 + 顶层默认的**只读快照**（`list_api_profiles` 输出与子代理
/// profile 解析共用）。
///
/// ## 为什么在定义层（2026-11 S12）
///
/// 它描述的是"用户在配置里写了什么"——与 [`ApiProfile`] 同源。此前它住在
/// `echo-plugin-provider-llm::api_pool`，于是 `echo-plugin-tools` 的
/// `list_api_profiles` 工具不得不 `use echo_plugin_provider_llm::...`：
/// **一个"读配置并投影给模型"的工具，因为一个结构体依赖了 provider 插件**。
///
/// 输出**绝不含明文密钥**：`default_key_set` / `ApiProfile.key_set` 只是
/// 布尔"是否已配置"。
#[derive(Debug, Default, Clone)]
pub struct PoolSnapshot {
    /// 池中全部 profile（原序）。
    pub profiles: Vec<ApiProfile>,
    /// 全局激活 profile 名（`[agent].active_api`；空 = 顶层默认）。
    pub active_api: String,
    /// 顶层默认 provider（无 profile / 未命中的兜底）。
    pub default_provider: String,
    /// 顶层默认 model。
    pub default_model: String,
    /// 顶层默认是否已配置 api_key（仅布尔，不回显明文）。
    pub default_key_set: bool,
}

/// 从**已解析的配置文档**读取供应商池快照。
///
/// 只吃 `toml::Value` 而不是 `ConfigStore`：读取文件是宿主的活，解析配置形状
/// 是词汇的事。于是消费方（工具插件 / 宿主 / 测试）都能用同一份解析器，
/// 而不必互相依赖。
///
/// - `Ok` = 读到（`[agent]` 段缺失按空池处理——全新部署尚未落盘）；
/// - `Err(message)` = `api_profiles` 存在但形状不对。
pub fn parse_pool(document: &toml::Value) -> Result<PoolSnapshot, String> {
    let agent = document.get("agent");
    let profiles = agent
        .and_then(|agent| agent.get("api_profiles"))
        .cloned()
        .unwrap_or_else(|| toml::Value::Array(Vec::new()))
        .try_into::<Vec<ApiProfile>>()
        .map_err(|e| format!("供应商池解析失败: {e}"))?;
    let get_str = |key: &str| -> String {
        agent
            .and_then(|agent| agent.get(key))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let key_set = |key: &str| -> bool {
        agent
            .and_then(|agent| agent.get(key))
            .and_then(|v| v.as_str())
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    };
    Ok(PoolSnapshot {
        profiles,
        active_api: get_str("active_api"),
        default_provider: get_str("provider"),
        default_model: get_str("model"),
        default_key_set: key_set("api_key"),
    })
}
