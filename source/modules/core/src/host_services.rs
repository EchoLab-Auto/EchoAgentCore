//! 宿主能力的**装配面**：把组合根手里的运行期句柄注册成服务键。
//!
//! ## 为什么这一层存在（全模块化 S6）
//!
//! 改造前，组合根要为每个内置模块手工"组装参数"：
//!
//! ```ignore
//! let shell_host = AgentShellCommandHost::new(agent.clone());
//! let skills_host = AgentSkillsCommandHost::new(agent.clone());
//! push(catalog, agent_module_skills::module(version, skills_host, gate))?;
//! ```
//!
//! 于���"哪个模块需要宿主的什么"这件事，**住在组合根里**——新增一个模块要改
//! 三个地方（目录、参数组装、门控闭包），而模块自己反而不知道自己需要什么。
//!
//! 现在反过来：模块在 `mount` 里按**自己声明的键**解析（`MountContext::require`），
//! 组合根只做一件事——**把手里的句柄按同一批键注册**：
//!
//! ```text
//!   host_services::register(ctx, wiring)   ← 本文件：一件件往服务表里放
//!               ↓
//!   模块 mount: mctx.require(&KEY)          ← 模块：按需取，缺了就挂不上
//! ```
//!
//! 于是 `builtin_modules::catalog(version)` 变成**纯静态目录**，一个参数都不带。
//!
//! ## persona 门控为什么不在这一层
//!
//! 门控（"这个使用者看不看得到这个模块"）曾是一串 `gate` 闭包，每个模块一个。
//! 它是**宿主对挂载事件的反应**，不需要模块参与：宿主监听
//! `module.mounted` / `module.unmounted`（见 [`apply_module_gating`]），
//! 一处逻辑覆盖全部模块——包括将来新增的、组合根根本不认识的那些。

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent_module_federation::Federation;
use agent_module_management::host_keys::{ManagementHost, MANAGEMENT_HOST};
use agent_module_management::protocol::BackendBridge;
use agent_module_provider_llm::host_keys::PROVIDER_HOST;
use agent_module_self_update::host_keys::EVENTS as SELF_UPDATE_EVENTS;
use agent_module_transcript::SESSION_VIEW_HOST_KEY;
use echo_context::{Ctx, Disposer};
use echo_defs::module_instance::ModuleInstance;
use tracing::warn;

use agent_module_loop::agent::provider_host::AgentProviderCommandHost;
use agent_module_loop::agent::session_commands_host::AgentSessionCommandHost;
use agent_module_loop::agent::session_view_host::AgentSessionViewHost;
use agent_module_loop::agent::shell_host::{AgentInstanceCommandHost, AgentShellCommandHost};
use agent_module_loop::agent::skills_host::AgentSkillsCommandHost;
use agent_module_loop::agent::tools_host::AgentToolsCommandHost;
use agent_module_loop::agent_manager::AgentManager;
use agent_module_loop::federation::FederationPorts;
use agent_module_loop::{Agent, EventSink};

use agent_module_federation::host_keys::{EVENTS as FEDERATION_EVENTS, PORTS};
use agent_module_shell::host_keys::SHELL_HOST;
use agent_module_skills::host_keys::SKILLS_HOST;
use agent_module_tools::host_keys::{INSTANCE_HOST, TOOLS_HOST};

/// 组合根手里那些"配置表达不了、只有装配期知道"的句柄。
///
/// 每一项都来自组合根本身的所有物：端口 / 令牌 / 实例句柄 / 事件出口 /
/// 联邦端口集合。**模块不再看到这个结构体**——它是本文件的入参形式，不是
/// 模块契约。
pub(crate) struct HostWiring {
    /// 代表 agent（大部分命令域的 per-persona 接缝以它为锚点）。
    pub agent: Arc<Agent>,
    /// 进程级事件出口（联邦 / 自更新的回执走它）。
    pub event_sink: EventSink,
    /// 联邦出口集合（workspace 跨机查询 + federation 命令处理）。
    pub federation: Arc<FederationPorts>,
    /// 模块实例句柄（平台实例模块的启停）。
    pub instance: Option<Arc<dyn ModuleInstance>>,
    /// 实例是否已接线 / 是否在运行（启停幂等）。
    pub instance_wired: Arc<AtomicBool>,
    pub instance_running: Arc<AtomicBool>,
    /// Panel 通道桥。
    pub bridge: Arc<BackendBridge>,
    /// 管理面监听地址 / 令牌（`modules.toml` 可覆盖地址，令牌不行）。
    pub management_address: String,
    pub management_token: String,
    /// 合并模式下承接联邦入站的监听句柄（None = 独立模式）。
    pub merged_federation: Option<Arc<Federation>>,
    /// 联邦句柄（**不看合并与否**）：管理面用它记录入站连接的 `Host`，
    /// 邀请串据此宣告「别人实际连我用的地址」（2026-11；None = 联邦关闭）。
    pub federation_observer: Option<Arc<Federation>>,
    /// 外部模块贡献的技能来源（None = 进程里没有模块运行时）。
    ///
    /// 由组合根在模块启动**之后**才拿得到——而 `HostWiring` 正是在那之后
    /// 组装的，所以这里是 `Option` 而不是"先塞空实现后回填"。
    pub skill_contributions:
        Option<Arc<dyn agent_module_skills::host_keys::SkillContributionSource>>,
    /// 面板服务注册表（2026-11）。
    ///
    /// 由组合根创建并**同时**做两件事：把它 provide 进 `Ctx`（模块据此登记
    /// 自己的服务）、把它递进命令泵的 `ProcessCommandHost`（`InvokeService`
    /// 据此分发）。两处必须是同一个实例——分开造会得到"模块登记进 A、
    /// 命令泵去 B 里查"的空表，而那不是报错，是静默查不到。
    pub panel_services: Arc<crate::panel_services::PanelServiceTable>,
    /// 面板 UI 贡献注册表（2026-11）。
    ///
    /// 模块在 `mount` 里登记自己的前端 bundle（`PanelUiRegistrar`），面板启动
    /// 时经面板服务 `echo-agent.panel-ui.list` 拿到清单再 `import()`。归属跟着
    /// 挂载状态走：模块被禁用，贡献随 `Disposer` 撤销，面板上的入口自然消失。
    pub panel_ui: Arc<crate::panel_ui::PanelUiTable>,
}

