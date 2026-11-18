//! 账户余额查询（DeepSeek `/user/balance`）。
//!
//! ## 为什么归本模块
//!
//! 这是 **provider 的 HTTP 知识**：端点的形状由供应商决定（`/anthropic`、
//! `/v1`、`/beta` 后缀都要归一到根域）、响应字段由供应商决定
//! （`balance_infos[].total_balance` 的字符串金额）。
//!
//! 它此前住在宿主 `agent/api_admin.rs`（约 110 行），与「读 `AgentConfig` /
//! 重建 provider / 广播事件」混在一起——于是"DeepSeek 的余额接口怎么算"
//! 这条知识在一个跟 provider 无关的模块里。
//!
//! 现在只剩一件事是宿主的：把 `AgentConfig` 投影成 [`ProviderConfig`]
//! （见 `echo_agent::agent::api_admin::fetch_balance` 的适配器）。

use echo_defs::token::truncate;

use crate::ProviderConfig;

/// 一次余额查询的原始结果（金额是**字符串**：供应商可能回 `"1287.91"`，
/// 也可能回 `"0"`；保留原文，交给宿主决定怎么格式化与记录）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BalanceFetch {
    /// 账户是否可用（供应商回的 `is_available`）。
    pub available: bool,
    /// 总余额。
    pub total: String,
    /// 赠送余额。
    pub granted: String,
    /// 充值余额。
    pub topped_up: String,
    /// 币种（`CNY` / `USD`）。
    pub currency: String,
}

/// 从 `base_url` 推导 DeepSeek 余额查询端点。
///
/// `https://api.deepseek.com/anthropic` / `.../v1` / `.../beta` / 裸域
/// 统一映射为 `{root}/user/balance`；非 DeepSeek 域返回 `None`（余额接口
/// 是供应商专有的，没有通用形态可猜）。
pub fn deepseek_balance_endpoint(base_url: &str) -> Option<String> {
    let mut root = base_url.trim().trim_end_matches('/');
    for suffix in ["/anthropic", "/v1", "/beta"] {
        if let Some(stripped) = root.strip_suffix(suffix) {
            root = stripped;
            break;
        }
    }
    if root.contains("deepseek.com") {
        Some(format!("{root}/user/balance"))
    } else {
        None
    }
}

/// 宽松解析金额字符串（失败 = 0.0；不阻断快照记录）。
///
/// 「解析失败不该让整条快照失败」是刻意的：供应商偶尔回 `"-"` 或空串，
/// 那时我们宁可记 0 也不丢这一时刻的数据点。
pub fn parse_amount(text: &str) -> f64 {
    text.trim().parse::<f64>().unwrap_or(0.0)
}

/// 请求供应商余额/额度（15s 超时；**无副作用**——不 emit、不记录，
/// 供手动查询与周期快照共用）。
///
/// 按 provider 分流：
/// - `openai-chatgpt`：走 [`crate::chatgpt::usage`]，把配额百分比塞进 `total`，
///   `plan_type` 塞进 `currency`，`granted` / `topped_up` 留空——语义对齐
///   「总余额 / 币种」但不真的是钱。人话摘要由宿主 `api_admin` 拼。
/// - 其它（DeepSeek）：走 `/user/balance`。
pub async fn fetch_balance(probe: &ProviderConfig) -> Result<BalanceFetch, String> {
    if probe.provider == "openai-chatgpt" {
        return fetch_chatgpt_quota().await;
    }
    fetch_deepseek_balance(probe).await
}

/// ChatGPT 订阅额度 → `BalanceFetch` 的窄投影。
///
/// 字段借用（语义上不是钱，但能让现有 `ApiBalanceResult` 直接携带）：
/// - `available` = `rate_limit.allowed`（配额触顶 = false）
/// - `total` = `used_percent` 字符串（如 `"100"`）
/// - `currency` = `plan_type`（如 `"prolite"`）
/// - `granted` = `reset_at`（Unix 秒字符串；给上层拼人话用）
/// - `topped_up` = `reset_after_seconds`
async fn fetch_chatgpt_quota() -> Result<BalanceFetch, String> {
    let usage = crate::chatgpt::usage::fetch_usage().await?;
    Ok(BalanceFetch {
        available: usage.allowed,
        total: usage.used_percent.to_string(),
        granted: usage.reset_at.to_string(),
        topped_up: usage.reset_after_seconds.to_string(),
        currency: usage.plan_type,
    })
}

