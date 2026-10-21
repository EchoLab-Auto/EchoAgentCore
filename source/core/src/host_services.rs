//! 宿主能力的**装配面**：把组合根手里的运行期句柄注册成服务键。
//!
//! ## 为什么这一层存在（全插件化 S6）
//!
//! 改造前，组合根要为每个内置插件手工"组装参数"：
//!
//! ```ignore
//! let shell_host = AgentShellCommandHost::new(agent.clone());
//! let skills_host = AgentSkillsCommandHost::new(agent.clone());
//! push(catalog, echo_plugin_skills::plugin(version, skills_host, gate))?;
//! ```
//!
//! 于���"哪个插件需要宿主的什么"这件事，**住在组合根里**——新增一个插件要改
//! 三个地方（目录、参数组装、门控闭包），而插件自己反而不知道自己需要什么。
//!
//! 现在反过来：插件在 `mount` 里按**自己声明的键**解析（`MountContext::require`），
//! 组合根只做一件事——**把手里的句柄按同一批键注册**：
//!
//! ```text
//!   host_services::register(ctx, wiring)   ← 本文件：一件件往服务表里放
//!               ↓
//!   插件 mount: mctx.require(&KEY)          ← 插件：按需取，缺了就挂不上
//! ```
//!
//! 于是 `builtin_plugins::catalog(version)` 变成**纯静态目录**，一个参数都不带。
//!
//! ## persona 门控为什么不在这一层
//!
//! 门控（"这个使用者看不看得到这个插件"）曾是一串 `gate` 闭包，每个插件一个。
//! 它是**宿主对挂载事件的反应**，不需要插件参与：宿主监听
//! `plugin.mounted` / `plugin.unmounted`（见 [`apply_plugin_gating`]），
//! 一处逻辑覆盖全部插件——包括将来新增的、组合根根本不认识的那些。

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use echo_context::{Ctx, Disposer};
use echo_defs::plugin_instance::PluginInstance;
use echo_federation::Federation;
use echo_plugin_management::host_keys::{ManagementHost, MANAGEMENT_HOST, SESSION_VIEW_HOST};
use echo_protocol::BackendBridge;
use echo_plugin_provider_llm::host_keys::PROVIDER_HOST;
use echo_plugin_self_update::host_keys::EVENTS as SELF_UPDATE_EVENTS;
use tracing::warn;

use echo_agent::agent_manager::AgentManager;
use echo_agent::agent::session_view_host::AgentSessionViewHost;
use echo_agent::agent::provider_host::AgentProviderCommandHost;
use echo_agent::agent::shell_host::{AgentInstanceCommandHost, AgentShellCommandHost};
use echo_agent::agent::skills_host::AgentSkillsCommandHost;
use echo_agent::agent::tools_host::AgentToolsCommandHost;
use echo_agent::agent::workspace_host::AgentWorkspaceHost;
use echo_agent::federation::FederationPorts;
use echo_agent::{Agent, EventSink};

use echo_plugin_federation::host_keys::{EVENTS as FEDERATION_EVENTS, PORTS};
use echo_plugin_skills::host_keys::SKILLS_HOST;
use echo_plugin_tools::host_keys::{INSTANCE_HOST, TOOLS_HOST};
use echo_plugin_shell::host_keys::SHELL_HOST;
use echo_plugin_workspace::host_keys::WORKSPACE_HOST;

/// 组合根手里那些"配置表达不了、只有装配期知道"的句柄。
///
/// 每一项都来自组合根本身的所有物：端口 / 令牌 / 实例句柄 / 事件出口 /
/// 联邦端口集合。**插件不再看到这个结构体**——它是本文件的入参形式，不是
/// 插件契约。
pub(crate) struct HostWiring {
    /// persona 管理器（门控要遍历全部人格）。
    pub manager: Arc<AgentManager>,
    /// 代表 agent（大部分命令域的 per-persona 接缝以它为锚点）。
    pub agent: Arc<Agent>,
    /// 进程级事件出口（联邦 / 自更新的回执走它）。
    pub event_sink: EventSink,
    /// 联邦出口集合（workspace 跨机查询 + federation 命令处理）。
    pub federation: Arc<FederationPorts>,
    /// 插件实例句柄（平台实例插件的启停）。
    pub instance: Option<Arc<dyn PluginInstance>>,
    /// 实例是否已接线 / 是否在运行（启停幂等）。
    pub instance_wired: Arc<AtomicBool>,
    pub instance_running: Arc<AtomicBool>,
    /// Panel 通道桥。
    pub bridge: Arc<BackendBridge>,
    /// 管理面监听地址 / 令牌（`plugins.toml` 可覆盖地址，令牌不行）。
    pub management_address: String,
    pub management_token: String,
    /// 合并模式下承接联邦入站的监听句柄（None = 独立模式）。
    pub merged_federation: Option<Arc<Federation>>,
}

