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
//! - [`PluginRuntime::tools`]：全部已启动插件的远程工具插件实例
//!   （`Arc<RemoteTool>`——`RemoteTool` 已实现 `echo_agent::tool::Tool`，
//!   注册时可直接向上转型为 `Arc<dyn Tool>`；同时保留 `package()` 供注册
//!   方打「按包分组」标签）。
//! - [`PluginRuntime::shutdown`]：5s 期限排空全部句柄（best-effort）。
//! - [`dump_config`]：`dump-config` 子命令的实现——只合成不启动，把分层
//!   合成树渲染回 TOML（每行带来源注释）。
//! - [`builtin_mount_plan`]：`plugins.toml` 里 `kind = "builtin"` 的行 →
//!   内置插件的挂载计划（配置化组合 P4：集合 / 顺序 / 启用由配置说了算）。
//! - [`config_origins`]：每个插件条目由**哪一层**配置声明（面板展示用）。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use echo_plugin_adapter::{CoreApiService, RemoteInstance, RemoteTool};
use echo_plugin_host::bootstrap::StartedPlugins;
use echo_plugin_host::{HostServiceRegistry, PluginSupervisor};

/// 已装载并启动的子进程插件运行时。
pub struct PluginRuntime {
    /// 启动结果（句柄 + 单点失败记录）。
    pub started: StartedPlugins,
    /// 未启动条目的人可读原因（disabled / 非 stdio 运输层等）。
    pub skipped: Vec<String>,
    /// 插件实例宿主服务（`core_api`；实例插件的入站 / 状态回呼入口；
    /// P1，2026-10 实例插件化）。无插件实例贡献时为空注册表（无副作用）。
    pub core_api: Arc<CoreApiService>,
    /// `plugins.toml` **静态条目**的元数据（id / name / kind / 是否启用 /
    /// 启动失败原因）。
    ///
    /// 只含静态条目：动态 `LaunchSpec`（QQ 实例）另有自己的门控壳，不能
    /// 重复注册。见 [`PluginRuntime::gate_shells`]。
    pub entries: Vec<StaticPluginEntry>,
}

/// `plugins.toml` 静态条目（用于在插件清单里显示**外部插件**）。
#[derive(Debug, Clone)]
pub struct StaticPluginEntry {
    /// 配置里的条目 id。
    pub id: String,
    /// 人读名（配置 `name`；空则回退 id）。
    pub name: String,
    /// 配置的 kind 字面量（`stdio` / `builtin` …）。
    pub kind: String,
    /// 配置里 `enabled = false`（未启动是配置选择，不是故障）。
    pub enabled: bool,
    /// 启动失败原因（握手失败 / 命令不存在）；`None` = 已就绪或未尝试。
    pub error: Option<String>,
    /// 启动命令（`plugins.toml` 的 `command`；面板据此排查「为什么不起来」）。
    pub command: String,
    /// 启动参数。
    pub args: Vec<String>,
    /// 环境变量**键名**（值不下发：常含 token / 密钥）。
    pub env_keys: Vec<String>,
}

/// 若 `<config_path 同目录>/plugins.toml` 存在则装载并启动；否则休眠。
///
/// `register_host_services`：插件启动**前**的宿主服务注册钩子（P2）——
/// 组合根在此把可回呼服务（如 `"sanitizer"`）注册进
/// [`HostServiceRegistry`]；插件随后经 `HostCall` 调用。无
/// plugins.toml（特性休眠）时钩子不执行（注册表无消费者，无副作用）。
#[allow(dead_code)] // 保留 API：测试与"仅静态 plugins.toml"调用形态。
pub async fn start_if_configured(
    config_path: &Path,
    register_host_services: impl FnOnce(&HostServiceRegistry),
) -> Option<PluginRuntime> {
    start_with_specs(config_path, register_host_services, Vec::new()).await
}