/// 工作区模块的跨机查询接缝：把三个语义方法映射到联邦查询出口。
///
/// ## 为什么由组合根实现
///
/// 契约声明方是**消费方**（`agent-module-workspace`），但网络那一半（发
/// `FedFrame::Query`、吃 `allow_queries` 授权、按 peer 路由）属联邦子系统。
/// 组合根同时依赖两侧，是唯一能同时看见"语义"与"传输"的地方——所以它在
/// 这里，而不是在任何一个模块里。
///
/// ## 未接线
///
/// `querier` 为 `None`（联邦未装配）时每个方法都立即回可读错误；模块据此
/// 显示「不可采集」占位。**不是** panic，也不该静默返回空结果——空结果会被
/// 人误读成"对端目录是空的"。
struct HostRemoteWorkspaceQuery {
    querier: Option<agent_module_federation::RemoteQuerier>,
}

impl HostRemoteWorkspaceQuery {
    /// 发起一次联邦查询；未接线时立即返回可读错误。
    fn query(
        &self,
        peer: String,
        kind: agent_module_federation::QueryKind,
        subject: String,
    ) -> agent_module_workspace::QueryFuture {
        match &self.querier {
            Some(querier) => querier(peer, kind, subject),
            None => Box::pin(async {
                Err("联邦未接线：远程工作区查询不可用".to_string())
            }),
        }
    }
}

impl agent_module_workspace::RemoteWorkspaceQuery for HostRemoteWorkspaceQuery {
    fn git_status(&self, peer: String, path: String) -> agent_module_workspace::QueryFuture {
        self.query(
            peer,
            agent_module_federation::QueryKind::WorkspaceGitStatus,
            path,
        )
    }

    fn list_files(&self, peer: String, path: String) -> agent_module_workspace::QueryFuture {
        self.query(
            peer,
            agent_module_federation::QueryKind::WorkspaceFiles,
            path,
        )
    }

