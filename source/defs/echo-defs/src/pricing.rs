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

/// 缺省定价表（DeepSeek / Kimi 官方公开价，2026-10 摘自 api-docs 与
/// platform.kimi.com/docs/pricing；可被 `[agent.pricing]` 覆盖或清空）。
/// 旧模型名 `deepseek-v4-flash` / `-vision-exp` 按 Flash 价计费——前缀匹配
/// 天然覆盖（更具体的条目优先；Kimi 的短名/全名两套命名同理）。
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

    // Kimi 条目构造：输入 / 输出 / 缓存命中（¥/MTok）。Kimi 无空闲折扣。
    let kimi = |model: &str, input: f64, output: f64, hit: f64| ModelPrice {
        model: model.into(),
        input_per_million: input,
        output_per_million: output,
        offpeak_input_per_million: None,
        offpeak_output_per_million: None,
        cache_hit_input_per_million: Some(hit),
        offpeak_cache_hit_input_per_million: None,
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
        // ── Kimi（Moonshot）─────────────────────────────────────────────
        // 官方价：platform.kimi.com/docs/pricing（2026-10-24 校准，¥/MTok）。
        // 口径对齐：Kimi 把「缓存写入(5min)」按**输入价**计费，正对应本框架的
        // 「未命中输入」，故 input_per_million 直接填官方输入价；命中价单列
        // （¥1.30/M 一档）。官方另有「缓存写入(1h) = 2×输入价」，本表无对应
        // 字段，长 TTL 场景仍会低估。Kimi 无空闲时段折扣 → offpeak_* 全 None。
        //
        // 命名两套并存：`/coding` 端点只认短名（k3 / k3.1-pro /
        // k3.1-pro-highspeed），开放平台认全名（kimi-k3.1-pro …）。全名同时被
        // `k3` / `kimi-k3` 这类短前缀命中，靠最长前缀选中正确条目——所以
        // `kimi-*` 别名条目**不能删**：删了 `kimi-k3.1-pro` 会掉到 K3 价的
        // ¥20/¥100 上（虚高 54%）。
        kimi("k3.1-pro", 13.0, 68.0, 1.30),
        kimi("kimi-k3.1-pro", 13.0, 68.0, 1.30),
        kimi("k3.1-pro-highspeed", 26.0, 136.0, 1.30),
        kimi("kimi-k3.1-pro-highspeed", 26.0, 136.0, 1.30),
        // K3：上一代旗舰。此前表内 12/32 是旧口径，官方现价 ¥20/¥100
        // （低估约 60%）；命中价 ¥2.00 不变。
        kimi("k3", 20.0, 100.0, 2.00),
        kimi("kimi-k3", 20.0, 100.0, 2.00),
        // K2 系列（仅开放平台有；/coding 端点不提供）。2026-12 底下线，仍在
        // 用的调用不该零估算。
        kimi("kimi-k2.7-code-highspeed", 13.0, 54.0, 2.60),
        kimi("kimi-k2.7-code", 6.5, 27.0, 1.30),
        kimi("kimi-k2.6", 6.5, 27.0, 1.10),
        // 注：`/coding` 端点的 `kimi-for-coding`（显示名 K2.8 Preview）官方
        // 定价页未列条目——宁可缺失让面板显式报「未计价」，也不臆造数字。
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
        assert!(pricing.iter().any(|p| p.model == "k3.1-pro"));
        assert!(pricing.iter().any(|p| p.model == "k3.1-pro-highspeed"));
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
