//! modules.toml 装载 + 子进程模块启动（解耦计划 P3/P4）。
//!
//! 配置文件缺省放在 core.toml 同目录的 `modules.toml`；不存在 = 该特性
//! 休眠（零行为变化）。
//!
//! 语义：
//! - [`start_with_specs`]：`config_path.with_file_name("modules.toml")`
//!   不存在 → `None`（debug 日志）。存在 → [`echo_module_loader::load`]
//!   合成 / 校验模块树；失败 → warn + `None`。之后 best-effort 启动全部
//!   enabled 的 stdio 模块（单个失败逐条 warn 但继续）。成功 →
//!   `tracing::info!(count, skipped)` 并返回 [`ModuleRuntime`]。
//! - [`ModuleRuntime::tools`]：全部已启动模块的远程工具模块实例
//!   （`Arc<RemoteTool>`——`RemoteTool` 已实现 `agent_module_loop::tool::Tool`，
//!   注册时可直接向上转型为 `Arc<dyn Tool>`；同时保留 `package()` 供注册
//!   方打「按包分组」标签）。
//! - [`ModuleRuntime::shutdown`]：5s 期限排空全部句柄（best-effort）。
//! - [`dump_config`]：`dump-config` 子命令的实现——只合成不启动，把分层
//!   合成树渲染回 TOML（每行带来源注释）。
//! - [`builtin_mount_plan`]：`modules.toml` 里 `kind = "builtin"` 的行 →
//!   内置模块的挂载计划（配置化组合 P4：集合 / 顺序 / 启用由配置说了算）。
//! - [`config_origins`]：每个模块条目由**哪一层**配置声明（面板展示用）。
//! - [`ensure_modules_base`]：缺失时生成 `modules-base.toml`（内置模块基线），
//!   让"宿主带了什么、什么顺序"从隐式变成可读可改。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use echo_module_host::bootstrap::StartedModules;
use echo_module_host::{HostServiceRegistry, ModuleSupervisor};
use agent_module_loop::adapter::{CoreApiService, RemoteInstance, RemoteTool};

/// 已装载并启动的子进程模块运行时。
pub struct ModuleRuntime {
    /// 启动结果（句柄 + 单点失败记录）。
    pub started: StartedModules,
    /// 未启动条目的人可读原因（disabled / 非 stdio 运输层等）。
    pub skipped: Vec<String>,
    /// 模块实例宿主服务（`core_api`；实例模块的入站 / 状态回呼入口；
    /// P1，2026-10 实例模块化）。无模块实例贡献时为空注册表（无副作用）。
    pub core_api: Arc<CoreApiService>,
    /// `modules.toml` **静态条目**的元数据（id / name / kind / 是否启用 /
    /// 启动失败原因）。
    ///
    /// 只含静态条目：动态 `LaunchSpec`（QQ 实例）另有自己的门控壳，不能
    /// 重复注册。见 [`ModuleRuntime::gate_shells`]。
    pub entries: Vec<StaticModuleEntry>,
    /// 各启动条目的原始 Hello.config；热替换必须原样沿用。
    launch_configs: std::collections::HashMap<String, serde_json::Value>,
}

/// 技能贡献来源：把已启动模块的 `Register` 贡献交给技能表。
///
/// 持 `ModuleHandle` 的**克隆**而不是 `Arc<ModuleRuntime>`：句柄本身是
/// `Arc<Shared>` 的包装，克隆廉价，而 `ModuleRuntime` 不是 `Clone`——用句柄
/// 也让"每次调用读最新贡献"这件事自然成立（热替换后 `Register` 会重发）。
pub struct ModuleSkillContributions {
    handles: Vec<echo_module_host::ModuleHandle>,
    /// 编译内置模块的技能（无 stdio 句柄，内容随 Core 二进制）。
    builtin: Vec<(String, Vec<echo_defs::skill::Skill>)>,
}

impl ModuleSkillContributions {
    /// `started` 为 `None` = 进程里没有外部模块运行时（没配 `modules.toml`），
    /// 此时只剩 `builtin` 一层。
    pub fn new(
        started: Option<&StartedModules>,
        builtin: Vec<(String, Vec<echo_defs::skill::Skill>)>,
    ) -> Self {
        Self {
            handles: started.map(|s| s.handles.clone()).unwrap_or_default(),
            builtin,
        }
    }
}

impl agent_module_skills::host_keys::SkillContributionSource for ModuleSkillContributions {
    fn contributed_skills(&self) -> Vec<(String, Vec<echo_defs::skill::Skill>)> {
        use agent_module_loop::adapter::ModuleHandleSkills;
        self.handles
            .iter()
            .map(|handle| handle.contributed_skills())
            .chain(self.builtin.iter().cloned())
            .filter(|(_, skills)| !skills.is_empty())
            .collect()
    }
}