/// [`start_if_configured`] 的动态实例版（P1.8，2026-10 实例插件化）：
/// 在 `plugins.toml` 静态条目之外追加组合根即时组装的 [`LaunchSpec`]——
/// 实例插件按**实例**拉起（每实例独立进程 / Hello.config），而
/// `plugins.toml` 只加载一次，无法承载运行期分配（端口 / persona / state 文件）。
///
/// 语义：
/// - `plugins.toml` 存在 → 静态条目 + `extra_specs` 合并启动；
/// - 不存在但 `extra_specs` 非空 → 仅启动动态实例；
/// - 两者皆空 → `None`（休眠，零行为变化）。
///
/// [`LaunchSpec`]: echo_plugin_host::bootstrap::LaunchSpec
pub async fn start_with_specs(
    config_path: &Path,
    register_host_services: impl FnOnce(&HostServiceRegistry),
    extra_specs: Vec<echo_plugin_host::bootstrap::LaunchSpec>,
) -> Option<PluginRuntime> {
    let path = config_path.with_file_name("plugins.toml");
    let mut load_failed = false;
    let mut entries: Vec<StaticPluginEntry> = Vec::new();
    let (mut specs, skipped) = if path.exists() {
        match echo_plugin_loader::load(&path) {
            Ok(tree) => {
                // 记下静态条目（进插件清单）+ 逐条诊断（同层重复 id 等）。
                entries = static_entries(&tree);
                echo_plugin_host::bootstrap::launch_specs(&tree)
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "plugins.toml load failed; continuing with dynamic specs only"
                );
                load_failed = true;
                (Vec::new(), Vec::new())
            }
        }
    } else {
        tracing::debug!(
            path = %path.display(),
            "plugins.toml not found; static subprocess plugins dormant"
        );
        (Vec::new(), Vec::new())
    };
    if !extra_specs.is_empty() {
        tracing::info!(
            count = extra_specs.len(),
            "dynamic plugin specs merged (adapter instances)"
        );
        specs.extend(extra_specs);
    }
    // 无静态条目且无动态实例：休眠（非法 plugins.toml 也按原语义休眠，
    // 不改变「错误配置 = 特性不启用」的行为）。
    if specs.is_empty() && (!path.exists() || load_failed) {
        return None;
    }
    let supervisor = PluginSupervisor::new();
    register_host_services(&supervisor.host_services());
    // P1：插件实例宿主服务先行注册——插件就绪后其 `submit_input` / `report_*`
    // 回呼即有接收面（否则实例插件启动即报 instance_not_found）。
    let core_api = Arc::new(CoreApiService::new());
    supervisor
        .host_services()
        .register("core_api", core_api.clone());
    // 旧插件按名 `adapter_host` 查找宿主服务（双注册过渡，2026-10 去适配器化）。
    supervisor
        .host_services()
        .register("adapter_host", core_api.clone());
    let started = echo_plugin_host::bootstrap::start_plugins(&supervisor, specs).await;
    for (plugin_id, error) in &started.errors {
        tracing::warn!(plugin = %plugin_id, %error, "subprocess plugin start failed");
    }
    // 启动失败原因回填到静态条目：插件清单里能直接看到「为什么没起来」。
    // 只认静态条目 id——动态实例（QQ）的失败另有实例状态通道上报。
    for entry in &mut entries {
        if let Some((_, error)) = started.errors.iter().find(|(id, _)| id == &entry.id) {
            entry.error = Some(error.clone());
        }
    }
    let runtime = PluginRuntime {
        started,
        skipped,
        core_api,
        entries,
    };
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

/// 已启动子进程插件的生命周期控制面（组合根注入进 [`PluginHost`]）。
///
/// 持有每个静态条目的 [`PluginHandle`]：面板的启停开关据此**真正停掉子进程**
/// （而不是只改注册表状态），重启按钮据此 stop + start。
/// 动态条目（QQ 实例）不在表里——它们的生命周期归实例插件。
pub struct SubprocessControl {
    handles: std::collections::HashMap<String, echo_plugin_host::supervisor::PluginHandle>,
    /// 配置里 `enabled = false` 的条目（没有 handle，但要显示为**已停止**，
    /// 否则面板对这类条目连运行态都不显示）。
    disabled: std::collections::HashSet<String>,
    /// 启动就没起来的条目（握手失败 / 命令不存在）：没有 handle，
    /// 但状态应为 **failed** + 原因（面板的「失败」徽标与详情块读它）。
    failed: std::collections::HashMap<String, String>,
}

