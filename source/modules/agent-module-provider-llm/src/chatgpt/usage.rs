//! ChatGPT 订阅的额度查询（`/backend-api/codex/usage`）。
//!
//! # 端点选择
//!
//! ChatGPT 订阅的额度信息**没有公开 API**，但官方 Codex CLI 用的内部端点
//! 已经实测可用（2026-11-18，本机 prolite 账号）：
//!
//! ```text
//! GET https://chatgpt.com/backend-api/codex/usage
//! Authorization: Bearer <access_token>
//! chatgpt-account-id: <account_id>
//! originator: codex_cli_rs
//! ```
//!
//! 返回的关键字段：
//!
//! - `plan_type`：套餐（`prolite` / `pro` / `plus` / …）
//! - `rate_limit.allowed`：现在能否调用（配额用尽 = false）
//! - `rate_limit.limit_reached`：是否触顶
//! - `rate_limit.primary_window.used_percent`：当前窗口已用百分比（0-100）
//! - `rate_limit.primary_window.reset_at`：下次重置的 Unix 秒时间戳
//! - `rate_limit.primary_window.reset_after_seconds`：距离重置还有多少秒
//! - `model_usage.<model>.available`：按模型分的可用性
//!
//! `/backend-api/wham/usage` 返回结构相同，可作备用。
//!
//! # 为什么单独一个模块
//!
//! - 端点是 ChatGPT 订阅通道**独有**的 HTTP 知识，和 auth/responses 平级。
//! - 调用方（`balance::fetch_balance`）只需要一个 `fetch_usage()` 入口，
//!   不需要知道 endpoint 形状、字段名、header 三件套。
//! - 复用 `auth::ensure_fresh()` 与 `auth::build_client()`，代理与令牌
//!   刷新逻辑零重复。

use super::auth;

/// 用量查询结果（保留供上层格式化的原始字段，不做单位换算）。
#[derive(Debug, Clone, Default)]
pub struct ChatGptUsage {
    /// 套餐类型（`prolite` / `pro` / `plus` / `free` / `go` / …）。
    pub plan_type: String,
    /// 现在能否调用（`rate_limit.allowed`）。
    pub allowed: bool,
    /// 是否已触顶（`rate_limit.limit_reached`）。
    pub limit_reached: bool,
    /// 当前窗口已用百分比（0-100）。窗口由 `window_seconds` 描述。
    pub used_percent: u32,
    /// 主配额窗口长度（秒）。prolite = 604800（7 天）。
    pub window_seconds: u64,
    /// 下次重置的 Unix 秒时间戳；0 = 未知。
    pub reset_at: u64,
    /// 距离重置还有多少秒；0 = 未知或立即可用。
    pub reset_after_seconds: u64,
    /// 用户邮箱（展示用）。
    pub email: String,
}

impl ChatGptUsage {
    /// 把人话摘要写进 `ApiBalanceResult.message`。
    ///
    /// 形如：
    /// - 可用：`ChatGPT 订阅 prolite 已用 32%，7 天窗口，2026-11-21 18:33 重置`
    /// - 触顶：`ChatGPT 订阅 prolite 配额已用尽（100%），2026-11-21 18:33 重置（约 74 小时后）`
    pub fn human_summary(&self) -> String {
        let plan = if self.plan_type.is_empty() {
            "未知套餐"
        } else {
            self.plan_type.as_str()
        };
        let reset_text = if self.reset_at > 0 {
            let dt = chrono::DateTime::from_timestamp(self.reset_at as i64, 0)
                .map(|utc| utc.with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap()));
            match dt {
                Some(local) => local.format("%Y-%m-%d %H:%M").to_string(),
                None => format!("unix {}", self.reset_at),
            }
        } else {
            "未知".to_string()
        };
        let countdown = if self.reset_after_seconds > 0 {
            let hours = self.reset_after_seconds / 3600;
            let mins = (self.reset_after_seconds % 3600) / 60;
            if hours > 0 {
                format!("（约 {} 小时 {} 分后）", hours, mins)
            } else {
                format!("（约 {} 分钟后）", mins)
            }
        } else {
            String::new()
        };
        let window = if self.window_seconds > 0 {
            let days = self.window_seconds / 86400;
            if days > 0 {
                format!("{} 天窗口，", days)
            } else {
                format!("{} 秒窗口，", self.window_seconds)
            }
        } else {
            String::new()
        };
        if self.limit_reached || !self.allowed {
            format!(
                "ChatGPT 订阅 {plan} 配额已用尽（{}%），{window}{reset_text} 重置{countdown}",
                self.used_percent
            )
        } else {
            format!(
                "ChatGPT 订阅 {plan} 已用 {}%，{window}{reset_text} 重置",
                self.used_percent
            )
        }
    }
}

/// 端点常量。`/backend-api/wham/usage` 是 Codex CLI 内部别名，结构相同；
/// 优先 codex 路径（语义更明确），失败再回退 wham。
const USAGE_ENDPOINTS: &[&str] = &[
    "https://chatgpt.com/backend-api/codex/usage",
    "https://chatgpt.com/backend-api/wham/usage",
];