/// `modules.toml` 静态条目（用于在模块清单里显示**外部模块**）。
#[derive(Debug, Clone)]
pub struct StaticModuleEntry {
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
    /// 启动命令（`modules.toml` 的 `command`；面板据此排查「为什么不起来」）。
    pub command: String,
    /// 启动参数。
    pub args: Vec<String>,
    /// 环境变量**键名**（值不下发：常含 token / 密钥）。
    pub env_keys: Vec<String>,
}

/// 启动入口（P1.8，2026-10 实例模块化）：
/// 在 `modules.toml` 静态条目之外追加组合根即时组装的 [`LaunchSpec`]——
/// 实例模块按**实例**拉起（每实例独立进程 / Hello.config），而
/// `modules.toml` 只加载一次，无法承载运行期分配（端口 / persona / state 文件）。
///
/// 语义：
/// - `modules.toml` 存在 → 静态条目 + `extra_specs` 合并启动；
/// - 不存在但 `extra_specs` 非空 → 仅启动动态实例；
/// - 两者皆空 → `None`（休眠，零行为变化）。
///
/// [`LaunchSpec`]: echo_module_host::bootstrap::LaunchSpec
pub async fn start_with_specs(
    config_path: &Path,
    register_host_services: impl FnOnce(&HostServiceRegistry),
    extra_specs: Vec<echo_module_host::bootstrap::LaunchSpec>,
) -> Option<ModuleRuntime> {
    let path = config_path.with_file_name("modules.toml");
    let mut load_failed = false;
    let mut entries: Vec<StaticModuleEntry> = Vec::new();
    let (mut specs, skipped) = if path.exists() {
        match echo_module_loader::load(&path) {
            Ok(tree) => {
                // 记下静态条目（进模块清单）+ 逐条诊断（同层重复 id 等）。
                entries = static_entries(&tree);
                echo_module_host::bootstrap::launch_specs(&tree)
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "modules.toml load failed; continuing with dynamic specs only"
                );
                load_failed = true;
                (Vec::new(), Vec::new())
            }
        }
    } else {
        tracing::debug!(
            path = %path.display(),
            "modules.toml not found; static subprocess modules dormant"
        );
        (Vec::new(), Vec::new())
    };
    if !extra_specs.is_empty() {
        tracing::info!(
            count = extra_specs.len(),
            "dynamic module specs merged (adapter instances)"
        );
        specs.extend(extra_specs);
    }
    // 无静态条目且无动态实例：休眠（非法 modules.toml 也按原语义休眠，
    // 不改变「错误配置 = 特性不启用」的行为）。
    if specs.is_empty() && (!path.exists() || load_failed) {
        return None;
    }
    // 频率限额用 supervisor 的缺省策略（事件 2000/s、回呼 500/s、日志 200/s
    // ——合法模块远够不到，失控循环会被挡住）。目前不做 per-module 配置：
    // 这是宿主保护，不是模块特性。
    let launch_configs = specs
        .iter()
        .map(|spec| (spec.module_id.clone(), spec.config.clone()))
        .collect();
    let supervisor = ModuleSupervisor::new();
    register_host_services(&supervisor.host_services());
    // P1：模块实例宿主服务先行注册——模块就绪后其 `submit_input` / `report_*`
    // 回呼即有接收面（否则实例模块启动即报 instance_not_found）。
    let core_api = Arc::new(CoreApiService::new());
    supervisor
        .host_services()
        .register_async("core_api", core_api.clone());
    // 旧模块按名 `adapter_host` 查找宿主服务（双注册过渡，2026-10 去适配器化）。
    supervisor
        .host_services()
        .register_async("adapter_host", core_api.clone());
    let started = echo_module_host::bootstrap::start_modules(&supervisor, specs).await;
    for (module_id, error) in &started.errors {
        tracing::warn!(module = %module_id, %error, "subprocess module start failed");
    }
    // 启动失败原因回填到静态条目：模块清单里能直接看到「为什么没起来」。
    // 只认静态条目 id——动态实例（QQ）的失败另有实例状态通道上报。
    for entry in &mut entries {
        if let Some((_, error)) = started.errors.iter().find(|(id, _)| id == &entry.id) {
            entry.error = Some(error.clone());
        }
    }
    let runtime = ModuleRuntime {
        started,
        skipped,
        core_api,
        entries,
        launch_configs,
    };
    for reason in &runtime.skipped {
        tracing::debug!(reason = %reason, "module entry skipped");
    }
    tracing::info!(
        count = runtime.started.handles.len(),
        skipped = runtime.skipped.len(),
        "subprocess modules started from modules.toml"
    );
    Some(runtime)
}