impl SubprocessControl {
    /// 从启动结果建表（只看静态条目：动态 `LaunchSpec` 的 id 不在 entries 里）。
    pub fn new(runtime: &PluginRuntime) -> Self {
        let known: std::collections::HashSet<&str> = runtime
            .entries
            .iter()
            .filter(|entry| entry.kind != "builtin")
            .map(|entry| entry.id.as_str())
            .collect();
        let handles = runtime
            .started
            .handles
            .iter()
            .filter(|handle| known.contains(handle.plugin_id()))
            .map(|handle| (handle.plugin_id().to_string(), handle.clone()))
            .collect();
        let disabled = runtime
            .entries
            .iter()
            .filter(|entry| entry.kind != "builtin" && !entry.enabled)
            .map(|entry| entry.id.clone())
            .collect();
        let failed = runtime
            .entries
            .iter()
            .filter_map(|entry| {
                let error = entry.error.as_ref()?;
                (entry.kind != "builtin").then(|| (entry.id.clone(), error.clone()))
            })
            .collect();
        Self {
            handles,
            disabled,
            failed,
        }
    }
}

impl echo_agent::plugins::SubprocessPluginControl for SubprocessControl {
    fn stop<'a>(
        &'a self,
        id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let handle = self
                .handles
                .get(id)
                .ok_or_else(|| format!("{id}: 不是已启动的外部插件"))?;
            handle
                .stop(std::time::Duration::from_secs(5))
                .await
                .map_err(|error| format!("{id} stop failed: {error}"))
        })
    }

    fn start<'a>(
        &'a self,
        id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let handle = self
                .handles
                .get(id)
                .ok_or_else(|| format!("{id}: 不是已启动的外部插件"))?;
            handle
                .start()
                .await
                .map_err(|error| format!("{id} start failed: {error}"))
        })
    }

    fn status(&self, id: &str) -> Option<echo_agent::plugins::SubprocessStatus> {
        // 没有 handle 的两种已知情形（都要给出明确状态，不能"查不到"）：
        // ① 启动就没起来 → failed + 原因；② 配置关掉从未启动 → stopped。
        let Some(handle) = self.handles.get(id) else {
            if let Some(error) = self.failed.get(id) {
                return Some(echo_agent::plugins::SubprocessStatus {
                    state: "failed".to_string(),
                    restarts: 0,
                    generation: 0,
                    error: Some(error.clone()),
                });
            }
            return self
                .disabled
                .contains(id)
                .then(|| echo_agent::plugins::SubprocessStatus {
                    state: "stopped".to_string(),
                    restarts: 0,
                    generation: 0,
                    error: None,
                });
        };
        let state = handle.state();
        Some(echo_agent::plugins::SubprocessStatus {
            state: state.as_str().to_string(),
            restarts: handle.restarts(),
            generation: handle.generation(),
            error: state.error().map(str::to_string),
        })
    }
}

/// 把合成树里的**静态**条目转成清单元数据（不含动态 `LaunchSpec`）。
fn static_entries(tree: &echo_plugin_loader::LoadedTree) -> Vec<StaticPluginEntry> {
    tree.entries
        .iter()
        .map(|entry| StaticPluginEntry {
            id: entry.id.clone(),
            name: if entry.name.trim().is_empty() {
                entry.id.clone()
            } else {
                entry.name.clone()
            },
            kind: entry.kind.as_str().to_string(),
            enabled: entry.enabled,
            error: None,
            command: entry.command.clone().unwrap_or_default(),
            args: entry.args.clone(),
            env_keys: entry.env.keys().cloned().collect(),
        })
        .collect()
}

/// 一个已注册的远程插件实例 + 其入站队列 receiver（组合根接到送达入口；
/// P1，2026-10 实例插件化）。
pub struct RemoteInstanceHandle {
    /// 远程插件实例（注册进 `InstanceRegistry`）。
    pub instance: Arc<RemoteInstance>,
    /// 入站消息队列 receiver（插件 `submit_input` 的出站端；
    /// 每实例串行 worker 消费）。
    pub inbound_rx: tokio::sync::mpsc::Receiver<echo_plugin_host::api::IncomingMessage>,
}

impl PluginRuntime {
    /// 全部已启动插件的远程工具插件实例（未注册工具贡献的插件贡献空表）。
    pub fn tools(&self) -> Vec<Arc<RemoteTool>> {
        echo_plugin_adapter::tools_of(&self.started)
    }

