//! 余额 / token 用量 / 成本估算的**兼容路径**（实现在
//! [`echo_plugin_provider_llm::metrics`]）。
//!
//! 2026-11 全插件化 S3：716 行搬进 provider 插件。计量是 **provider 侧的事**
//! ——同一个 crate 里已经有计量装饰器（`metering.rs`：包在 provider 外面记录
//! 每次调用的 token 用量），而它的数据落点（`MetricsStore`）却住在宿主。
//! 现在两侧同处一地。
//!
//! 搬家的先决条件是 `ModelPrice` 下沉到 `echo_defs::pricing`（跨边界配置
//! 词汇：写在 core.toml、被计量用来估算成本、被 `QueryApiMetrics` 回执）。
//!
//! 本模块保留 `crate::metrics::…` 这条路径（`llm.rs` 的 `UsageRecorder`
//! 实现、`agent/builder.rs` 的注入点、面板指标查询都在用）；内容全部是
//! re-export。

pub use echo_plugin_provider_llm::metrics::{
    estimate_cost, is_peak_time, now_ms, price_for, BalancePoint, MetricsStore, UsageSlice,
    METRICS_WINDOW_MS,
};