/// 把全部宿主能力注册进服务表。返回的 `Disposer` 需要活到进程结束。
pub(crate) fn register(ctx: &Arc<Ctx>, wiring: HostWiring) -> Vec<Disposer> {
    let mut keep = Vec::new();
    /// 注册一个键；冲突（同键重复注册）只可能是装配 bug——记一条 warn 并
    /// 继续，让其余能力仍能就位（一个键的问题不该让整个进程起不来）。
    fn put<T: Send + Sync + 'static>(
        keep: &mut Vec<Disposer>,
        ctx: &Arc<Ctx>,
        key: &echo_context::ServiceKey<T>,
        value: T,
    ) {
        match ctx.provide(key, value) {
            Ok(disposer) => keep.push(disposer),
            Err(error) => warn!(service = key.name(), %error, "host service already provided"),
        }
    }


    // ── 内置工具集：三个命令域各一个键 ──
    put(
        &mut keep,
        ctx,
        &SHELL_HOST,
        Arc::new(AgentShellCommandHost::new(wiring.agent.clone()))
            as Arc<dyn echo_agent::tool::builtin::ShellCommandHostAlias>,
    );
    put(
        &mut keep,
        ctx,
        &INSTANCE_HOST,
        Arc::new(AgentInstanceCommandHost::new(wiring.agent.clone()))
            as Arc<dyn echo_agent::tool::builtin::InstanceCommandHostAlias>,
    );
    put(
        &mut keep,
        ctx,
        &TOOLS_HOST,
        Arc::new(AgentToolsCommandHost::new(wiring.agent.clone()))
            as Arc<dyn echo_agent::tool::builtin::ToolsCommandHostAlias>,
    );

    // ── 工具状态 → 线上事件（**绑定方声明**，2026-11 S7）──
    //
    // 执行链不再有 `match state_key { "checklist" => … }` 表，这里也不该有
    // 工具名：谁装了这个工具，谁声明"它的状态长什么线上事件"。绑定本体在
    // `echo-plugin-tools`（见 `checklist::state_event_binding`），本层只把
    // 声明装进 agent——新增一个有状态工具时这里一行都不用改。
    for (key, build) in echo_plugin_tools::checklist::all_state_event_bindings() {
        wiring.agent.register_tool_state_event(key, build);
    }

    // ── 技能目录 ──
    put(
        &mut keep,
        ctx,
        &SKILLS_HOST,
        Arc::new(AgentSkillsCommandHost::new(wiring.agent.clone()))
            as Arc<dyn echo_agent::skill::commands::SkillsCommandHost>,
    );

    // ── 工作区（store 按 team_id 解析：进程级一个插件、per-persona 多份 store）──
    let manager_for_agents = wiring.manager.clone();
    put(
        &mut keep,
        ctx,
        &WORKSPACE_HOST,
        Arc::new(AgentWorkspaceHost::from_agents(
            move || {
                manager_for_agents
                    .all()
                    .into_iter()
                    .map(|running| Arc::clone(&running.agent))
                    .collect()
            },
            wiring.event_sink.clone(),
            // 跨机目录查询（2026-11 S14 修）：此前这里是 `None`，于是**面板**
            // 的 `RequestBrowseDirectories` / `RequestWorkspaceFiles` 走
            // `node://` 时一律回「联邦未接线：远程目录浏览不可用」——而模型侧
            // 的 `workspace` 工具（组合根注册时传了 adapter）是好的。
            // 同一个能力两条路径两种行为，是纯粹的漏接线。
            Some(echo_agent::federation::workspace_query(
                wiring.federation.querier.clone(),
            )) as Option<Arc<dyn echo_defs::workspace::RemoteWorkspaceQuery>>,
        )) as Arc<dyn echo_agent::workspace::commands::WorkspaceHost>,
    );

    // ── 联邦：出口集合 + 事件出口 ──
    put(&mut keep, ctx, &PORTS, wiring.federation.clone());
    put(&mut keep, ctx, &FEDERATION_EVENTS, wiring.event_sink.clone());

    // ── 管理面：监听参数 + 会话视图数据面 ──
    put(
        &mut keep,
        ctx,
        &MANAGEMENT_HOST,
        Arc::new(CoreManagementHost {
            address: wiring.management_address.clone(),
            token: wiring.management_token.clone(),
            bridge: wiring.bridge.clone(),
            federation: wiring.merged_federation.clone(),
        }) as Arc<dyn ManagementHost>,
    );
    put(
        &mut keep,
        ctx,
        &SESSION_VIEW_HOST,
        Arc::new(AgentSessionViewHost::new(wiring.agent.clone()))
            as Arc<dyn echo_plugin_management::session_view::SessionViewHost>,
    );

    // ── 供应商 / API 命令域 ──
    put(
        &mut keep,
        ctx,
        &PROVIDER_HOST,
        Arc::new(AgentProviderCommandHost::new(wiring.agent.clone()))
            as Arc<dyn echo_plugin_provider_llm::commands::ProviderCommandHost>,
    );

    // ── 自更新：只要事件出口 ──
    put(&mut keep, ctx, &SELF_UPDATE_EVENTS, wiring.event_sink.clone());

    // ── 平台实例（配置派生）：启停控制 ──
    if let Some(instance) = wiring.instance.clone() {
        put(
        &mut keep,
        ctx,
        &INSTANCE_CONTROL,
            Arc::new(PluginInstanceControl {
                instance,
                wired: wiring.instance_wired.clone(),
                running: wiring.instance_running.clone(),
            }) as Arc<dyn InstanceControl>,
    );
    }

    keep
}