    fn browse(&self, peer: String, path: String) -> agent_module_workspace::QueryFuture {
        self.query(
            peer,
            agent_module_federation::QueryKind::BrowseDirectories,
            path,
        )
    }
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
            as Arc<dyn agent_module_loop::tool::builtin::ShellCommandHostAlias>,
    );
    put(
        &mut keep,
        ctx,
        &INSTANCE_HOST,
        Arc::new(AgentInstanceCommandHost::new(wiring.agent.clone()))
            as Arc<dyn agent_module_loop::tool::builtin::InstanceCommandHostAlias>,
    );
    put(
        &mut keep,
        ctx,
        &TOOLS_HOST,
        Arc::new(AgentToolsCommandHost::new(wiring.agent.clone()))
            as Arc<dyn agent_module_loop::tool::builtin::ToolsCommandHostAlias>,
    );
    // ── 通用提示词存储：skills 主动写入 ──
    put(
        &mut keep,
        ctx,
        &agent_module_skills::host_keys::PROMPT_STORE,
        wiring.agent.prompt_store(),
    );

    // ── 技能目录 ──
    put(
        &mut keep,
        ctx,
        &SKILLS_HOST,
        Arc::new(AgentSkillsCommandHost::new(wiring.agent.clone()))
            as Arc<dyn agent_module_loop::skill::commands::SkillsCommandHost>,
    );

    // ── 外部模块的技能贡献（qq / subagent / prodoc …）──
    //
    // 键挂在根作用域，人格作用域沿 `Ctx` 父链读同一份：技能表是 per-persona
    // 的（`PERSONA_SCOPED_KINDS` 把 `kind = "skill"` 的模块按人格各挂一份），
    // 而贡献是进程级的，只有这一层能同时看见两边。
    if let Some(source) = wiring.skill_contributions.clone() {
        put(
            &mut keep,
            ctx,
            &agent_module_skills::host_keys::SKILL_CONTRIBUTION_SOURCE,
            source,
        );
    }

    // ── 工作区 ──
    //
    // 此前这里注入 `WORKSPACE_HOST`（`AgentWorkspaceHost`：store 查表 + emit +
    // 浏览根 + 联邦查询），供进程内模块使用。工作区模块 2026-11 迁出为独立
    // 仓库后，它自己拥有 store、自己发事件，宿主这四条能力全部不再需要：
    //   - store → 模块持有（宿主只留只读镜像）
    //   - emit  → 模块 emit 模块事件，组合根翻译成 BackendEvent（见 main.rs）
    //   - 浏览根 → 模块自己遍历自己的 store
    //
    // 唯一还回来的能力是**跨机查询接缝**（2026-11）：`node://` 目录的 git /
    // 文件 / 浏览必须在**对端**机器上跑，网络那一半只能宿主做。契约
    // （`RemoteWorkspaceQuery`）声明在模块侧——它是消费方；这里只做
    // 「语义方法 → 联邦 `QueryKind`」的一行映射。
    put(
        &mut keep,
        ctx,
        &agent_module_workspace::REMOTE_QUERY,
        Arc::new(HostRemoteWorkspaceQuery {
            querier: wiring.federation.querier.clone(),
        }) as agent_module_workspace::RemoteQueryHandle,
    );
    // ── 联邦：出口集合 + 事件出口 ──
    put(&mut keep, ctx, &PORTS, wiring.federation.clone());
    let fed_sink = wiring.event_sink.clone();
    let fed_events: std::sync::Arc<dyn Fn(agent_module_federation::FederationEvent) + Send + Sync> =
        std::sync::Arc::new(move |ev| {
            if let Ok(json) = serde_json::to_string(&ev) {
                if let Ok(be) =
                    serde_json::from_str::<agent_module_management::protocol::BackendEvent>(&json)
                {
                    fed_sink(be);
                }
            }
        });
    put(&mut keep, ctx, &FEDERATION_EVENTS, fed_events);

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
            federation_observer: wiring.federation_observer.clone(),
        }) as Arc<dyn ManagementHost>,
    );
    put(
        &mut keep,
        ctx,
        &SESSION_VIEW_HOST_KEY,
        Arc::new(AgentSessionViewHost::new(wiring.agent.clone()))
            as Arc<dyn agent_module_transcript::session_view::SessionViewHost>,
    );

    // ── 会话写命令域（清理 / 归档 / 压缩）──
    put(
        &mut keep,
        ctx,
        &agent_module_session::SESSION_COMMAND_HOST_KEY,
        Arc::new(AgentSessionCommandHost::new(wiring.agent.clone()))
            as Arc<dyn agent_module_session::SessionCommandHost>,
    );

    // ── 供应商 / API 命令域 ──
    put(
        &mut keep,
        ctx,
        &PROVIDER_HOST,
        Arc::new(AgentProviderCommandHost::new(wiring.agent.clone()))
            as Arc<dyn agent_module_provider_llm::commands::ProviderCommandHost>,
    );

    // ── 自更新：只要事件出口 ──
    put(
        &mut keep,
        ctx,
        &SELF_UPDATE_EVENTS,
        wiring.event_sink.clone(),
    );

    // ── 平台实例（配置派生）：启停控制 ──
    if let Some(instance) = wiring.instance.clone() {
        put(
            &mut keep,
            ctx,
            &INSTANCE_CONTROL,
            Arc::new(ModuleInstanceControl {
                instance,
                wired: wiring.instance_wired.clone(),
                running: wiring.instance_running.clone(),
            }) as Arc<dyn InstanceControl>,
        );
    }

    // ── 面板服务注册表（2026-11）──
    //
    // **两个键、同一张表**：模块拿登记面（只能登记），命令泵拿分发面（只能
    // 分发）。拆成两个 trait 是为了让权限显式——给出去的能力不该同时包含
    // "遍历并调用别人的服务"。
    put(
        &mut keep,
        ctx,
        &echo_defs::panel_service::PANEL_SERVICE_REGISTRAR,
        wiring.panel_services.registrar(),
    );
    put(
        &mut keep,
        ctx,
        &echo_defs::panel_service::PANEL_SERVICE_INVOKER,
        wiring.panel_services.invoker(),
    );
    // ── 面板 UI 贡献注册表（2026-11）──
    //
    // 与上面两个键同一条纪律：必须在任何模块 mount 之前 provide，否则模块
    // `mount` 里 `service()` 返回 `None` —— 按设计那不算致命错误（嵌入式
    // 部署没有宿主），于是这类 bug 会静默。所以注册侧一律打 debug 日志。
    put(
        &mut keep,
        ctx,
        &echo_defs::panel_ui::PANEL_UI_REGISTRAR,
        wiring.panel_ui.registrar(),
    );
    // 清单服务（`echo-agent.panel-ui.list`）由**组合根**登记：它是注册表的
    // 投影，不属于任何模块。做在模块里会让"列出 UI 模块"依赖那个模块是否
    // 挂载——一个模块没装，面板就看不到别人也有 UI。
    match wiring.panel_services.registrar().register(Arc::new(
        crate::panel_ui::CorePanelUiService::new(wiring.panel_ui.clone()),
    )) {
        Ok(disposer) => keep.push(disposer),
        Err(error) => warn!(%error, "panel-ui list service registration failed"),
    }

    keep
}