/// 已启动子进程模块的生命周期控制面（组合根注入进 [`ModuleHost`]）。
///
/// 持有每个静态条目的 [`ModuleHandle`]：面板的启停开关据此**真正停掉子进程**
/// （而不是只改注册表状态），重启按钮据此执行兼容热替换。
/// 动态条目（QQ 实例）不在表里——它们的生命周期归实例模块。
pub struct SubprocessControl {
    handles: std::collections::HashMap<String, echo_module_host::supervisor::ModuleHandle>,
    /// 配置里 `enabled = false` 的条目（没有 handle，但要显示为**已停止**，
    /// 否则面板对这类条目连运行态都不显示）。
    disabled: std::collections::HashSet<String>,
    /// 启动就没起来的条目（握手失败 / 命令不存在）：没有 handle，
    /// 但状态应为 **failed** + 原因（面板的「失败」徽标与详情块读它）。
    failed: std::collections::HashMap<String, String>,
    /// 每个静态条目的启动配置（热替换时原样传给 replacement）。
    launch_configs: std::collections::HashMap<String, serde_json::Value>,
}

impl SubprocessControl {
    /// 从启动结果建表（只看静态条目：动态 `LaunchSpec` 的 id 不在 entries 里）。
    pub fn new(runtime: &ModuleRuntime) -> Self {
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
            .filter(|handle| known.contains(handle.module_id()))
            .map(|handle| (handle.module_id().to_string(), handle.clone()))
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
            launch_configs: runtime.launch_configs.clone(),
        }
    }
}

impl agent_module_loop::modules::SubprocessModuleControl for SubprocessControl {
    fn stop<'a>(
        &'a self,
        id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let Some(handle) = self.handles.get(id) else {
                // 启动失败 / 配置里就没启动的条目同样允许禁用：
                // 这时没有子进程可停，操作语义是「保持不启动」而非失败。
                if self.disabled.contains(id) || self.failed.contains_key(id) {
                    return Ok(());
                }
                return Err(format!("{id}: 未知外部模块"));
            };
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
                .ok_or_else(|| format!("{id}: 不是已启动的外部模块"))?;
            handle
                .start()
                .await
                .map_err(|error| format!("{id} start failed: {error}"))
        })
    }

    fn upgrade<'a>(
        &'a self,
        id: &'a str,
        timeout: std::time::Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let handle = self
                .handles
                .get(id)
                .ok_or_else(|| format!("{id}: 不是已启动的外部模块"))?;
            let config = self
                .launch_configs
                .get(id)
                .cloned()
                .ok_or_else(|| format!("{id}: 缺少启动配置，拒绝热替换"))?;
            handle
                .upgrade_preserving_contributions(config, timeout)
                .await
                .map_err(|error| format!("{id} hot upgrade failed: {error}"))
        })
    }

    fn status(&self, id: &str) -> Option<agent_module_loop::modules::SubprocessStatus> {
        // 没有 handle 的两种已知情形（都要给出明确状态，不能"查不到"）：
        // ① 启动就没起来 → failed + 原因；② 配置关掉从未启动 → stopped。
        let Some(handle) = self.handles.get(id) else {
            if let Some(error) = self.failed.get(id) {
                return Some(agent_module_loop::modules::SubprocessStatus {
                    state: "failed".to_string(),
                    restarts: 0,
                    generation: 0,
                    error: Some(error.clone()),
                });
            }
            return self.disabled.contains(id).then(|| {
                agent_module_loop::modules::SubprocessStatus {
                    state: "stopped".to_string(),
                    restarts: 0,
                    generation: 0,
                    error: None,
                }
            });
        };
        let state = handle.state();
        Some(agent_module_loop::modules::SubprocessStatus {
            state: state.as_str().to_string(),
            restarts: handle.restarts(),
            generation: handle.generation(),
            error: state.error().map(str::to_string),
        })
    }
}