/// 请求 DeepSeek `/user/balance`（15s 超时；**无副作用**）。
async fn fetch_deepseek_balance(probe: &ProviderConfig) -> Result<BalanceFetch, String> {
    #[derive(serde::Deserialize)]
    struct BalanceResponse {
        #[serde(default)]
        is_available: bool,
        #[serde(default)]
        balance_infos: Vec<BalanceInfo>,
    }
    #[derive(serde::Deserialize)]
    struct BalanceInfo {
        #[serde(default)]
        currency: String,
        #[serde(default)]
        total_balance: String,
        #[serde(default)]
        granted_balance: String,
        #[serde(default)]
        topped_up_balance: String,
    }

    let Some(endpoint) = deepseek_balance_endpoint(&probe.base_url) else {
        return Err(format!(
            "余额查询仅支持 DeepSeek 官方端点（当前 base_url: {}）",
            probe.base_url
        ));
    };
    let api_key = probe.api_key.trim();
    if api_key.is_empty() {
        return Err("缺少 API Key，无法查询余额".into());
    }

    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("HTTP client build failed: {e}"))?;
        let response = client
            .get(&endpoint)
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(|e| format!("请求失败: {e}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("读取响应失败: {e}"))?;
        if !status.is_success() {
            return Err(format!("HTTP {status}: {}", truncate(&text, 200)));
        }
        let parsed: BalanceResponse = serde_json::from_str(&text)
            .map_err(|e| format!("响应解析失败: {e} — body: {}", truncate(&text, 200)))?;
        Ok::<BalanceResponse, String>(parsed)
    })
    .await;

    match result {
        Ok(Ok(parsed)) => {
            let info = parsed.balance_infos.first();
            Ok(BalanceFetch {
                available: parsed.is_available,
                total: info.map(|i| i.total_balance.clone()).unwrap_or_default(),
                granted: info.map(|i| i.granted_balance.clone()).unwrap_or_default(),
                topped_up: info
                    .map(|i| i.topped_up_balance.clone())
                    .unwrap_or_default(),
                currency: info.map(|i| i.currency.clone()).unwrap_or_default(),
            })
        }
        Ok(Err(message)) => Err(message),
        Err(_) => Err("请求超时（>15s）".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端点归一：三种常见后缀都要映射回根域。
    ///
    /// 这条知识来自真实故障——用户把 base_url 填成 `/anthropic`（Anthropic
    /// 兼容端点）后，余额查询拼出 `.../anthropic/user/balance` 而 404。
    #[test]
    fn endpoint_normalises_provider_suffixes() {
        for base in [
            "https://api.deepseek.com",
            "https://api.deepseek.com/",
            "https://api.deepseek.com/v1",
            "https://api.deepseek.com/beta",
            "https://api.deepseek.com/anthropic",
        ] {
            assert_eq!(
                deepseek_balance_endpoint(base).as_deref(),
                Some("https://api.deepseek.com/user/balance"),
                "base_url = {base}"
            );
        }
        // 非 DeepSeek 域：余额接口是供应商专有的，不猜。
        assert!(deepseek_balance_endpoint("https://api.openai.com/v1").is_none());
        assert!(deepseek_balance_endpoint("http://127.0.0.1:8080/ollama").is_none());
    }

    /// 金额解析宽松：解析不了记 0（不阻断快照）。
    #[test]
    fn amount_parsing_is_lenient() {
        assert_eq!(parse_amount("1287.91"), 1287.91);
        assert_eq!(parse_amount(" 0 "), 0.0);
        assert_eq!(parse_amount(""), 0.0);
        assert_eq!(parse_amount("-"), 0.0);
        assert_eq!(parse_amount("n/a"), 0.0);
    }

    /// 非 DeepSeek 端点直接报错，且错误里带上当前 base_url（排查用）。
    #[tokio::test]
    async fn non_deepseek_endpoint_fails_with_context() {
        let probe = ProviderConfig {
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-x".into(),
            ..Default::default()
        };
        let error = fetch_balance(&probe)
            .await
            .expect_err("非 DeepSeek 必须失败");
        assert!(error.contains("api.example.com"), "{error}");
    }

    /// 缺 key 时不去打网络（避免把"没配 key"报成网络错误）。
    #[tokio::test]
    async fn missing_key_fails_before_any_request() {
        let probe = ProviderConfig {
            base_url: "https://api.deepseek.com".into(),
            api_key: String::new(),
            ..Default::default()
        };
        let error = fetch_balance(&probe).await.expect_err("缺 key 必须失败");
        assert!(error.contains("API Key"), "{error}");
    }
}