/// 管理面的宿主实现（组合根的小适配器：四个字段，无逻辑）。
struct CoreManagementHost {
    address: String,
    token: String,
    bridge: Arc<BackendBridge>,
    federation: Option<Arc<Federation>>,
    /// 观测出处（独立监听时也需要，见 `HostWiring::federation_observer`）。
    federation_observer: Option<Arc<Federation>>,
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
    fn federation_observer(&self) -> Option<Arc<Federation>> {
        self.federation_observer.clone()
    }
}

/// 平台实例的启停控制（模块壳用它代替直接捕获句柄）。
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

struct ModuleInstanceControl {
    instance: Arc<dyn ModuleInstance>,
    wired: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
}

impl InstanceControl for ModuleInstanceControl {
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
                warn!(%error, "module instance start via mount failed");
            }
        })
    }

    fn stop(&self) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if !self.running.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Err(error) = self.instance.stop().await {
                warn!(%error, "module instance stop via unmount failed");
            }
        })
    }
}

/// 进程里**全部**服务键（诊断与守卫用：不注册任何东西）。
///
/// 存在理由很具体：模块在 `inject()` 里声明"没它我不能挂"，而"谁能提供那个
/// 键"只有宿主知道。两者对不上时的表现是**启动期挂载失败**
/// （`MissingService`）——如果没人守着，代价是"这次发布起不来"，或者更糟：
/// 某个模块静默缺席、功能少一块但没人发现。
///
/// 所以这里把键清单**显式列出**，由 `catalog_shape_tests` 与模块声明逐条
/// 对表。新增模块时忘了注册宿主能力 = 测试红。
///
/// 三类来源，都是"有人提供"：
/// 1. [`register`] 注册的宿主能力（本文件）；
/// 2. 组合根更早注册的进程级服务（LLM 出口 / 脱敏器——模块在 mount 里要它们）；
/// 3. 模块自己 `provide` 的（`loop` 的 TurnRunner / `tools.builtin` 的 provider）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn provided_keys() -> Vec<String> {
    let mut keys = vec![
        // ── 1. 本层注册 ──
        // Shell 会话（2026-11 独立模块：`echo-agent.shell`）。
        SHELL_HOST.name().to_string(),
        INSTANCE_HOST.name().to_string(),
        TOOLS_HOST.name().to_string(),
        SKILLS_HOST.name().to_string(),
        agent_module_skills::host_keys::PROMPT_STORE
            .name()
            .to_string(),
        PORTS.name().to_string(),
        FEDERATION_EVENTS.name().to_string(),
        MANAGEMENT_HOST.name().to_string(),
        agent_module_transcript::SESSION_VIEW_HOST_KEY
            .name()
            .to_string(),
        // 会话写命令域（清理 / 归档 / 压缩）：2026-11 S6 从宿主迁进会话模块。
        agent_module_session::SESSION_COMMAND_HOST_KEY
            .name()
            .to_string(),
        PROVIDER_HOST.name().to_string(),
        SELF_UPDATE_EVENTS.name().to_string(),
        INSTANCE_CONTROL.name().to_string(),
        // 面板服务注册表（2026-11）：两个键共享一张表。
        echo_defs::panel_service::PANEL_SERVICE_REGISTRAR
            .name()
            .to_string(),
        echo_defs::panel_service::PANEL_SERVICE_INVOKER
            .name()
            .to_string(),
        echo_defs::panel_ui::PANEL_UI_REGISTRAR.name().to_string(),
        // provider 实例：键由 provider 模块声明（`PROVIDER_KEY`），值由组合根
        // 按配置构建——两侧同源，见该常量的文档。
        echo_defs::llm::LLM_PROVIDER_SERVICE_KEY.to_string(),
        agent_module_sanitize::SERVICE_KEY.to_string(),
        // ── 3. 模块自 provide ──
        agent_module_loop::SERVICE_KEY.to_string(),
        agent_module_tools::module::SERVICE_KEY.to_string(),
        agent_module_shell::SERVICE_KEY.to_string(),
    ];
    keys.sort();
    keys.dedup();
    keys
}

/// 拨一个人的门控开关（模块挂载 / 卸载事件的反应）。
///
/// 为什么集中在这里：门控是**宿主对生命周期的反应**，不是模块的职责。此前
/// 每个模块构造函数各收一个 `gate: impl Fn(bool)`，于是组合根要为 6 个模块
/// 写 6 份语义相同的闭包——而且新增模块时还必须记得写第 7 份。
pub(crate) fn apply_module_gating(manager: &Arc<AgentManager>, module_id: &str, enabled: bool) {
    for running in manager.all() {
        running.agent.reapply_module_gating(module_id, enabled);
    }
}

/// 模块生命周期事件 → 门控（`module.mounted` / `module.unmounted`）。
///
/// 返回值：是否被本函数处理（不是这两类事件就 `false`，调用方继续别的翻译）。
pub(crate) fn gating_from_module_event(
    manager: &Arc<AgentManager>,
    event: &echo_module::ModuleEvent,
) -> bool {
    match event.name.as_str() {
        "module.mounted" => {
            apply_module_gating(manager, &event.module, true);
            true
        }
        "module.unmounted" => {
            apply_module_gating(manager, &event.module, false);
            true
        }
        _ => false,
    }
}

