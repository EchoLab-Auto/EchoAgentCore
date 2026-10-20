//! 全局供应商池的**兼容路径**（实现在 [`echo_plugin_provider_llm::api_pool`]）。
//!
//! 2026-11 全插件化 S3：132 行读取助手 + 4 项测试搬进 provider 插件。搬家的
//! 先决条件是 `ApiProfile` 下沉到定义层（`echo_defs::api_profile`）——它此前
//! 是 `echo-agent::config` 里的类型，插件读不到它。
//!
//! 保留本模块是为了 `crate::api_pool::…` 这条路径不变（`agent/api_admin.rs`
//! 与 `tools_builtin/api_profiles.rs` 在用）；内容全部 re-export。

pub use echo_plugin_provider_llm::api_pool::{read_snapshot, PoolSnapshot};