/// 拉取当前账号的订阅额度。
///
/// - 未登录 → 明确的错误（沿用 `ensure_fresh()` 的提示）
/// - 令牌过期 → `ensure_fresh()` 内部自动刷新并落盘
/// - 两个 endpoint 都失败 → 把最后一次的错误返回（带 status / body 截断）
///
/// 15s 超时（与 `balance::fetch_balance` 对齐）。
pub async fn fetch_usage() -> Result<ChatGptUsage, String> {
    let auth = auth::ensure_fresh().await?;
    if !auth.usable() {
        return Err("ChatGPT 令牌不完整（缺 account_id），请重新登录".into());
    }
    let client = auth::build_client()?;

    let mut last_err = String::from("未发起请求");
    for endpoint in USAGE_ENDPOINTS {
        match fetch_one(&client, endpoint, &auth).await {
            Ok(usage) => return Ok(usage),
            Err(e) => {
                last_err = format!("{endpoint}: {e}");
            }
        }
    }
    Err(format!("ChatGPT 额度查询失败：{last_err}"))
}

async fn fetch_one(
    client: &reqwest::Client,
    endpoint: &str,
    auth: &auth::ChatGptAuth,
) -> Result<ChatGptUsage, String> {
    let resp = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        client
            .get(endpoint)
            .bearer_auth(&auth.access_token)
            .header("chatgpt-account-id", &auth.account_id)
            .header("originator", "codex_cli_rs")
            .header("accept", "application/json")
            .send()
            .await
    })
    .await
    .map_err(|_| "请求超时（>15s）".to_string())?
    .map_err(|e| format!("请求失败: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("读响应失败: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "HTTP {status}: {}",
            echo_defs::token::truncate(&text, 200)
        ));
    }
    parse_usage(&text).map_err(|e| format!("解析失败: {e}"))
}

/// 解析 `/codex/usage` 的响应体。
///
/// 用 `serde_json::Value` 而不是强类型 struct：上游字段经常在变（次级窗口、
/// 按模型分的子配额），强类型会让"新增一个无关字段"变成反序列化失败。
/// 这里只取我们真正用的几个 key。
fn parse_usage(body: &str) -> Result<ChatGptUsage, serde_json::Error> {
    let root: serde_json::Value = serde_json::from_str(body)?;
    let mut out = ChatGptUsage {
        plan_type: root
            .get("plan_type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        email: root
            .get("email")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        // 缺 rate_limit 时按"可用"兜底：上游裁剪字段不该让面板显示"配额已用尽"。
        allowed: true,
        ..Default::default()
    };
    if let Some(rate) = root.get("rate_limit") {
        out.allowed = rate
            .get("allowed")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        out.limit_reached = rate
            .get("limit_reached")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(primary) = rate.get("primary_window") {
            out.used_percent = primary
                .get("used_percent")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            out.window_seconds = primary
                .get("limit_window_seconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            out.reset_at = primary
                .get("reset_at")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            out.reset_after_seconds = primary
                .get("reset_after_seconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实测样本（prolite，2026-11-18 配额触顶）。
    const SAMPLE: &str = r#"{
        "user_id": "user-x",
        "account_id": "acc-x",
        "email": "e@x.com",
        "plan_type": "prolite",
        "rate_limit": {
            "allowed": false,
            "limit_reached": true,
            "primary_window": {
                "used_percent": 100,
                "limit_window_seconds": 604800,
                "reset_after_seconds": 266897,
                "reset_at": 1795247617
            },
            "secondary_window": null
        },
        "model_usage": {
            "gpt-6-astra": {"available": false}
        }
    }"#;

    #[test]
    fn parses_real_payload() {
        let usage = parse_usage(SAMPLE).expect("sample parses");
        assert_eq!(usage.plan_type, "prolite");
        assert!(!usage.allowed);
        assert!(usage.limit_reached);
        assert_eq!(usage.used_percent, 100);
        assert_eq!(usage.window_seconds, 604800);
        assert_eq!(usage.reset_at, 1795247617);
        assert_eq!(usage.reset_after_seconds, 266897);
        assert_eq!(usage.email, "e@x.com");
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // 上游裁剪字段时不要挂：缺啥补默认值。
        let usage = parse_usage("{}").expect("empty parses");
        assert!(usage.allowed, "缺 rate_limit 时按可用兜底");
        assert_eq!(usage.used_percent, 0);
        assert_eq!(usage.reset_at, 0);
    }

    #[test]
    fn summary_when_limit_reached_mentions_reset_time() {
        let usage = parse_usage(SAMPLE).unwrap();
        let text = usage.human_summary();
        assert!(text.contains("prolite"), "{text}");
        assert!(text.contains("100%"), "{text}");
        assert!(text.contains("配额已用尽"), "{text}");
        assert!(text.contains("重置"), "{text}");
        assert!(text.contains("小时"), "{text}");
    }

    #[test]
    fn summary_when_available_skips_alarm_wording() {
        let mut usage = parse_usage(SAMPLE).unwrap();
        usage.allowed = true;
        usage.limit_reached = false;
        usage.used_percent = 32;
        let text = usage.human_summary();
        assert!(text.contains("已用 32%"), "{text}");
        assert!(!text.contains("配额已用尽"), "{text}");
    }
}