/// 工作区模块事件 → 宿主动作（返回 `true` = 本事件已被工作区桥消费，
/// 不再作为 `ModuleEvent` 透传给面板）。
///
/// 三类事件（协议见模块仓 `agent-module-workspace` 的模块文档）：
///
/// - `workspace.backend_event`：载荷是**要发给前端的 `BackendEvent`**（模块
///   自己拼好了线形状）——按 `team_id` 找到人格出口，事件原样进面板；
/// - `workspace.changed`：模块推来全量快照 → 写进该人格的镜像，再跑宿主的
///   那半段（确保会话通道 + 广播 `SessionUpdated` / `WorkspaceSessions`）；
/// - `workspace.conversation_deleted`：清空被删对话的通道历史。
///
/// 为什么在宿主翻译：这几件事都动宿主的会话子系统（通道 / provider / 事件
/// 出口），模块不认识它们，也不该认识。
pub(crate) fn workspace_from_module_event(
    manager: &Arc<AgentManager>,
    event: &echo_module::ModuleEvent,
) -> bool {
    match event.name.as_str() {
        agent_module_workspace::events::BACKEND_EVENT => {
            let team_id = event
                .payload
                .as_object()
                .and_then(|object| object.values().next())
                .and_then(|body| body.get("team_id"))
                .and_then(|value| value.as_str())
                .filter(|id| !id.is_empty())
                .map(str::to_string);
            match serde_json::from_value::<agent_module_management::protocol::BackendEvent>(
                event.payload.clone(),
            ) {
                Ok(backend_event) => {
                    // 冷启动恢复（C3c）：`WorkspaceSessions` 是模块从**权威 store**
                    // 读出的全量快照——启动拉取与面板重发都回这条命令，镜像空初始化
                    // 时用它补种（只补一次，已有来源不覆盖；细节在
                    // `Agent::adopt_workspace_sessions`）。事件本身照旧送面板，恢复
                    // 不改变线上形状。
                    if let Some(reply) = event.payload.get("WorkspaceSessions") {
                        if let Some(running) = team_id.as_deref().and_then(|id| manager.get(id)) {
                            running.agent.adopt_workspace_sessions(reply);
                        }
                    }
                    match team_id.and_then(|id| manager.get(&id)) {
                        // 有人格归属 → 走它的出口（team 标注由 `Agent::emit` 负责）。
                        Some(running) => running.agent.emit(backend_event),
                        // 无人格归属（浏览目录这类全局命令）→ 交给事件汇聚点。
                        None => tracing::debug!(
                            name = %event.name,
                            "workspace backend event without a resolvable persona; dropped"
                        ),
                    }
                }
                Err(error) => tracing::warn!(
                    %error,
                    "workspace.backend_event 载荷不是合法的 BackendEvent（模块与宿主线格式漂移？）"
                ),
            }
            true
        }
        agent_module_workspace::events::CHANGED => {
            let snapshot = match agent_module_loop::agent::workspace_mirror::WorkspaceSnapshot::try_from_event_payload(
                &event.payload,
            ) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(%error, "workspace.changed 载荷无效，保留现有镜像");
                    return true;
                }
            };
            let team_id = event
                .payload
                .get("team_id")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            match manager.get(team_id) {
                Some(running) => running.agent.apply_workspace_snapshot(snapshot),
                None => tracing::warn!(
                    %team_id,
                    "workspace.changed 指向未知人格（人格刚被禁/删？），快照丢弃"
                ),
            }
            true
        }
        agent_module_workspace::events::CONVERSATION_DELETED => {
            let team_id = event
                .payload
                .get("team_id")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let workspace_id = event
                .payload
                .get("workspace_id")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let Some(conversation) = event
                .payload
                .get("conversation")
                .cloned()
                .and_then(|value| {
                    serde_json::from_value::<
                            agent_module_workspace::wire::WorkspaceConversationInfo,
                        >(value)
                        .ok()
                })
            else {
                tracing::warn!("workspace.conversation_deleted 缺 conversation 载荷");
                return true;
            };
            if let Some(running) = manager.get(team_id) {
                running
                    .agent
                    .clear_workspace_conversation_channel(workspace_id, &conversation);
            }
            true
        }
        _ => false,
    }
}

