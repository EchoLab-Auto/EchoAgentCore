//! plugins.toml 装载 + 子进程插件启动（解耦计划 P3/P4）。
//!
//! 配置文件缺省放在 core.toml 同目录的 `plugins.toml`；不存在 = 该特性
//! 休眠（零行为变化）。
//!
//! 语义：
//! - [`start_if_configured`]：`config_path.with_file_name("plugins.toml")`
//!   不存在 → `None`（debug 日志）。存在 → [`echo_plugin_loader::load`]
//!   合成 / 校验插件树；失败 → warn + `None`。之后 best-effort 启动全部
//!   enabled 的 stdio 插件（单个失败逐条 warn 但继续）。成功 →
//!   `tracing::info!(count, skipped)` 并返回 [`PluginRuntime`]。
//! - [`PluginRuntime::tools`]：全部已启动插件的远程工具适配器
//!   （`Arc<RemoteTool>`——`RemoteTool` 已实现 `echo_agent::tool::Tool`，
//!   注册时可直接向上转型为 `Arc<dyn Tool>`；同时保留 `package()` 供注册
//!   方打「按包分组」标签）。
//! - [`PluginRuntime::shutdown`]：5s 期限排空全部句柄（best-effort）。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use echo_plugin_host::bootstrap::StartedPlugins;
use echo_plugin_host::{PluginSupervisor, RemoteTool};

/// 已装载并启动的子进程插件运行时。
pub struct PluginRuntime {
    /// 启动结果（句柄 + 单点失败记录）。
    pub started: StartedPlugins,
    /// 未启动条目的人可读原因（disabled / 非 stdio 运输层等）。
    pub skipped: Vec<String>,
}

/// 若 `<config_path 同目录>/plugins.toml` 存在则装载并启动；否则休眠。
pub async fn start_if_configured(config_path: &Path) -> Option<PluginRuntime> {
    let path = config_path.with_file_name("plugins.toml");
    if !path.exists() {
        tracing::debug!(
            path = %path.display(),
            "plugins.toml not found; subprocess plugins dormant"
        );
        return None;
    }
    let tree = match echo_plugin_loader::load(&path) {
        Ok(tree) => tree,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "plugins.toml load failed; subprocess plugins dormant"
            );
            return None;
        }
    };
    let (specs, skipped) = echo_plugin_host::bootstrap::launch_specs(&tree);
    let supervisor = PluginSupervisor::new();
    let started = echo_plugin_host::bootstrap::start_plugins(&supervisor, specs).await;
    for (plugin_id, error) in &started.errors {
        tracing::warn!(plugin = %plugin_id, %error, "subprocess plugin start failed");
    }
    let runtime = PluginRuntime { started, skipped };
    for reason in &runtime.skipped {
        tracing::debug!(reason = %reason, "plugin entry skipped");
    }
    tracing::info!(
        count = runtime.started.handles.len(),
        skipped = runtime.skipped.len(),
        "subprocess plugins started from plugins.toml"
    );
    Some(runtime)
}

impl PluginRuntime {
    /// 全部已启动插件的远程工具适配器（未注册工具贡献的插件贡献空表）。
    pub fn tools(&self) -> Vec<Arc<RemoteTool>> {
        self.started.tools()
    }

    /// 优雅关停全部插件（5s 期限；单点错误由 bootstrap 内部记 debug 日志）。
    pub async fn shutdown(&self) {
        self.started.shutdown(Duration::from_secs(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 唯一临时目录（测试尾部清理；不引入 tempfile 依赖）。
    fn unique_dir(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "echo-core-plugins-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ))
    }

    /// plugins.toml 不存在 → None（特性休眠，零行为变化）。
    #[tokio::test]
    async fn missing_plugins_toml_is_dormant() {
        let dir = unique_dir("missing");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("echo-agent-core.toml");
        assert!(start_if_configured(&config_path).await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// plugins.toml 存在但内容非法 → None（warn，但不 panic）。
    #[tokio::test]
    async fn invalid_plugins_toml_returns_none_without_panic() {
        let dir = unique_dir("invalid");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("plugins.toml"), "not = valid = toml [[[").expect("write toml");
        let config_path = dir.join("echo-agent-core.toml");
        assert!(start_if_configured(&config_path).await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