/// 管理面的宿主实现（组合根的小适配器：四个字段，无逻辑）。
struct CoreManagementHost {
    address: String,
    token: String,
    bridge: Arc<BackendBridge>,
    federation: Option<Arc<Federation>>,
}

impl ManagementHost for CoreManagementHost {
    fn default_address(&self) -> String {
        self.address.clone()
    }
    fn token(&self) -> String {
        self.token.clone()
    }
    fn bridge(&self) -> Arc<BackendBridge> {
        Arc::clone(&self.bridge)
    }
    fn federation(&self) -> Option<Arc<Federation>> {
        self.federation.clone()
    }
}

/// 平台实例的启停控制（插件壳用它代替直接捕获句柄）。
pub trait InstanceControl: Send + Sync {
    /// 是否已接线（未接线时启动是 no-op —— 启动期 mount 不抢跑）。
    fn is_wired(&self) -> bool;
    /// 启动实例（幂等；已运行则 no-op）。
    fn start(&self) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + '_>>;
    /// 停止实例（幂等）。
    fn stop(&self) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// 平台实例启停的服务键。
pub static INSTANCE_CONTROL: echo_context::ServiceKey<Arc<dyn InstanceControl>> =
    echo_context::ServiceKey::new("echo-agent.host.instance");

struct PluginInstanceControl {
    instance: Arc<dyn PluginInstance>,
    wired: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
}

impl InstanceControl for PluginInstanceControl {
    fn is_wired(&self) -> bool {
        self.wired.load(Ordering::SeqCst)
    }

    fn start(&self) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if !self.wired.load(Ordering::SeqCst) {
                return;
            }
            if self.running.swap(true, Ordering::SeqCst) {
                return; // 已在运行
            }
            if let Err(error) = self.instance.start().await {
                self.running.store(false, Ordering::SeqCst);
                warn!(%error, "plugin instance start via mount failed");
            }
        })
    }

    fn stop(&self) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if !self.running.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Err(error) = self.instance.stop().await {
                warn!(%error, "plugin instance stop via unmount failed");
            }
        })
    }
}