/// **冷启动恢复（C3c）**：让每个能用工作区的人格向模块拉一次权威快照。
///
/// ## 为什么需要这一步
///
/// 宿主的镜像只消费 `workspace.changed`，而那是"变更时才有"的事件。进程重启
/// 后磁盘上仍有激活工作区/对话，但**没有任何变更发生**——镜像空着，于是项目
/// 对话通道不注册、`SessionUpdated` 不广播，面板的「本地当前对话」要等用户手动
/// 改一次工作区才出现。
///
/// ## 为什么复用 `RequestWorkspaceSessions`
///
/// 它是模块既有的**读**命令：从权威 store 取快照，以 `workspace.backend_event`
/// 回执（面板本来就在用它刷新列表）。因此恢复**不加命令、不加权限、不改线
/// 格式、不写 Workspace 文件、不引入跨机状态**：宿主只是把"面板会发的那条
/// 请求"在启动时替每个人格发一次，回执仍经同一条事件桥
/// （[`workspace_from_module_event`] → `Agent::adopt_workspace_sessions`）。
///
/// ## 边界（**不**声称解决的部分）
///
/// 回执与 `workspace.changed` 都**没有版本/序号**，宿主无法判定"迟到的回执"
/// 与"更新的变更事件"谁更晚。所以补种只做一次（见
/// `WorkspaceMirror::seed_if_unseeded`）：镜像已有来源时回执一律不覆盖。代价是
/// "变更事件在启动窗口里丢失 + 镜像先被一条旧回执种子化"这种组合仍然只能等
/// 下一次变更事件自愈——要严谨覆盖它需要给事件加版本（协议改动，不在本任务）。
///
/// 返回是否发出了至少一条请求（`false` = 工作区模块未挂载 / 没人人格允许它，
/// 都不是错误：那种部署本来就没有工作区能力）。
pub(crate) fn restore_workspace_sessions(
    manager: &Arc<AgentManager>,
    module_host: &Arc<agent_module_loop::modules::ModuleHost>,
) -> bool {
    // 只问"允许工作区"的人格：名单外的人格就算有历史文档也不该被恢复出通道
    // （门控是权限，恢复不能绕开它——这与 `workspace.changed` 的落点同一判据，
    // 只是这里由宿主主动发起，必须自己先看名单）。
    let targets: Vec<String> = manager
        .all()
        .into_iter()
        .filter(|running| {
            running
                .agent
                .persona_allows_module(agent_module_loop::modules::WORKSPACE_MODULE_ID)
        })
        .map(|running| running.id)
        .collect();
    if targets.is_empty() {
        return false;
    }
    let module_host = Arc::clone(module_host);
    // 事件桥是同步闭包，组合根这里是同步装配段——把 I/O 甩给运行时，启动不被
    // 一次模块往返拖住。回执异步到达，落点与面板请求完全相同。
    let handle = tokio::runtime::Handle::try_current();
    let Some(handle) = handle.ok() else {
        tracing::debug!("no tokio runtime; workspace session restore skipped");
        return false;
    };
    handle.spawn(async move {
        for team_id in targets {
            let payload = serde_json::json!({ "team_id": team_id });
            match module_host
                .dispatch_raw("RequestWorkspaceSessions", payload)
                .await
            {
                Some(Ok(_)) => tracing::debug!(team_id = %team_id, "workspace sessions restored"),
                Some(Err(message)) => tracing::warn!(
                    team_id = %team_id,
                    %message,
                    "workspace session restore failed (workspace 模块回了错误)"
                ),
                None => {
                    tracing::debug!("workspace 模块未挂载（未启用 / 未注册），跳过工作区会话恢复");
                    break;
                }
            }
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三个语义方法必须映射到**对的** `QueryKind` 且原样透传 `(peer, path)`。
    ///
    /// 这层只有三行映射，但错了的表现极隐蔽：对端会拿"列文件"的请求去跑
    /// git，回一个空清单，面板显示成"目录是空的"而不是报错。
    #[tokio::test]
    async fn remote_query_maps_semantic_methods_to_query_kinds() {
        use agent_module_workspace::RemoteWorkspaceQuery;

        let seen: Arc<std::sync::Mutex<Vec<(String, String, String)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let querier: agent_module_federation::RemoteQuerier =
            Arc::new(move |peer, kind, subject| {
                recorder
                    .lock()
                    .unwrap()
                    .push((peer, format!("{kind:?}"), subject));
                Box::pin(async { Ok(serde_json::json!({"ok": true})) })
            });
        let host = HostRemoteWorkspaceQuery {
            querier: Some(querier),
        };

        host.git_status("gpu-box".into(), "/srv/repo".into())
            .await
            .unwrap();
        host.list_files("gpu-box".into(), "/srv/repo".into())
            .await
            .unwrap();
        host.browse("gpu-box".into(), "/".into()).await.unwrap();

        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                (
                    "gpu-box".to_string(),
                    "WorkspaceGitStatus".to_string(),
                    "/srv/repo".to_string()
                ),
                (
                    "gpu-box".to_string(),
                    "WorkspaceFiles".to_string(),
                    "/srv/repo".to_string()
                ),
                (
                    "gpu-box".to_string(),
                    "BrowseDirectories".to_string(),
                    "/".to_string()
                ),
            ]
        );
    }

    /// 联邦未装配（`querier = None`）→ 三个方法都回可读错误，不 panic、
    /// 不静默返回空载荷（空载荷会被读成"对端目录是空的"）。
    #[tokio::test]
    async fn remote_query_without_federation_fails_readably() {
        use agent_module_workspace::RemoteWorkspaceQuery;
        let host = HostRemoteWorkspaceQuery { querier: None };
        assert!(host
            .git_status("gpu-box".into(), "/srv/repo".into())
            .await
            .unwrap_err()
            .contains("联邦未接线"));
        assert!(host
            .list_files("gpu-box".into(), "/srv/repo".into())
            .await
            .is_err());
        assert!(host.browse("gpu-box".into(), "/".into()).await.is_err());
    }
    /// 生命周期事件名 → 门控方向的映射（两行代码，但写反了没人发现）。
    #[test]
    fn gating_event_names_are_the_registry_ones() {
        let event = echo_module::ModuleEvent::new("p", "module.mounted", serde_json::Value::Null);
        assert!(!event.name.is_empty());
        assert_eq!(event.name, "module.mounted");
        let event = echo_module::ModuleEvent::new("p", "module.unmounted", serde_json::Value::Null);
        assert_eq!(event.name, "module.unmounted");
    }

    // ── 工作区冷启动恢复（C3c）──

    /// 最小 provider（恢复测试不跑模型，只要一个合法的 `LlmProvider`）。
    struct NullProvider;

    #[async_trait::async_trait]
    impl agent_module_loop::llm::LlmProvider for NullProvider {
        fn name(&self) -> &str {
            "null"
        }
        fn default_model(&self) -> &str {
            "null"
        }
        async fn chat(
            &self,
            _request: &agent_module_loop::llm::ChatRequest,
        ) -> Result<agent_module_loop::llm::ChatResponse, agent_module_loop::llm::LlmError>
        {
            Err(agent_module_loop::llm::LlmError::Config("not used".into()))
        }
        async fn chat_stream(
            &self,
            request: &agent_module_loop::llm::ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<agent_module_loop::llm::ChatChunk>,
        ) -> Result<agent_module_loop::llm::ChatResponse, agent_module_loop::llm::LlmError>
        {
            self.chat(request).await
        }
    }

    /// 造一个只有一个人格 `t` 的管理器，并把它的镜像交出来。
    fn manager_with_mirror() -> (
        Arc<AgentManager>,
        Arc<agent_module_loop::agent::workspace_mirror::WorkspaceMirror>,
    ) {
        use agent_module_loop::config::{AgentConfig, TeamMember};
        let mirror = Arc::new(agent_module_loop::agent::workspace_mirror::WorkspaceMirror::new());
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "t".into(),
            TeamMember {
                name: "T".into(),
                enabled: true,
                ..Default::default()
            },
        );
        let shared = Arc::clone(&mirror);
        let manager = Arc::new(AgentManager::build(&raw, move |id, _profile| {
            let agent = Arc::new(agent_module_loop::Agent::new(
                Arc::new(NullProvider),
                AgentConfig::default(),
                agent_module_loop::skill::SkillRegistry::new(),
                agent_module_loop::tool::ToolRegistry::new(),
                Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
            ));
            agent.set_team_id(Some(id));
            agent.set_workspace_mirror(Arc::clone(&shared));
            agent
        }));
        (manager, mirror)
    }

    /// 组合根那一跳：`workspace.backend_event` 里的 `WorkspaceSessions` 回执要
    /// 喂进**该人格**的镜像（这是重启后"已存激活对话自己出现"的入口）。
    ///
    /// 守两个方向：合法回执补种；他人格的回执不能落到 `t` 的镜像里。
    #[tokio::test]
    async fn workspace_sessions_reply_seeds_persona_mirror() {
        let (manager, mirror) = manager_with_mirror();
        assert!(mirror.is_empty() && !mirror.is_seeded());

        // ① 别人格的回执：路由到不存在的 id → 丢弃，`t` 的镜像保持空。
        let foreign = echo_module::ModuleEvent::new(
            agent_module_workspace::MODULE_ID,
            agent_module_workspace::events::BACKEND_EVENT,
            serde_json::json!({
                "WorkspaceSessions": {
                    "team_id": "other",
                    "sessions": [{ "id": "evil", "name": "Evil" }],
                    "active": "evil",
                }
            }),
        );
        assert!(workspace_from_module_event(&manager, &foreign));
        assert!(!mirror.is_seeded(), "他人格的回执不碰本人格的镜像");

        // ② 本人格的回执：补种 + 跑宿主的后半段（通道注册是异步 spawn 的）。
        let own = echo_module::ModuleEvent::new(
            agent_module_workspace::MODULE_ID,
            agent_module_workspace::events::BACKEND_EVENT,
            serde_json::json!({
                "WorkspaceSessions": {
                    "team_id": "t",
                    "sessions": [{ "id": "proj", "name": "Proj", "directories": ["/srv/proj"] }],
                    "active": "proj",
                    "active_conversation": null,
                }
            }),
        );
        assert!(workspace_from_module_event(&manager, &own));
        assert!(mirror.is_seeded());
        assert_eq!(mirror.snapshot().active.as_deref(), Some("proj"));
        // 通道注册由 spawn 完成，等一拍。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let running = manager.get("t").expect("persona t");
        assert!(
            running
                .agent
                .trunk
                .all()
                .iter()
                .any(|s| s.id == "local:workspace:proj:local_user"),
            "恢复后通道会话已注册"
        );

        // ③ 坏载荷：不覆盖。
        let broken = echo_module::ModuleEvent::new(
            agent_module_workspace::MODULE_ID,
            agent_module_workspace::events::BACKEND_EVENT,
            serde_json::json!({
                "WorkspaceSessions": { "team_id": "t", "sessions": "not-a-list" }
            }),
        );
        assert!(workspace_from_module_event(&manager, &broken));
        assert_eq!(
            mirror.snapshot().active.as_deref(),
            Some("proj"),
            "坏载荷不覆盖合法状态"
        );
    }
    #[tokio::test]
    async fn workspace_invalid_change_does_not_claim_or_replace_the_mirror() {
        let (manager, mirror) = manager_with_mirror();
        let change = |payload| {
            workspace_from_module_event(
                &manager,
                &echo_module::ModuleEvent::new(
                    agent_module_workspace::MODULE_ID,
                    agent_module_workspace::events::CHANGED,
                    payload,
                ),
            )
        };
        let malformed = serde_json::json!({"team_id": "t"});
        assert!(change(malformed.clone()));
        assert!(!mirror.is_seeded());
        assert!(change(serde_json::json!({
            "team_id": "t", "sessions": [{"id":"proj", "name":"Proj"}], "active":"proj"
        })));
        assert!(change(malformed));
        assert_eq!(mirror.snapshot().active.as_deref(), Some("proj"));
        assert!(change(serde_json::json!({"team_id": "t", "sessions": []})));
        assert!(mirror.is_empty());
    }

    /// 门控不因"恢复"被绕开：白名单里没有工作区的人格**不发起**拉取。
    ///
    /// 反向用例（默认无名单 → 允许）在同一测试里先跑一次：否则"永远返回 false"
    /// 的实现也能骗过这条断言。
    #[tokio::test]
    async fn restore_workspace_sessions_skips_gated_personas() {
        let (manager, _mirror) = manager_with_mirror();
        let host = Arc::new(agent_module_loop::modules::ModuleHost::new_without_skill_prompt());
        assert!(
            restore_workspace_sessions(&manager, &host),
            "无名单（= 允许）→ 发起拉取"
        );

        // 收紧到"只允许工具模块"：工作区在名单外。
        let running = manager.get("t").expect("persona t");
        running
            .agent
            .apply_capabilities(&agent_module_loop::TeamMember {
                name: "T".into(),
                enabled: true,
                enabled_modules: vec!["echo-agent.tools.builtin".into()],
                ..Default::default()
            })
            .await;
        assert!(
            !restore_workspace_sessions(&manager, &host),
            "名单外的人格不拉取（恢复不扩大权限）"
        );
    }

    /// **端到端**：`restore_workspace_sessions` 真的把模块里的已存激活会话拉回
    /// 宿主的镜像（真实 workspace 模块 + 真实事件桥，只有人格是测试造的）。
    ///
    /// 这正是重启现场：文档在装配**之前**就落盘，镜像初始化时是空的，只有宿主
    /// 主动拉一次，面板才能看到「本地当前对话」。
    #[tokio::test]
    async fn restore_workspace_sessions_pulls_the_persisted_active() {
        let dir = std::env::temp_dir().join(format!(
            "echo-core-workspace-restore-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("state dir");
        // 上一次运行留下的文档（装配前落盘 = 重启时磁盘上就有的那一份）。
        {
            let store = agent_module_workspace::store::WorkspaceStore::load(Some(
                dir.join("echo-workspaces-t.json"),
            ));
            store
                .upsert(agent_module_workspace::wire::WorkspaceSessionInfo {
                    id: "proj".into(),
                    name: "Proj".into(),
                    directories: vec!["/srv/proj".into()],
                    ..Default::default()
                })
                .expect("persist workspace");
            store
                .set_active(Some("proj".into()), None)
                .expect("persist active");
        }

        let (manager, mirror) = manager_with_mirror();
        assert!(mirror.is_empty() && !mirror.is_seeded(), "重启：镜像空着");

        let host = Arc::new(agent_module_loop::modules::ModuleHost::new_without_skill_prompt());
        // 配置与事件出口**必须**在 mount 之前（mount 取的是当时的值）。
        host.set_configs([(
            agent_module_workspace::MODULE_ID,
            serde_json::json!({ "state_dir": dir.to_string_lossy() }),
        )]);
        let sink_manager = Arc::clone(&manager);
        host.set_event_sink(Arc::new(move |event: echo_module::ModuleEvent| {
            workspace_from_module_event(&sink_manager, &event);
        }));
        host.register_and_mount(agent_module_workspace::module("0.0.0"))
            .expect("mount workspace module");

        assert!(
            restore_workspace_sessions(&manager, &host),
            "有人格允许工作区 → 发出了拉取"
        );
        // 回执与补种是异步的（事件桥同步派发，通道注册再 spawn 一拍）。
        for _ in 0..50 {
            if mirror.is_seeded() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(mirror.is_seeded(), "回执补种了镜像");
        assert_eq!(mirror.snapshot().active.as_deref(), Some("proj"));
        assert_eq!(mirror.snapshot().sessions.len(), 1);
        let running = manager.get("t").expect("persona t");
        assert!(
            running
                .agent
                .trunk
                .all()
                .iter()
                .any(|s| s.id == "local:workspace:proj:local_user"),
            "已存激活对话的通道被恢复注册"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
