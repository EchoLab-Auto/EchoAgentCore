//! 模型定价的**定义层词汇**：`ModelPrice` + 缺省定价表。
//!
//! ## 为什么在定义层
//!
//! 定价是**跨边界配置词汇**：写在 `core.toml` 的 `[agent.pricing]`、被计量
//! 存储用来估算成本、被 `QueryApiMetrics` 回执给面板。此前它住在
//! `echo-agent::config`，于是**计量与成本估算**（`metrics.rs`，716 行纯逻辑）
//! 无法独立于宿主（全插件化 S3 的又一处阻塞点）。
//!
//! 定义层只放词汇与缺省值，不放任何聚合逻辑——`estimate_cost` / 高峰时段
//! 判定那些属于计量插件。

use serde::{Deserialize, Serialize};

/// 模型价格（`[agent.pricing]` 条目；费用估算用，单位 = 每百万 tokens）。
///
/// - `model` 以**最长前缀**匹配请求模型名（大小写不敏感）；
/// - 输入价按"缓存未命中"口径（框架不区分缓存命中，估值是上界）；
/// - 高峰 / 空闲两档：`offpeak_*` 缺省 = 无空闲折扣。DeepSeek 口径为
///   北京时间周一至周五 9:00–12:00、14:00–18:00 高峰，其余时段（含周末）
///   半价；法定节假日的空闲计价未纳入估算（近似）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPrice {
    /// 模型名（前缀匹配；大小写不敏感）。
    pub model: String,
    /// 高峰时段输入价（缓存未命中；每百万 tokens）。
    pub input_per_million: f64,
    /// 高峰时段输出价（每百万 tokens）。
    pub output_per_million: f64,
    /// 空闲时段输入价（None = 与高峰同价）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offpeak_input_per_million: Option<f64>,
    /// 空闲时段输出价（None = 与高峰同价）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offpeak_output_per_million: Option<f64>,
    /// 高峰时段缓存命中输入价（None = 不支持缓存命中计价，按未命中价）。
    /// DeepSeek 官方价：deepseek-flash 0.04 / deepseek-v4-pro 0.30。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit_input_per_million: Option<f64>,
    /// 空闲时段缓存命中输入价（None = 与高峰同价）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offpeak_cache_hit_input_per_million: Option<f64>,
    /// 币种（默认 "CNY"）。
    pub currency: String,
}

impl Default for ModelPrice {
    fn default() -> Self {
        Self {
            model: String::new(),
            input_per_million: 0.0,
            output_per_million: 0.0,
            offpeak_input_per_million: None,
            offpeak_output_per_million: None,
            cache_hit_input_per_million: None,
            offpeak_cache_hit_input_per_million: None,
            currency: "CNY".into(),
        }
    }
}

/// 缺省定价表（DeepSeek 官方公开价，2026-10 摘自 api-docs；可被
/// `[agent.pricing]` 覆盖或清空）。旧模型名 `deepseek-v4-flash` /
/// `-vision-exp` 按 Flash 价计费——前缀匹配天然覆盖（更具体的条目优先）。
pub fn default_pricing() -> Vec<ModelPrice> {
    let flash = |model: &str| ModelPrice {
        model: model.into(),
        input_per_million: 2.0,
        output_per_million: 8.0,
        offpeak_input_per_million: Some(1.0),
        offpeak_output_per_million: Some(4.0),
        cache_hit_input_per_million: Some(0.04),
        offpeak_cache_hit_input_per_million: Some(0.02),
        currency: "CNY".into(),
    };
    vec![
        flash("deepseek-flash"),
        flash("deepseek-v4-flash"),
        ModelPrice {
            model: "deepseek-v4-pro".into(),
            input_per_million: 9.0,
            output_per_million: 27.0,
            offpeak_input_per_million: Some(4.5),
            offpeak_output_per_million: Some(13.5),
            cache_hit_input_per_million: Some(0.30),
            offpeak_cache_hit_input_per_million: Some(0.15),
            currency: "CNY".into(),
        },
        // Kimi K3（2026-10 审计：未计价条目——k3 的 4.5M tokens 此前零估算）。
        // 基础价按原口径（约 ¥12/¥32 每百万；官方 K3 API 定价页 2026-10 为
        // ¥20/¥100，订阅产品请以实际套餐为准）。**缓存命中价补 ¥2/M**：
        // 官方定价页公开（platform.kimi.com/docs/pricing）；缺失会随缓存
        // 读取量按未命中价估算，严重虚高（2026-10 归一修复配套）。
        ModelPrice {
            model: "k3".into(),
            input_per_million: 12.0,
            output_per_million: 32.0,
            offpeak_input_per_million: None,
            offpeak_output_per_million: None,
            cache_hit_input_per_million: Some(2.0),
            offpeak_cache_hit_input_per_million: None,
            currency: "CNY".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 缺省表按前缀可覆盖：更具体的条目在前（`deepseek-v4-flash` 先于
    /// `deepseek-flash` 命中——调用方按序最长前缀匹配）。
    #[test]
    fn default_pricing_covers_the_configured_models() {
        let pricing = default_pricing();
        assert!(pricing.iter().any(|p| p.model == "deepseek-flash"));
        assert!(pricing.iter().any(|p| p.model == "deepseek-v4-pro"));
        assert!(pricing.iter().any(|p| p.model == "k3"));
        assert!(
            pricing.iter().all(|p| p.currency == "CNY"),
            "缺省币种必须统一（否则成本合计会串币种）"
        );
    }

    /// 缺省值：未知字段取 Default（币种 CNY、价格 0），不是报错。
    #[test]
    fn model_price_defaults_are_documented() {
        let price = ModelPrice::default();
        assert_eq!(price.currency, "CNY");
        assert_eq!(price.input_per_million, 0.0);
        assert!(price.offpeak_input_per_million.is_none());
    }
}