    /// 全部已启动插件的插件实例贡献的远程插件实例（P1，2026-10 实例插件化）。
    ///
    /// 每个 `(InstanceContribution, PluginHandle)` 构造一个 `RemoteInstance`
    /// 并注册进 `core_api`（入站队列 / 状态回执入口就位，队列 receiver
    /// 随元组带出）；组合根随后把插件实例注册进 `InstanceRegistry`，并把
    /// receiver 接到消息送达入口（见 `main.rs` 装配）。
    pub fn instances(&self) -> Vec<RemoteInstanceHandle> {
        echo_plugin_adapter::instance_contributions(&self.started)
            .into_iter()
            .map(|(contribution, handle)| {
                let instance = RemoteInstance::new(contribution, handle);
                // 注册进 core_api（入站队列就绪；receiver 交组合根）。
                let inbound_rx = self.core_api.register(instance.clone());
                RemoteInstanceHandle {
                    instance,
                    inbound_rx,
                }
            })
            .collect()
    }

    /// 插件实例宿主服务（`core_api`；插件回呼的接收面）。
    pub fn core_api(&self) -> Arc<CoreApiService> {
        self.core_api.clone()
    }

    /// 优雅关停全部插件（5s 期限；单点错误由 bootstrap 内部记 debug 日志）。
    pub async fn shutdown(&self) {
        self.started.shutdown(Duration::from_secs(5)).await;
    }
}

/// `dump-config` 子命令：把 `plugins.toml` 的**分层合成结果**渲染回 TOML。
///
/// 只**合成不启动**——不拉起任何子进程、不碰 kernel / 网络，纯函数式地
/// 走一遍 [`echo_plugin_loader::load`]（profile → bundles → patches →
/// 行内条目 的 dsh 式"按 id 定位、整行替换"），再把结果交给
/// [`echo_plugin_loader::dump`] 渲染成带来源注释的 TOML：
///
/// ```text
/// # --- example (layer 1: bundle-extra.toml) ---
/// id = "example"
/// kind = "stdio"
/// ...
/// ```
///
/// 用途：回答"这行配置**到底**从哪一层来的、最终长什么样"——patch 覆盖
/// bundle、bundle 覆盖前一个 bundle 的结果全都在注释里，不必手工推演。
///
/// 语义与启动路径严格一致：**用同一个 `load()`**，所以这里能合成出来的
/// 树就是运行期会装载的树；校验失败 / 环 / 未知 `requires` 同样在这里
/// 报同一个错（错误文本原样回给用户，前后缀由调用方加）。
///
/// 返回 `Ok(None)` = `plugins.toml` 不存在（外部插件特性休眠，与启动
/// 路径的"零行为变化"一致——不是错误）。
pub fn dump_config(config_path: &Path) -> Result<Option<String>, echo_plugin_loader::LoadError> {
    let path = config_path.with_file_name("plugins.toml");
    if !path.exists() {
        return Ok(None);
    }
    let tree = echo_plugin_loader::load(&path)?;
    let mut text = echo_plugin_loader::dump(&tree);
    // 合成期诊断（如同一层内重复 id）不进 dump 的正文——它们是告警而非
    // 配置内容，追加为文件末尾注释，既不污染可回灌的 TOML 语义，又不丢信息。
    if !tree.warnings.is_empty() {
        text.push_str("\n# ── 合成期告警 ──\n");
        for warning in &tree.warnings {
            text.push_str(&format!("# {warning}\n"));
        }
    }
    Ok(Some(text))
}

/// `plugins.toml` 里 `kind = "builtin"` 的行 → 内置插件的**挂载计划**。
///
/// 只挑 `kind = "builtin"`：`stdio` 行走子进程启动路径（[`start_with_specs`]），
/// 两类条目在同一次合成里共存、各管各的。
///
/// 语义（与启动路径共用同一个 `load()`，所以不存在"dump 看到的树"与
/// "装载的树"不一致）：
/// - 文件不存在 / 解析失败 → **空计划**（外部插件特性休眠，或是坏文件——
///   后者启动路径会 warn 并继续，这里同样不阻断启动：内置插件的缺省行为
///   是"全挂载"，空计划正好等价于改造前的行为）；
/// - 条目顺序即挂载顺序（`requires` 已在合成期拓扑排序过）；
/// - `enabled = false` 的内置行 → 计划里保留但标记禁用：注册表仍会注册它
///   （Panel 清单可见、可就地启用），只是启动时不挂载。
///
/// 未在计划里出现的内置插件仍按目录顺序挂载——见
/// [`echo_agent::plugins::resolve_mount_order`] 的说明。
pub fn builtin_mount_plan(config_path: &Path) -> Vec<echo_agent::plugins::MountStep> {
    use echo_plugin_loader::EntryKind;
    let path = config_path.with_file_name("plugins.toml");
    if !path.exists() {
        return Vec::new();
    }
    match echo_plugin_loader::load(&path) {
        Ok(tree) => tree
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Builtin)
            .map(|entry| echo_agent::plugins::MountStep::new(entry.id.clone(), entry.enabled))
            .collect(),
        Err(error) => {
            // 与启动路径同款：坏文件 warn 后按"无配置"继续（内置插件照旧
            // 全挂载），把可读错误留给用户从日志里看到具体哪一行坏了。
            tracing::warn!(path = %path.display(), %error, "plugins.toml load failed; builtin mount plan falls back to defaults");
            Vec::new()
        }
    }
}

