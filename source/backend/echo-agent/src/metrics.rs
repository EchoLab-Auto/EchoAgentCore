//! API 指标本地积累（余额快照 + token 用量）——余额/用量图表的数据源。
//!
//! 余额与用量都是**只增不改**的时间序列，落两个 JSONL 文件（与 core.toml
//! 同目录）：
//!
//! - `echo-balances.jsonl`：周期任务（`[agent].balance_snapshot_secs`）与
//!   手动「查余额」成功后的余额快照；
//! - `echo-usage.jsonl`：每次 LLM 调用的 token 用量（经 provider 计量
//!   装饰器记录，覆盖内置循环 / echo-loop / 子代理 / 压缩等全部出口）。
//!
//! 面板经 `QueryApiMetrics` 读取近 [`METRICS_WINDOW_MS`] 窗口：余额原样、
//! 用量按小时 × 模型聚合，并附 `[agent.pricing]` 定价表的费用估算。文件按
//! 保留期剪枝（每 [`PRUNE_EVERY`] 次追加触发一次，低频重写）。

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config::ModelPrice;

/// 面板读取窗口：近 7 天。
pub const METRICS_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// 余额快照保留期：30 天（10 分钟周期 ≈ 4320 点）。
const BALANCE_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// 用量记录保留期：90 天。
const USAGE_RETENTION_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// 追加次数达到该值时做一次剪枝检查（把过期行重写掉）。
const PRUNE_EVERY: u64 = 512;

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 单条余额快照（内存形态）。
#[derive(Debug, Clone, PartialEq)]
pub struct BalancePoint {
    pub ts_ms: i64,
    pub total: f64,
    pub granted: f64,
    pub topped_up: f64,
    pub currency: String,
}

/// 单条用量聚合（小时桶 × 模型）。
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSlice {
    pub ts_ms: i64,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub calls: u64,
}

/// `echo-balances.jsonl` 行格式。
#[derive(Debug, Serialize, Deserialize)]
struct BalanceRecord {
    ts_ms: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    currency: String,
    total: f64,
    #[serde(default)]
    granted: f64,
    #[serde(default)]
    topped_up: f64,
}

/// `echo-usage.jsonl` 行格式。
#[derive(Debug, Serialize, Deserialize)]
struct UsageRecord {
    ts_ms: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

/// 剪枝扫描用的最小行（只取时间戳）。
#[derive(Debug, Deserialize)]
struct TsOnly {
    ts_ms: i64,
}

/// 单个 JSONL 文件的追加端（fd + 追加计数 + 剪枝）。
struct Sink {
    path: PathBuf,
    retention_ms: i64,
    appends: u64,
    file: Option<File>,
}

impl Sink {
    fn new(path: PathBuf, retention_ms: i64) -> Self {
        Self {
            path,
            retention_ms,
            appends: 0,
            file: None,
        }
    }

    fn append_line(&mut self, line: &str) {
        if self.file.is_none() {
            match self.open() {
                Ok(f) => self.file = Some(f),
                Err(error) => {
                    tracing::warn!(path = %self.path.display(), %error, "metrics: open failed");
                    return;
                }
            }
        }
        if let Some(file) = self.file.as_mut() {
            if let Err(error) = file
                .write_all(line.as_bytes())
                .and_then(|_| file.write_all(b"\n"))
            {
                tracing::warn!(path = %self.path.display(), %error, "metrics: append failed");
                self.file = None;
                return;
            }
        }
        self.appends = self.appends.wrapping_add(1);
        if self.appends.is_multiple_of(PRUNE_EVERY) {
            self.prune();
        }
    }

    fn open(&self) -> std::io::Result<File> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
    }

    /// 过期行剪枝：读全文件、保留未过期行、原子重写。文件不大（≤ 数 MB），
    /// 低频（每 `PRUNE_EVERY` 次追加）执行，代价可忽略。
    fn prune(&mut self) {
        let cutoff = now_ms() - self.retention_ms;
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(_) => return,
        };
        let mut kept = String::with_capacity(text.len());
        let mut dropped = 0usize;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<TsOnly>(line) {
                Ok(rec) if rec.ts_ms < cutoff => dropped += 1,
                _ => {
                    kept.push_str(line);
                    kept.push('\n');
                }
            }
        }
        if dropped == 0 {
            return;
        }
        let tmp = self.path.with_extension("jsonl.prune-tmp");
        if fs::write(&tmp, kept).is_ok() {
            let _ = fs::rename(&tmp, &self.path);
            // 重命名后旧 fd 指向旧 inode——丢弃，下次追加重开。
            self.file = None;
            tracing::info!(path = %self.path.display(), dropped, "metrics: pruned expired rows");
        }
    }
}