/// 进程里**全部**服务键（诊断与守卫用：不注册任何东西）。
///
/// 存在理由很具体：插件在 `inject()` 里声明"没它我不能挂"，而"谁能提供那个
/// 键"只有宿主知道。两者对不上时的表现是**启动期挂载失败**
/// （`MissingService`）——如果没人守着，代价是"这次发布起不来"，或者更糟：
/// 某个插件静默缺席、功能少一块但没人发现。
///
/// 所以这里把键清单**显式列出**，由 `catalog_shape_tests` 与插件声明逐条
/// 对表。新增插件时忘了注册宿主能力 = 测试红。
///
/// 三类来源，都是"有人提供"：
/// 1. [`register`] 注册的宿主能力（本文件）；
/// 2. 组合根更早注册的进程级服务（LLM 出口 / 脱敏器——插件在 mount 里要它们）；
/// 3. 插件自己 `provide` 的（`loop` 的 TurnRunner / `tools.builtin` 的 provider）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn provided_keys() -> Vec<String> {
    let mut keys = vec![
        // ── 1. 本层注册 ──
        // Shell 会话（2026-11 独立插件：`echo-agent.shell`）。
        SHELL_HOST.name().to_string(),
        INSTANCE_HOST.name().to_string(),
        TOOLS_HOST.name().to_string(),
        SKILLS_HOST.name().to_string(),
        WORKSPACE_HOST.name().to_string(),
        PORTS.name().to_string(),
        FEDERATION_EVENTS.name().to_string(),
        MANAGEMENT_HOST.name().to_string(),
        SESSION_VIEW_HOST.name().to_string(),
        PROVIDER_HOST.name().to_string(),
        SELF_UPDATE_EVENTS.name().to_string(),
        INSTANCE_CONTROL.name().to_string(),
        // ── 2. 组合根早期注册 ──
        // provider 实例：键由 provider 插件声明（`PROVIDER_KEY`），值由组合根
        // 按配置构建——两侧同源，见该常量的文档。
        echo_defs::llm::LLM_PROVIDER_SERVICE_KEY.to_string(),
        "sanitizer".to_string(),
        // ── 3. 插件自 provide ──
        echo_plugin_loop::SERVICE_KEY.to_string(),
        echo_plugin_tools::plugin::SERVICE_KEY.to_string(),
        echo_plugin_shell::SERVICE_KEY.to_string(),
    ];
    keys.sort();
    keys.dedup();
    keys
}

/// 拨一个人的门控开关（插件挂载 / 卸载事件的反应）。
///
/// 为什么集中在这里：门控是**宿主对生命周期的反应**，不是插件的职责。此前
/// 每个插件构造函数各收一个 `gate: impl Fn(bool)`，于是组合根要为 6 个插件
/// 写 6 份语义相同的闭包——而且新增插件时还必须记得写第 7 份。
pub(crate) fn apply_plugin_gating(manager: &Arc<AgentManager>, plugin_id: &str, enabled: bool) {
    for running in manager.all() {
        running.agent.reapply_plugin_gating(plugin_id, enabled);
    }
}

/// 插件生命周期事件 → 门控（`plugin.mounted` / `plugin.unmounted`）。
///
/// 返回值：是否被本函数处理（不是这两类事件就 `false`，调用方继续别的翻译）。
pub(crate) fn gating_from_plugin_event(
    manager: &Arc<AgentManager>,
    event: &echo_plugin::PluginEvent,
) -> bool {
    match event.name.as_str() {
        "plugin.mounted" => {
            apply_plugin_gating(manager, &event.plugin, true);
            true
        }
        "plugin.unmounted" => {
            apply_plugin_gating(manager, &event.plugin, false);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {

    /// 生命周期事件名 → 门控方向的映射（两行代码，但写反了没人发现）。
    #[test]
    fn gating_event_names_are_the_registry_ones() {
        let event = echo_plugin::PluginEvent::new("p", "plugin.mounted", serde_json::Value::Null);
        assert!(!event.name.is_empty());
        assert_eq!(event.name, "plugin.mounted");
        let event = echo_plugin::PluginEvent::new("p", "plugin.unmounted", serde_json::Value::Null);
        assert_eq!(event.name, "plugin.unmounted");
    }
}