/// `plugins.toml` 里每个条目由**哪一层**声明 → `(插件 id, 来源描述)`。
///
/// 分层合成（profile → bundles → patches → 行内）之后，每个条目都带着
/// `layer`（0 起的应用序号）与 `layer_source`（文件名）。面板的插件清单
/// 用它回答「这行为什么长这样 / 我要去哪一层改它」。
///
/// 描述形如 `plugins.toml`（本文件行内条目）或 `base.toml · layer 0`
/// （被 bundle/patch 覆盖过的行按**最后写入**的那一层显示——与合成语义
/// 一致：整行替换）。
///
/// 配置未提及的插件不会出现在结果里（调用方保持空串 = 编译内置的缺省）。
/// 文件不存在 / 解析失败 → 空表（与 [`builtin_mount_plan`] 同款降级：
/// 展示信息缺失不该阻断启动）。
pub fn config_origins(config_path: &Path) -> Vec<(String, String)> {
    let path = config_path.with_file_name("plugins.toml");
    if !path.exists() {
        return Vec::new();
    }
    match echo_plugin_loader::load(&path) {
        Ok(tree) => {
            let profile_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("plugins.toml")
                .to_string();
            tree.entries
                .iter()
                .map(|entry| {
                    let origin = if entry.layer_source == profile_name {
                        // 本文件行内条目：直接报文件名，不必说第几层。
                        entry.layer_source.clone()
                    } else {
                        format!("{} · layer {}", entry.layer_source, entry.layer)
                    };
                    (entry.id.clone(), origin)
                })
                .collect()
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "plugins.toml load failed; plugin config origins unavailable");
            Vec::new()
        }
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

    /// 动态 specs（插件实例）穿透 plugins.toml 休眠门控：无 plugins.toml
    /// 但有 extra specs → 仍启动（命令不存在 → 逐条失败记录，runtime 仍在）。
    #[tokio::test]
    async fn dynamic_specs_start_without_plugins_toml() {
        let dir = unique_dir("dynamic");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("echo-agent-core.toml");
        let spec = echo_plugin_host::bootstrap::LaunchSpec {
            plugin_id: "adapter-qq-test".to_string(),
            command: "/nonexistent/echo-adapter-qq-test".to_string(),
            args: vec![],
            env: Default::default(),
            config: serde_json::json!({"name": "qq-test"}),
        };
        let runtime = start_with_specs(&config_path, |_| {}, vec![spec])
            .await
            .expect("dynamic specs must wake the runtime");
        assert!(
            !runtime.started.errors.is_empty(),
            "missing command must be recorded as a start error"
        );
        assert!(
            runtime.started.handles.is_empty(),
            "no handle for a failed launch"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── dump-config（只合成不启动）──────────────────────────────────────

    /// 无 plugins.toml → `Ok(None)`：休眠不是错误（与启动路径同语义）。
    #[test]
    fn dump_config_reports_dormant_when_missing() {
        let dir = unique_dir("dump-missing");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("echo-agent-core.toml");
        assert!(
            dump_config(&config_path)
                .expect("missing file is not an error")
                .is_none(),
            "no plugins.toml must yield None, not an error"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 三层合成（bundle → patch → 行内）后渲染：每行带来源注释，
    /// 且 patch 的整行替换确实覆盖了 bundle 的值。
    #[test]
    fn dump_config_renders_composed_tree_with_provenance() {
        let dir = unique_dir("dump-layers");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("base.toml"),
            "[[plugin]]\nid = \"a\"\nkind = \"stdio\"\nname = \"a-base\"\ncommand = \"/bin/a\"\n",
        )
        .expect("write bundle");
        std::fs::write(
            dir.join("over.toml"),
            "[[plugin]]\nid = \"a\"\nkind = \"stdio\"\nname = \"a-patched\"\ncommand = \"/bin/a2\"\n",
        )
        .expect("write patch");
        std::fs::write(
            dir.join("plugins.toml"),
            "[profile]\nbundles = [\"base\"]\npatches = [\"over\"]\n\n             [[plugin]]\nid = \"b\"\nkind = \"stdio\"\nname = \"b\"\ncommand = \"/bin/b\"\n",
        )
        .expect("write profile");

        let text = dump_config(&dir.join("echo-agent-core.toml"))
            .expect("compose")
            .expect("plugins.toml exists");
        // 来源注释把「哪一层」写在每行上方：bundle=0、patch=1、行内=2。
        assert!(text.contains("# --- a (layer 1: over.toml) ---"), "{text}");
        assert!(
            text.contains("# --- b (layer 2: plugins.toml) ---"),
            "{text}"
        );
        // patch 整行替换（不是字段级合并）：值是 patch 的，bundle 的不留残余。
        assert!(text.contains("name = \"a-patched\""), "{text}");
        assert!(
            !text.contains("a-base"),
            "patch must replace the whole row: {text}"
        );
        // 渲染结果必须是可回灌的合法 TOML（dump 的契约）。
        let reparsed: toml::Value = toml::from_str(&text).expect("dump output must be valid TOML");
        assert!(reparsed["plugin"]
            .as_array()
            .is_some_and(|rows| rows.len() == 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 合成失败（未知 requires）→ 与启动路径**同一个**错误，不 panic。
    #[test]
    fn dump_config_surfaces_compose_errors() {
        let dir = unique_dir("dump-error");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("plugins.toml"),
            "[[plugin]]\nid = \"a\"\nkind = \"stdio\"\nname = \"a\"\ncommand = \"/bin/a\"\n             requires = [\"ghost\"]\n",
        )
        .expect("write profile");
        let error = dump_config(&dir.join("echo-agent-core.toml")).expect_err("must fail");
        assert!(error.to_string().contains("ghost"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 内置插件挂载计划（配置化组合 P4）────────────────────────────────

    /// 无 plugins.toml → 空计划（= 内置插件按目录顺序全挂载，行为不变）。
    #[test]
    fn builtin_plan_is_empty_without_plugins_toml() {
        let dir = unique_dir("plan-missing");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        assert!(builtin_mount_plan(&dir.join("echo-agent-core.toml")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只挑 builtin 行：stdio 行属于子进程启动路径，不能混进内置计划；
    /// 顺序即行序（合成期已拓扑排序）；`enabled = false` 原样带出。
    #[test]
    fn builtin_plan_picks_only_builtin_rows_in_order() {
        let dir = unique_dir("plan-mixed");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("plugins.toml"),
            "[[plugin]]\nid = \"external\"\nkind = \"stdio\"\nname = \"ext\"\ncommand = \"/bin/ext\"\n\n\
             [[plugin]]\nid = \"skills.dir\"\nkind = \"builtin\"\nname = \"技能目录\"\n\n\
             [[plugin]]\nid = \"tools.builtin\"\nkind = \"builtin\"\nname = \"内置工具集\"\nenabled = false\n",
        )
        .expect("write profile");

        let plan = builtin_mount_plan(&dir.join("echo-agent-core.toml"));
        let ids: Vec<&str> = plan.iter().map(|step| step.id.as_str()).collect();
        assert_eq!(ids, vec!["skills.dir", "tools.builtin"], "{plan:?}");
        assert!(plan[0].enabled, "缺省 enabled = true");
        assert!(!plan[1].enabled, "显式 enabled = false 必须带出");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 坏文件 → 空计划（不阻断启动）：内置插件回到缺省全挂载，
    /// 与子进程路径"warn 后继续"保持一致。
    #[test]
    fn builtin_plan_falls_back_to_defaults_on_broken_file() {
        let dir = unique_dir("plan-broken");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("plugins.toml"), "not = valid = toml [[[").expect("write toml");
        assert!(builtin_mount_plan(&dir.join("echo-agent-core.toml")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 配置来源层（面板展示用）──────────────────────────────────────

    /// 无 plugins.toml → 空表（面板显示「配置未声明」）。
    #[test]
    fn config_origins_empty_without_plugins_toml() {
        let dir = unique_dir("origins-missing");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        assert!(config_origins(&dir.join("echo-agent-core.toml")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 行内条目报文件名；被 bundle 覆盖的行报「哪一层」。
    #[test]
    fn config_origins_name_the_writing_layer() {
        let dir = unique_dir("origins-layers");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("base.toml"),
            "[[plugin]]\nid = \"a\"\nkind = \"stdio\"\nname = \"a\"\ncommand = \"/bin/a\"\n\n\
             [[plugin]]\nid = \"overridden\"\nkind = \"stdio\"\nname = \"old\"\ncommand = \"/bin/old\"\n",
        )
        .expect("write bundle");
        std::fs::write(
            dir.join("plugins.toml"),
            "[profile]\nbundles = [\"base\"]\n\n\
             [[plugin]]\nid = \"overridden\"\nkind = \"stdio\"\nname = \"new\"\ncommand = \"/bin/new\"\n",
        )
        .expect("write profile");

        let origins: std::collections::HashMap<String, String> =
            config_origins(&dir.join("echo-agent-core.toml"))
                .into_iter()
                .collect();
        // bundle 里的行：报 bundle 文件名 + 层号。
        assert_eq!(
            origins.get("a").map(String::as_str),
            Some("base.toml · layer 0")
        );
        // 被本文件覆盖的行：报本文件名（最后写入的那一层）。
        assert_eq!(
            origins.get("overridden").map(String::as_str),
            Some("plugins.toml")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 外部条目进插件清单（此前完全不可见）──────────────────────────

    /// 静态条目按合成树取元数据：id / name（空回退 id）/ kind / enabled。
    #[test]
    fn static_entries_mirror_the_composed_tree() {
        let dir = unique_dir("entries");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("plugins.toml"),
            "[[plugin]]\nid = \"with-name\"\nkind = \"stdio\"\nname = \"有名字\"\ncommand = \"/bin/x\"\n\n\
             [[plugin]]\nid = \"unnamed\"\nkind = \"stdio\"\nname = \"\"\ncommand = \"/bin/y\"\n\n\
             [[plugin]]\nid = \"off\"\nkind = \"stdio\"\nname = \"关掉的\"\ncommand = \"/bin/z\"\nenabled = false\n",
        )
        .expect("write profile");

        let tree = echo_plugin_loader::load(&dir.join("plugins.toml")).expect("load");
        let entries = static_entries(&tree);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "有名字");
        assert!(entries[0].enabled);
        // name 写成空串 → 回退 id（面板要有东西可显示；name 是 schema 必填，
        // 所以"未写"不可达，可达的是空串）。
        assert_eq!(entries[1].name, "unnamed");
        // enabled = false 是配置选择，不是故障 → error 仍为 None。
        assert!(!entries[2].enabled);
        assert_eq!(entries[2].error, None);
        assert_eq!(entries[2].kind, "stdio");
        // 启动命令随条目带出（面板排查「为什么不起来」要看它）。
        assert_eq!(entries[0].command, "/bin/x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 启动失败的原因回填到对应静态条目（面板「失败」徽标的数据源）。
    #[test]
    fn start_errors_are_attached_to_static_entries() {
        // 直接验证回填逻辑本身（真实拉起子进程由 bootstrap 的测试覆盖）。
        let mut entries = vec![
            StaticPluginEntry {
                id: "ok-ext".into(),
                name: "ok".into(),
                kind: "stdio".into(),
                enabled: true,
                error: None,
                command: "/bin/ok".into(),
                args: vec![],
                env_keys: vec![],
            },
            StaticPluginEntry {
                id: "bad-ext".into(),
                name: "bad".into(),
                kind: "stdio".into(),
                enabled: true,
                error: None,
                command: "/bin/bad".into(),
                args: vec![],
                env_keys: vec![],
            },
        ];
        let started_errors = [("bad-ext".to_string(), "spawn failed".to_string())];
        for entry in &mut entries {
            if let Some((_, error)) = started_errors.iter().find(|(id, _)| id == &entry.id) {
                entry.error = Some(error.clone());
            }
        }
        assert_eq!(entries[0].error, None);
        assert_eq!(entries[1].error.as_deref(), Some("spawn failed"));
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