/// 进程级指标存储（组合根构建，注入 Agent 与 provider 装饰器）。
pub struct MetricsStore {
    balances_path: PathBuf,
    usage_path: PathBuf,
    balances: Mutex<Sink>,
    usage: Mutex<Sink>,
}

impl MetricsStore {
    /// `dir` = 数据目录（与 core.toml 同目录；文件在首次写入时创建）。
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let balances_path = dir.join("echo-balances.jsonl");
        let usage_path = dir.join("echo-usage.jsonl");
        Self {
            balances: Mutex::new(Sink::new(balances_path.clone(), BALANCE_RETENTION_MS)),
            usage: Mutex::new(Sink::new(usage_path.clone(), USAGE_RETENTION_MS)),
            balances_path,
            usage_path,
        }
    }

    fn lock<'a>(sink: &'a Mutex<Sink>) -> std::sync::MutexGuard<'a, Sink> {
        sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 记录一次成功的余额快照（周期任务 / 手动查询共用）。
    pub fn record_balance(
        &self,
        name: &str,
        currency: &str,
        total: f64,
        granted: f64,
        topped_up: f64,
    ) {
        let record = BalanceRecord {
            ts_ms: now_ms(),
            name: name.to_string(),
            currency: currency.to_string(),
            total,
            granted,
            topped_up,
        };
        match serde_json::to_string(&record) {
            Ok(line) => Self::lock(&self.balances).append_line(&line),
            Err(error) => tracing::warn!(%error, "metrics: serialize balance failed"),
        }
    }

    /// 记录一次 LLM 调用的 token 用量（计量装饰器调用）。
    pub fn record_usage(
        &self,
        name: &str,
        model: &str,
        prompt_tokens: u32,
        completion_tokens: u32,
    ) {
        let record = UsageRecord {
            ts_ms: now_ms(),
            name: name.to_string(),
            model: model.to_string(),
            prompt_tokens: u64::from(prompt_tokens),
            completion_tokens: u64::from(completion_tokens),
        };
        match serde_json::to_string(&record) {
            Ok(line) => Self::lock(&self.usage).append_line(&line),
            Err(error) => tracing::warn!(%error, "metrics: serialize usage failed"),
        }
    }

    /// 某 profile 自 `since_ms` 起的余额快照（时间升序）。
    pub fn balance_points(&self, name: &str, since_ms: i64) -> Vec<BalancePoint> {
        let mut points = Vec::new();
        let Ok(file) = File::open(&self.balances_path) else {
            return points;
        };
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<BalanceRecord>(&line) {
                Ok(rec) if rec.name == name && rec.ts_ms >= since_ms => {
                    points.push(BalancePoint {
                        ts_ms: rec.ts_ms,
                        total: rec.total,
                        granted: rec.granted,
                        topped_up: rec.topped_up,
                        currency: rec.currency,
                    });
                }
                _ => {}
            }
        }
        points.sort_by_key(|p| p.ts_ms);
        points
    }

    /// 某 profile 自 `since_ms` 起的用量，按**小时 × 模型**聚合（时间升序）。
    pub fn usage_slices(&self, name: &str, since_ms: i64) -> Vec<UsageSlice> {
        const HOUR_MS: i64 = 60 * 60 * 1000;
        let mut buckets: BTreeMap<(i64, String), UsageSlice> = BTreeMap::new();
        let Ok(file) = File::open(&self.usage_path) else {
            return Vec::new();
        };
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<UsageRecord>(&line) {
                Ok(rec) if rec.name == name && rec.ts_ms >= since_ms => {
                    let bucket = rec.ts_ms.div_euclid(HOUR_MS) * HOUR_MS;
                    let entry = buckets
                        .entry((bucket, rec.model.clone()))
                        .or_insert_with(|| UsageSlice {
                            ts_ms: bucket,
                            model: rec.model.clone(),
                            prompt_tokens: 0,
                            completion_tokens: 0,
                            calls: 0,
                        });
                    entry.prompt_tokens += rec.prompt_tokens;
                    entry.completion_tokens += rec.completion_tokens;
                    entry.calls += 1;
                }
                _ => {}
            }
        }
        buckets.into_values().collect()
    }
}

