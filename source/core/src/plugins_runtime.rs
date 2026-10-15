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
use echo_plugin_host::{HostServiceRegistry, PluginSupervisor, RemoteTool};

/// 已装载并启动的子进程插件运行时。
pub struct PluginRuntime {
    /// 启动结果（句柄 + 单点失败记录）。
    pub started: StartedPlugins,
    /// 未启动条目的人可读原因（disabled / 非 stdio 运输层等）。
    pub skipped: Vec<String>,
    /// 适配器宿主服务（`adapter_host`；适配器插件的入站 / 状态回呼入口；
    /// P1，2026-10 适配器插件化）。无适配器贡献时为空注册表（无副作用）。
    pub adapter_host: Arc<echo_plugin_host::AdapterHostService>,
}

/// 若 `<config_path 同目录>/plugins.toml` 存在则装载并启动；否则休眠。
///
/// `register_host_services`：插件启动**前**的宿主服务注册钩子（P2）——
/// 组合根在此把可回呼服务（如 `"sanitizer"`）注册进
/// [`HostServiceRegistry`]；插件随后经 `HostCall` 调用。无
/// plugins.toml（特性休眠）时钩子不执行（注册表无消费者，无副作用）。
pub async fn start_if_configured(
    config_path: &Path,
    register_host_services: impl FnOnce(&HostServiceRegistry),
) -> Option<PluginRuntime> {
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
    register_host_services(&supervisor.host_services());
    // P1：适配器宿主服务先行注册——插件就绪后其 `deliver_message` / `report_*`
    // 回呼即有接收面（否则适配器插件启动即报 adapter_not_found）。
    let adapter_host = Arc::new(echo_plugin_host::AdapterHostService::new());
    supervisor.host_services().register("adapter_host", adapter_host.clone());
    let started = echo_plugin_host::bootstrap::start_plugins(&supervisor, specs).await;
    for (plugin_id, error) in &started.errors {
        tracing::warn!(plugin = %plugin_id, %error, "subprocess plugin start failed");
    }
    let runtime = PluginRuntime { started, skipped, adapter_host };
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

/// 一个已注册的远程适配器 + 其入站队列 receiver（组合根接到送达入口；
/// P1，2026-10 适配器插件化）。
pub struct RemoteAdapterHandle {
    /// 远程适配器（注册进 `AdapterRegistry`）。
    pub adapter: Arc<echo_plugin_host::RemoteAdapter>,
    /// 入站消息队列 receiver（插件 `deliver_message` 的出站端；
    /// 每实例串行 worker 消费）。
    pub inbound_rx: tokio::sync::mpsc::Receiver<echo_plugin_host::api::AdapterMessage>,
}

impl PluginRuntime {
    /// 全部已启动插件的远程工具适配器（未注册工具贡献的插件贡献空表）。
    pub fn tools(&self) -> Vec<Arc<RemoteTool>> {
        self.started.tools()
    }

    /// 全部已启动插件的适配器贡献的远程适配器（P1，2026-10 适配器插件化）。
    ///
    /// 每个 `(AdapterContribution, PluginHandle)` 构造一个 `RemoteAdapter`
    /// 并注册进 `adapter_host`（入站队列 / 状态回执入口就位，队列 receiver
    /// 随元组带出）；组合根随后把适配器注册进 `AdapterRegistry`，并把
    /// receiver 接到消息送达入口（见 `main.rs` 装配）。
    pub fn adapters(&self) -> Vec<RemoteAdapterHandle> {
        self.started
            .adapters()
            .into_iter()
            .map(|(contribution, handle)| {
                let adapter = echo_plugin_host::RemoteAdapter::new(contribution, handle);
                // 注册进 adapter_host（入站队列就绪；receiver 交组合根）。
                let inbound_rx = self.adapter_host.register(adapter.clone());
                RemoteAdapterHandle {
                    adapter,
                    inbound_rx,
                }
            })
            .collect()
    }

    /// 适配器宿主服务（`adapter_host`；插件回呼的接收面）。
    pub fn adapter_host(&self) -> Arc<echo_plugin_host::AdapterHostService> {
        self.adapter_host.clone()
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
        assert!(start_if_configured(&config_path, |_| {}).await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// plugins.toml 存在但内容非法 → None（warn，但不 panic）。
    #[tokio::test]
    async fn invalid_plugins_toml_returns_none_without_panic() {
        let dir = unique_dir("invalid");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("plugins.toml"), "not = valid = toml [[[").expect("write toml");
        let config_path = dir.join("echo-agent-core.toml");
        assert!(start_if_configured(&config_path, |_| {}).await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