/// 把合成树里的**静态**条目转成清单元数据（不含动态 `LaunchSpec`）。
fn static_entries(tree: &echo_module_loader::LoadedTree) -> Vec<StaticModuleEntry> {
    tree.entries
        .iter()
        .map(|entry| StaticModuleEntry {
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

/// 一个已注册的远程模块实例 + 其入站队列 receiver（组合根接到送达入口；
/// P1，2026-10 实例模块化）。
pub struct RemoteInstanceHandle {
    /// 远程模块实例（注册进 `InstanceRegistry`）。
    pub instance: Arc<RemoteInstance>,
    /// 入站消息队列 receiver（模块 `submit_input` 的出站端；
    /// 每实例串行 worker 消费）。
    pub inbound_rx: tokio::sync::mpsc::Receiver<agent_module_loop::adapter::IncomingMessage>,
}

impl ModuleRuntime {
    /// 全部已启动模块的远程工具模块实例（未注册工具贡献的模块贡献空表）。
    pub fn tools(&self) -> Vec<Arc<RemoteTool>> {
        agent_module_loop::adapter::tools_of(&self.started)
    }

    /// 全部已启动模块的模块实例贡献的远程模块实例（P1，2026-10 实例模块化）。
    ///
    /// 每个 `(InstanceContribution, ModuleHandle)` 构造一个 `RemoteInstance`
    /// 并注册进 `core_api`（入站队列 / 状态回执入口就位，队列 receiver
    /// 随元组带出）；组合根随后把模块实例注册进 `InstanceRegistry`，并把
    /// receiver 接到消息送达入口（见 `main.rs` 装配）。
    pub fn instances(&self) -> Vec<RemoteInstanceHandle> {
        agent_module_loop::adapter::instance_contributions(&self.started)
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

    /// 模块实例宿主服务（`core_api`；模块回呼的接收面）。
    pub fn core_api(&self) -> Arc<CoreApiService> {
        self.core_api.clone()
    }

    /// 优雅关停全部模块（5s 期限；单点错误由 bootstrap 内部记 debug 日志）。
    pub async fn shutdown(&self) {
        self.started.shutdown(Duration::from_secs(5)).await;
    }
}

/// `dump-config` 子命令：把 `modules.toml` 的**分层合成结果**渲染回 TOML。
///
/// 只**合成不启动**——不拉起任何子进程、不碰 kernel / 网络，纯函数式地
/// 走一遍 [`echo_module_loader::load`]（profile → bundles → patches →
/// 行内条目 的 dsh 式"按 id 定位、整行替换"），再把结果交给
/// [`echo_module_loader::dump`] 渲染成带来源注释的 TOML：
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
/// 返回 `Ok(None)` = `modules.toml` 不存在（外部模块特性休眠，与启动
/// 路径的"零行为变化"一致——不是错误）。
pub fn dump_config(config_path: &Path) -> Result<Option<String>, echo_module_loader::LoadError> {
    let path = config_path.with_file_name("modules.toml");
    if !path.exists() {
        return Ok(None);
    }
    let tree = echo_module_loader::load(&path)?;
    let mut text = echo_module_loader::dump(&tree);
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

/// 基线文件（`modules-base.toml`）的文件名，与装载器共用同一个常量。
pub const BASE_FILE_NAME: &str = echo_module_loader::BASE_FILE_NAME;

/// 基线文件缺失时，按**当前目录**（编译进 Core 的这批模块）生成一份。
///
/// ## 为什么要生成，而不是让用户手写
///
/// 内置模块的集合此前是隐式的：`modules.toml` 不提它们，它们照样挂载。
/// 结果是"一个部署到底装了什么、什么顺序"只能去面板里翻——而**顺序**恰恰
/// 是能在面板里调不了的（面板只有开关）。基线文件把这份集合变成可读可改
/// 的文本。
///
/// 手写不现实：十几个模块的 id 又长又容易拼错，升级后新增的模块没人会
/// 记得补。所以由宿主生成，内容就是"当前目录，全部启用，按目录顺序"——
/// 与不写这份文件时的行为**逐字等价**，纯声明。
///
/// ## 只在缺失时写
///
/// 用户改过的、删掉的都保持原样：删掉等于回到隐式行为（下次启动不会再
/// 长出来？——会长出来，但它的内容仍然是"什么都不改变"，所以删了也没有
/// 副作用，只是文件回来而已；真要永久关掉基线，写 `enabled = false` 的
/// 那一行，而不是删文件）。
///
/// 写盘失败只 warn：一个诊断性的配置文件不该阻断启动。
///
/// ## 为什么 `name` 写成注释
///
/// `[[module]]` 的 `name` 字段对 `kind = "builtin"` **不生效**（标题来自
/// 模块自己的 manifest）。写成键会让人以为改它就能改面板标题——已经有人
/// 这么误会过。写成注释既有可读性，又不会被误认为配置。
pub fn ensure_modules_base(config_path: &Path, entries: &[(String, String)]) {
    let path = config_path.with_file_name(BASE_FILE_NAME);
    if path.exists() {
        return;
    }
    // 用 raw 字符串：正文里有反引号包着的 `enabled = false` 与引号，逐行
    // 转义写出来只会更容易出错（第一版就漏了一个引号，编译期才炸）。
    let mut text = String::from(
        r#"# 模块基线：编译进 Core 的模块的声明（由 Core 在缺失时生成，可自由修改）。
#
# 这一份只回答「宿主带了哪些模块、按什么顺序挂载、默认开不开」。
# 追加的东西（外部仓模块、子进程模块）写在同目录的 `modules.toml`。
#
# 语义：
#   - 这里没提到的内置模块**照样挂载**（升级后新增的模块不会因为这份文件
#     过时而消失）——想关掉必须显式写 enabled = false；
#   - 行的先后就是挂载顺序；
#   - [module.config] 在 mount 时下发给该模块（内置模块因此能改配置不改代码）。
#     同目录 `modules.toml` 里同 id 的行会**整行覆盖**这里的一条（位置不变，
#     且不继承这里的 config——覆盖就要写全套）；
#   - kind = kernel 的模块是运行地基，写 enabled = false 会导致启动失败并点名
#     （而不是静默跳过）。

"#,
    );
    for (id, name) in entries {
        text.push_str(&format!("# {name}\n[[module]]\nid = \"{id}\"\nkind = \"builtin\"\nenabled = true\n\n"));
    }
    if let Err(error) = std::fs::write(&path, text) {
        tracing::warn!(path = %path.display(), %error, "write modules-base.toml failed; builtin baseline stays implicit");
        return;
    }
    tracing::info!(
        path = %path.display(),
        count = entries.len(),
        "modules-base.toml generated (builtin baseline; edit it to change order or disable a builtin module)"
    );
}

/// 合成模块树：`modules.toml`（profile）+ 同目录的 `modules-base.toml`（基线）。
///
/// 判据是**两个文件任一存在**，不是只看 profile。基线本身就是一份能单独生效
/// 的声明：用户删掉 profile、只想留一份基线时，基线必须仍然被读到——否则
/// "删掉 modules.toml 回到隐式行为"会变成"基线的开关全部失效"，两个说法
/// 矛盾，而后者更难查（配置还在，就是不生效）。
///
/// 返回 `None` 只表示**两个文件都没有**（外部模块特性休眠，与改造前一致）。
fn load_tree(config_path: &Path) -> Option<echo_module_loader::LoadedTree> {
    let profile = config_path.with_file_name("modules.toml");
    let base = config_path.with_file_name(BASE_FILE_NAME);
    let path = if profile.exists() {
        profile
    } else if base.exists() {
        // 没有 profile 时直接以基线为入口：装载器会因为"文件名等于基线"而
        // 跳过自动应用，正好读一次、不重复。
        base
    } else {
        return None;
    };
    match echo_module_loader::load(&path) {
        Ok(tree) => Some(tree),
        Err(error) => {
            // 与启动路径同款：坏文件 warn 后按"无配置"继续（内置模块照旧
            // 全挂载），把可读错误留给用户从日志里看到具体哪一行坏了。
            tracing::warn!(path = %path.display(), %error, "modules config load failed; builtin plan falls back to defaults");
            None
        }
    }
}

/// `modules.toml` 里 `kind = "builtin"` 的行 → 内置模块的**挂载计划**。
///
/// 只挑 `kind = "builtin"`：`stdio` 行走子进程启动路径（[`start_with_specs`]），
/// 两类条目在同一次合成里共存、各管各的。
///
/// 语义（与启动路径共用同一个 `load()`，所以不存在"dump 看到的树"与
/// "装载的树"不一致）：
/// - 文件不存在 / 解析失败 → **空计划**（外部模块特性休眠，或是坏文件——
///   后者启动路径会 warn 并继续，这里同样不阻断启动：内置模块的缺省行为
///   是"全挂载"，空计划正好等价于改造前的行为）；
/// - 条目顺序即挂载顺序（`requires` 已在合成期拓扑排序过）；
/// - `enabled = false` 的内置行 → 计划里保留但标记禁用：注册表仍会注册它
///   （Panel 清单可见、可就地启用），只是启动时不挂载。
///
/// 未在计划里出现的内置模块仍按目录顺序挂载——见
/// [`agent_module_loop::modules::resolve_mount_order`] 的说明。
pub fn builtin_mount_plan(config_path: &Path) -> Vec<agent_module_loop::modules::MountStep> {
    use echo_module_loader::EntryKind;
    let Some(tree) = load_tree(config_path) else {
        return Vec::new();
    };
    tree.entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Builtin)
        .map(|entry| agent_module_loop::modules::MountStep::new(entry.id.clone(), entry.enabled))
        .collect()
}

/// `modules.toml` 里内置行（`kind = "builtin"`）的 `[module.config]` → 配置表。
///
/// 这是**配置下发**的装载端：结果交给
/// [`agent_module_loop::modules::ModuleHost::set_configs`]，注册表在 `mount` 时按 id
/// 注入 `MountContext.config`，于是 in-process 模块不必自己去找并解析
/// `modules.toml`（也就不会读到别人的配置）。
///
/// 只挑 `kind = "builtin"`：`stdio` 行的配置本来就经 `Hello.config` 下发给
/// 子进程，两条路互不重叠。
///
/// 降级：文件不存在 / 解析失败 / 该行没写 `[module.config]` → 空表
/// （与 [`config_origins`] 同款：配置缺失不该阻断启动）。
pub fn builtin_configs(config_path: &Path) -> Vec<(String, serde_json::Value)> {
    use echo_module_loader::EntryKind;
    let Some(tree) = load_tree(config_path) else {
        return Vec::new();
    };
    tree.entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Builtin)
        // `config` 缺省是空对象；没有键的行不必登记（注册表缺省即 Null）。
        .filter(|entry| entry.config.as_object().is_some_and(|m| !m.is_empty()))
        .map(|entry| (entry.id.clone(), entry.config.clone()))
        .collect()
}

/// `modules.toml` 里每个条目由**哪一层**声明 → `(模块 id, 来源描述)`。
///
/// 分层合成（profile → bundles → patches → 行内）之后，每个条目都带着
/// `layer`（0 起的应用序号）与 `layer_source`（文件名）。面板的模块清单
/// 用它回答「这行为什么长这样 / 我要去哪一层改它」。
///
/// 描述形如 `modules.toml`（本文件行内条目）或 `base.toml · layer 0`
/// （被 bundle/patch 覆盖过的行按**最后写入**的那一层显示——与合成语义
/// 一致：整行替换）。
///
/// 配置未提及的模块不会出现在结果里（调用方保持空串 = 编译内置的缺省）。
/// 文件不存在 / 解析失败 → 空表（与 [`builtin_mount_plan`] 同款降级：
/// 展示信息缺失不该阻断启动）。
pub fn config_origins(config_path: &Path) -> Vec<(String, String)> {
    let Some(tree) = load_tree(config_path) else {
        return Vec::new();
    };
    let profile_name = config_path
        .with_file_name("modules.toml")
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("modules.toml")
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
            "echo-core-modules-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ))
    }

    /// modules.toml 不存在 → None（特性休眠，零行为变化）。
    #[tokio::test]
    async fn missing_modules_toml_is_dormant() {
        let dir = unique_dir("missing");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("echo-agent-core.toml");
        assert!(start_with_specs(&config_path, |_| {}, Vec::new())
            .await
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 动态 specs（模块实例）穿透 modules.toml 休眠门控：无 modules.toml
    /// 但有 extra specs → 仍启动（命令不存在 → 逐条失败记录，runtime 仍在）。
    #[tokio::test]
    async fn dynamic_specs_start_without_modules_toml() {
        let dir = unique_dir("dynamic");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("echo-agent-core.toml");
        let spec = echo_module_host::bootstrap::LaunchSpec {
            module_id: "adapter-qq-test".to_string(),
            command: "/nonexistent/agent-module-adapter-qq-test".to_string(),
            args: vec![],
            env: Default::default(),
            config: serde_json::json!({"name": "qq-test"}),
            limits: None,
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

    /// 无 modules.toml → `Ok(None)`：休眠不是错误（与启动路径同语义）。
    #[test]
    fn dump_config_reports_dormant_when_missing() {
        let dir = unique_dir("dump-missing");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("echo-agent-core.toml");
        assert!(
            dump_config(&config_path)
                .expect("missing file is not an error")
                .is_none(),
            "no modules.toml must yield None, not an error"
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
            "[[module]]\nid = \"a\"\nkind = \"stdio\"\nname = \"a-base\"\ncommand = \"/bin/a\"\n",
        )
        .expect("write bundle");
        std::fs::write(
            dir.join("over.toml"),
            "[[module]]\nid = \"a\"\nkind = \"stdio\"\nname = \"a-patched\"\ncommand = \"/bin/a2\"\n",
        )
        .expect("write patch");
        std::fs::write(
            dir.join("modules.toml"),
            "[profile]\nbundles = [\"base\"]\npatches = [\"over\"]\n\n             [[module]]\nid = \"b\"\nkind = \"stdio\"\nname = \"b\"\ncommand = \"/bin/b\"\n",
        )
        .expect("write profile");

        let text = dump_config(&dir.join("echo-agent-core.toml"))
            .expect("compose")
            .expect("modules.toml exists");
        // 来源注释把「哪一层」写在每行上方：bundle=0、patch=1、行内=2。
        assert!(text.contains("# --- a (layer 1: over.toml) ---"), "{text}");
        assert!(
            text.contains("# --- b (layer 2: modules.toml) ---"),
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
        assert!(reparsed["module"]
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
            dir.join("modules.toml"),
            "[[module]]\nid = \"a\"\nkind = \"stdio\"\nname = \"a\"\ncommand = \"/bin/a\"\n             requires = [\"ghost\"]\n",
        )
        .expect("write profile");
        let error = dump_config(&dir.join("echo-agent-core.toml")).expect_err("must fail");
        assert!(error.to_string().contains("ghost"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 内置模块挂载计划（配置化组合 P4）────────────────────────────────

    /// 无 modules.toml → 空计划（= 内置模块按目录顺序全挂载，行为不变）。
    #[test]
    fn builtin_plan_is_empty_without_modules_toml() {
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
            dir.join("modules.toml"),
            "[[module]]\nid = \"external\"\nkind = \"stdio\"\nname = \"ext\"\ncommand = \"/bin/ext\"\n\n\
             [[module]]\nid = \"skills.dir\"\nkind = \"builtin\"\nname = \"技能目录\"\n\n\
             [[module]]\nid = \"tools.builtin\"\nkind = \"builtin\"\nname = \"内置工具集\"\nenabled = false\n",
        )
        .expect("write profile");

        let plan = builtin_mount_plan(&dir.join("echo-agent-core.toml"));
        let ids: Vec<&str> = plan.iter().map(|step| step.id.as_str()).collect();
        assert_eq!(ids, vec!["skills.dir", "tools.builtin"], "{plan:?}");
        assert!(plan[0].enabled, "缺省 enabled = true");
        assert!(!plan[1].enabled, "显式 enabled = false 必须带出");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 坏文件 → 空计划（不阻断启动）：内置模块回到缺省全挂载，
    /// 与子进程路径"warn 后继续"保持一致。
    #[test]
    fn builtin_plan_falls_back_to_defaults_on_broken_file() {
        let dir = unique_dir("plan-broken");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("modules.toml"), "not = valid = toml [[[").expect("write toml");
        assert!(builtin_mount_plan(&dir.join("echo-agent-core.toml")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 配置下发（模块配置装载端）────────────────────────────────────

    /// 无 modules.toml → 空表（配置下发休眠，模块看到 `Null`）。
    #[test]
    fn builtin_configs_empty_without_modules_toml() {
        let dir = unique_dir("cfg-missing");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        assert!(builtin_configs(&dir.join("echo-agent-core.toml")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只取内置行的 `[module.config]`：stdio 行的配置走 `Hello.config`，
    /// 两条路不重叠；没写 config 的内置行不登记（注册表缺省即 `Null`）。
    #[test]
    fn builtin_configs_picks_only_configured_builtin_rows() {
        let dir = unique_dir("cfg-pick");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("modules.toml"),
            "[[module]]\nid = \"external\"\nkind = \"stdio\"\nname = \"ext\"\ncommand = \"/bin/ext\"\n\
             [module.config]\nport = 1\n\n\
             [[module]]\nid = \"echo-agent.management.panel\"\nkind = \"builtin\"\nname = \"管理面\"\n\
             [module.config]\naddress = \"127.0.0.1:8737\"\n\n\
             [[module]]\nid = \"echo-agent.skills.dir\"\nkind = \"builtin\"\nname = \"技能目录\"\n",
        )
        .expect("write profile");

        let configs = builtin_configs(&dir.join("echo-agent-core.toml"));
        assert_eq!(configs.len(), 1, "只有管理面写了 config: {configs:?}");
        assert_eq!(configs[0].0, "echo-agent.management.panel");
        assert_eq!(configs[0].1["address"], "127.0.0.1:8737");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 坏文件 → 空表（与 `builtin_mount_plan` 同款：配置读不到不该阻断启动）。
    #[test]
    fn builtin_configs_fall_back_to_empty_on_broken_file() {
        let dir = unique_dir("cfg-broken");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("modules.toml"), "not = valid = toml [[[").expect("write toml");
        assert!(builtin_configs(&dir.join("echo-agent-core.toml")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 装配集成：装载端 + 注册表**串起来**——`modules.toml` 写下的配置
    /// 真的到达模块的 `mount` 闭包。这是「配置下发」的唯一端到端证据，
    /// 只测 `builtin_configs` 或只测注册表都盖不住接缝。
    #[test]
    fn module_config_reaches_the_mount_closure_end_to_end() {
        use std::sync::{Arc, Mutex};
        let dir = unique_dir("cfg-e2e");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("modules.toml"),
            "[[module]]\nid = \"t.management\"\nkind = \"builtin\"\nname = \"管理面\"\n\
             [module.config]\naddress = \"127.0.0.1:9999\"\n",
        )
        .expect("write profile");

        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let slot = seen.clone();
        let module = echo_module::BuiltinModule::new(
            echo_module::ModuleManifest::builtin(
                "t.management",
                "管理面",
                "1",
                "management",
                "",
            ),
            move |mctx: &echo_module::MountContext| {
                *slot.lock().unwrap() = mctx.config_str("address").map(str::to_string);
                Ok(Vec::new())
            },
        );
        let host = agent_module_loop::modules::ModuleHost::new();
        host.set_configs(builtin_configs(&dir.join("echo-agent-core.toml")));
        host.register_and_mount(Arc::new(module)).expect("mount ok");

        assert_eq!(seen.lock().unwrap().as_deref(), Some("127.0.0.1:9999"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 内置模块基线（modules-base.toml）────────────────────────────

    /// 缺失时生成，且生成的内容**通过装载器回放**得到同样的内置行。
    ///
    /// 断言走一遍 `builtin_mount_plan`（真正的消费方）而不是只比字符串：
    /// 生成的文件如果格式不对，在这里就会变成"计划是空的"，而不是等到
    /// 部署后才发现内置模块的开关不生效。
    #[test]
    fn ensure_modules_base_generates_a_loadable_baseline() {
        let dir = unique_dir("base-generate");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config = dir.join("echo-agent-core.toml");
        let entries = vec![
            ("echo-agent.tools".to_string(), "内置工具集".to_string()),
            ("echo-agent.skills".to_string(), "技能目录".to_string()),
        ];

        ensure_modules_base(&config, &entries);

        let path = dir.join(BASE_FILE_NAME);
        assert!(path.is_file(), "缺失时必须生成");
        let text = std::fs::read_to_string(&path).expect("read baseline");
        // 名字进注释、不进 `name =`：那个字段对 builtin 行不生效（面板标题
        // 来自模块自己的 manifest），写成键会让人以为改它能改标题。
        assert!(text.contains("# 内置工具集"), "名字应以注释形式出现");
        assert!(!text.contains("name ="), "内置行的 name 字段不生效，不该写成键");

        let plan = builtin_mount_plan(&config);
        let ids: Vec<&str> = plan.iter().map(|step| step.id.as_str()).collect();
        assert_eq!(ids, vec!["echo-agent.tools", "echo-agent.skills"]);
        assert!(plan.iter().all(|step| step.enabled), "生成的基线全部启用");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 已存在就**不动**——用户的编辑不该被启动流程覆盖。
    #[test]
    fn ensure_modules_base_never_overwrites() {
        let dir = unique_dir("base-keep");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config = dir.join("echo-agent-core.toml");
        let path = dir.join(BASE_FILE_NAME);
        std::fs::write(&path, "# 手写的基线\n[[module]]\nid = \"x\"\nkind = \"builtin\"\nname = \"x\"\nenabled = false\n")
            .expect("write baseline");

        ensure_modules_base(&config, &[("echo-agent.tools".to_string(), "内置工具集".to_string())]);

        let text = std::fs::read_to_string(&path).expect("read baseline");
        assert_eq!(text, "# 手写的基线\n[[module]]\nid = \"x\"\nkind = \"builtin\"\nname = \"x\"\nenabled = false\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 基线里的行会被 `builtin_mount_plan` 看见（装载器把同目录的基线当第 0 层）。
    #[test]
    fn baseline_rows_join_the_mount_plan() {
        let dir = unique_dir("base-plan");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config = dir.join("echo-agent-core.toml");
        std::fs::write(
            dir.join(BASE_FILE_NAME),
            "[[module]]\nid = \"echo-agent.tools\"\nkind = \"builtin\"\nname = \"t\"\nenabled = false\n",
        )
        .expect("write baseline");
        std::fs::write(
            dir.join("modules.toml"),
            "[[module]]\nid = \"ext.qq\"\nkind = \"stdio\"\nname = \"qq\"\ncommand = \"/bin/true\"\n",
        )
        .expect("write profile");

        let plan = builtin_mount_plan(&config);
        assert_eq!(plan.len(), 1, "只有基线里的 builtin 行进计划");
        assert_eq!(plan[0].id, "echo-agent.tools");
        assert!(!plan[0].enabled, "基线写 enabled = false 必须生效");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 配置来源层（面板展示用）──────────────────────────────────────

    /// 无 modules.toml → 空表（面板显示「配置未声明」）。
    #[test]
    fn config_origins_empty_without_modules_toml() {
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
            "[[module]]\nid = \"a\"\nkind = \"stdio\"\nname = \"a\"\ncommand = \"/bin/a\"\n\n\
             [[module]]\nid = \"overridden\"\nkind = \"stdio\"\nname = \"old\"\ncommand = \"/bin/old\"\n",
        )
        .expect("write bundle");
        std::fs::write(
            dir.join("modules.toml"),
            "[profile]\nbundles = [\"base\"]\n\n\
             [[module]]\nid = \"overridden\"\nkind = \"stdio\"\nname = \"new\"\ncommand = \"/bin/new\"\n",
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
            Some("modules.toml")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 外部条目进模块清单（此前完全不可见）──────────────────────────

    /// 静态条目按合成树取元数据：id / name（空回退 id）/ kind / enabled。
    #[test]
    fn static_entries_mirror_the_composed_tree() {
        let dir = unique_dir("entries");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("modules.toml"),
            "[[module]]\nid = \"with-name\"\nkind = \"stdio\"\nname = \"有名字\"\ncommand = \"/bin/x\"\n\n\
             [[module]]\nid = \"unnamed\"\nkind = \"stdio\"\nname = \"\"\ncommand = \"/bin/y\"\n\n\
             [[module]]\nid = \"off\"\nkind = \"stdio\"\nname = \"关掉的\"\ncommand = \"/bin/z\"\nenabled = false\n",
        )
        .expect("write profile");

        let tree = echo_module_loader::load(&dir.join("modules.toml")).expect("load");
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
            StaticModuleEntry {
                id: "ok-ext".into(),
                name: "ok".into(),
                kind: "stdio".into(),
                enabled: true,
                error: None,
                command: "/bin/ok".into(),
                args: vec![],
                env_keys: vec![],
            },
            StaticModuleEntry {
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

    /// modules.toml 存在但内容非法 → None（warn，但不 panic）。
    #[tokio::test]
    async fn invalid_modules_toml_returns_none_without_panic() {
        let dir = unique_dir("invalid");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("modules.toml"), "not = valid = toml [[[").expect("write toml");
        let config_path = dir.join("echo-agent-core.toml");
        assert!(start_with_specs(&config_path, |_| {}, Vec::new())
            .await
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