/// 选择模型价格：大小写不敏感的**最长前缀**匹配（更具体的条目优先）。
pub fn price_for<'a>(pricing: &'a [ModelPrice], model: &str) -> Option<&'a ModelPrice> {
    let model_lc = model.to_ascii_lowercase();
    pricing
        .iter()
        .filter(|price| {
            let prefix = price.model.trim();
            !prefix.is_empty() && model_lc.starts_with(&prefix.to_ascii_lowercase())
        })
        .max_by_key(|price| price.model.trim().len())
}

/// 是否 DeepSeek **高峰时段**：北京时间周一至周五 9:00–12:00、14:00–18:00
/// （法定节假日的空闲计价未纳入估算，属近似）。
pub fn is_peak_time(ts_ms: i64) -> bool {
    use chrono::{Datelike, TimeZone, Timelike, Weekday};
    let offset = match chrono::FixedOffset::east_opt(8 * 3600) {
        Some(offset) => offset,
        None => return false,
    };
    let Some(dt) = offset.timestamp_millis_opt(ts_ms).single() else {
        return false;
    };
    if matches!(dt.weekday(), Weekday::Sat | Weekday::Sun) {
        return false;
    }
    matches!(dt.hour(), 9..=11 | 14..=17)
}

/// 费用估算：返回（费用, 币种)。无匹配价格 = None。
///
/// 输入价按"缓存未命中"口径（框架不区分缓存命中，估值是上界）；按调用
/// 时刻的高峰/空闲档取价。
pub fn estimate_cost(
    pricing: &[ModelPrice],
    model: &str,
    ts_ms: i64,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> Option<(f64, String)> {
    let price = price_for(pricing, model)?;
    let peak = is_peak_time(ts_ms);
    let input = if peak {
        price.input_per_million
    } else {
        price
            .offpeak_input_per_million
            .unwrap_or(price.input_per_million)
    };
    let output = if peak {
        price.output_per_million
    } else {
        price
            .offpeak_output_per_million
            .unwrap_or(price.output_per_million)
    };
    let cost = prompt_tokens as f64 / 1_000_000.0 * input
        + completion_tokens as f64 / 1_000_000.0 * output;
    Some((cost, price.currency.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_pricing;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "echo-metrics-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        dir
    }

    #[test]
    fn balance_roundtrip_and_filter() {
        let dir = temp_dir("balance");
        let store = MetricsStore::new(&dir);
        store.record_balance("deepseek", "CNY", 1287.91, 0.0, 1287.91);
        store.record_balance("deepseek", "CNY", 1280.0, 0.0, 1280.0);
        store.record_balance("other", "USD", 5.0, 1.0, 4.0);

        let points = store.balance_points("deepseek", 0);
        assert_eq!(points.len(), 2);
        assert!((points[0].total - 1287.91).abs() < f64::EPSILON);
        assert_eq!(points[1].total, 1280.0);
        assert_eq!(points[0].currency, "CNY");
        assert_eq!(store.balance_points("missing", 0).len(), 0);
        let _ = fs::remove_dir_all(&dir);

        // since 过滤：手工写入带确定时间戳的记录（record_balance 的 ts 是
        // 同毫秒粒度，无法在测试里制造可区分的两段窗口）。
        let dir2 = temp_dir("balance-filter");
        fs::create_dir_all(&dir2).unwrap();
        fs::write(
            dir2.join("echo-balances.jsonl"),
            "{\"ts_ms\":1000,\"name\":\"a\",\"currency\":\"CNY\",\"total\":1.0,\"granted\":0.0,\"topped_up\":1.0}\n\
             {\"ts_ms\":5000,\"name\":\"a\",\"currency\":\"CNY\",\"total\":2.0,\"granted\":0.0,\"topped_up\":2.0}\n",
        )
        .unwrap();
        let store2 = MetricsStore::new(&dir2);
        let filtered = store2.balance_points("a", 2000);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].total, 2.0);
        let _ = fs::remove_dir_all(&dir2);
    }

    #[test]
    fn usage_aggregates_by_hour_and_model() {
        let dir = temp_dir("usage");
        let store = MetricsStore::new(&dir);
        store.record_usage("deepseek", "deepseek-flash", 1000, 100);
        store.record_usage("deepseek", "deepseek-flash", 500, 50);
        store.record_usage("deepseek", "deepseek-v4-pro", 700, 70);
        store.record_usage("kimi", "k3", 999, 99);

        let slices = store.usage_slices("deepseek", 0);
        // 同一小时：flash 合并为 1 条、pro 1 条
        assert_eq!(slices.len(), 2);
        let flash = slices
            .iter()
            .find(|s| s.model == "deepseek-flash")
            .expect("flash slice");
        assert_eq!(flash.prompt_tokens, 1500);
        assert_eq!(flash.completion_tokens, 150);
        assert_eq!(flash.calls, 2);
        let pro = slices
            .iter()
            .find(|s| s.model == "deepseek-v4-pro")
            .expect("pro slice");
        assert_eq!(pro.calls, 1);

        assert_eq!(store.usage_slices("kimi", 0).len(), 1);
        // 未来窗口过滤为空
        assert_eq!(
            store.usage_slices("deepseek", now_ms() + 1_000_000).len(),
            0
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_drops_expired_rows() {
        let dir = temp_dir("prune");
        let path = dir.join("echo-balances.jsonl");
        fs::create_dir_all(&dir).unwrap();
        let old = now_ms() - BALANCE_RETENTION_MS - 1000;
        let fresh = now_ms();
        fs::write(
            &path,
            format!(
                "{{\"ts_ms\":{old},\"name\":\"a\",\"currency\":\"CNY\",\"total\":1.0,\"granted\":0.0,\"topped_up\":1.0}}\n\
                 {{\"ts_ms\":{fresh},\"name\":\"a\",\"currency\":\"CNY\",\"total\":2.0,\"granted\":0.0,\"topped_up\":2.0}}\n"
            ),
        )
        .unwrap();
        let store = MetricsStore::new(&dir);
        // 加载不受剪枝影响：两条都在（旧行仍可读，仅保留期外的会在剪枝时删）
        assert_eq!(store.balance_points("a", 0).len(), 2);
        // 直接手动触发一次剪枝
        store.balances.lock().unwrap().prune();
        let points = store.balance_points("a", 0);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].total, 2.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn price_matching_longest_prefix_and_case() {
        let pricing = default_pricing();
        // 精确名
        assert_eq!(
            price_for(&pricing, "deepseek-flash")
                .unwrap()
                .input_per_million,
            2.0
        );
        // 旧名按 Flash 价（前缀条目）
        assert_eq!(
            price_for(&pricing, "deepseek-v4-flash")
                .unwrap()
                .output_per_million,
            8.0
        );
        // vision 变体走更长前缀（deepseek-v4-flash）而非 deepseek-flash
        assert!(price_for(&pricing, "deepseek-v4-flash-vision-exp").is_some());
        // 大小写不敏感
        assert!(price_for(&pricing, "DeepSeek-V4-Pro-0813").is_some());
        assert_eq!(
            price_for(&pricing, "DeepSeek-V4-Pro-0813")
                .unwrap()
                .input_per_million,
            9.0
        );
        // 未知模型
        assert!(price_for(&pricing, "gpt-6-astra").is_none());
    }

    #[test]
    fn peak_window_uses_beijing_time() {
        use chrono::TimeZone;
        let off = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let at = |y, m, d, h| {
            off.with_ymd_and_hms(y, m, d, h, 0, 0)
                .single()
                .unwrap()
                .timestamp_millis()
        };
        // 2026-10-05 是周一：9-12、14-18 为高峰
        assert!(is_peak_time(at(2026, 10, 5, 10)));
        assert!(!is_peak_time(at(2026, 10, 5, 12)));
        assert!(is_peak_time(at(2026, 10, 5, 15)));
        assert!(!is_peak_time(at(2026, 10, 5, 18)));
        assert!(!is_peak_time(at(2026, 10, 5, 8)));
        // 2026-10-03 是周六：全天空闲
        assert!(!is_peak_time(at(2026, 10, 3, 10)));
    }

    #[test]
    fn cost_estimation_peak_and_offpeak() {
        let pricing = default_pricing();
        use chrono::TimeZone;
        let off = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let peak_ms = off
            .with_ymd_and_hms(2026, 10, 5, 10, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let offpeak_ms = off
            .with_ymd_and_hms(2026, 10, 5, 22, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();

        // deepseek-flash 高峰：1M in + 1M out = 2 + 8 = 10 元
        let (cost, currency) =
            estimate_cost(&pricing, "deepseek-v4-flash", peak_ms, 1_000_000, 1_000_000).unwrap();
        assert!((cost - 10.0).abs() < 1e-9, "peak cost = {cost}");
        assert_eq!(currency, "CNY");
        // 空闲半价：1 + 4 = 5 元
        let (cost, _) = estimate_cost(
            &pricing,
            "deepseek-v4-flash",
            offpeak_ms,
            1_000_000,
            1_000_000,
        )
        .unwrap();
        assert!((cost - 5.0).abs() < 1e-9, "offpeak cost = {cost}");
        // 无匹配价格
        assert!(estimate_cost(&pricing, "gpt-6-astra", peak_ms, 1, 1).is_none());
    }
}
