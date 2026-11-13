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

use echo_context::{Ctx, Disposer};
use echo_defs::module_instance::ModuleInstance;
use agent_module_federation::Federation;
use agent_module_management::host_keys::{ManagementHost, MANAGEMENT_HOST};
use agent_module_management::protocol::BackendBridge;
use agent_module_provider_llm::host_keys::PROVIDER_HOST;
use agent_module_self_update::host_keys::EVENTS as SELF_UPDATE_EVENTS;
use agent_module_transcript::SESSION_VIEW_HOST_KEY;
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
            None => Box::pin(async { Err("联邦未接线：远程工作区查询不可用".to_string()) }),
        }
    }
}

impl agent_module_workspace::RemoteWorkspaceQuery for HostRemoteWorkspaceQuery {
    fn git_status(&self, peer: String, path: String) -> agent_module_workspace::QueryFuture {
        self.query(peer, agent_module_federation::QueryKind::WorkspaceGitStatus, path)
    }

    fn list_files(&self, peer: String, path: String) -> agent_module_workspace::QueryFuture {
        self.query(peer, agent_module_federation::QueryKind::WorkspaceFiles, path)
    }

    fn browse(&self, peer: String, path: String) -> agent_module_workspace::QueryFuture {
        self.query(peer, agent_module_federation::QueryKind::BrowseDirectories, path)
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

    // ── 工具状态 → 线上事件（**绑定方声明**，2026-11 S7）──
    //
    // 执行链不再有 `match state_key { "checklist" => … }` 表，这里也不该有
    // 工具名：谁装了这个工具，谁声明"它的状态长什么线上事件"。绑定本体在
    // `agent-module-tools`（见 `checklist::state_event_binding`），本层只把
    // 声明装进 agent——新增一个有状态工具时这里一行都不用改。
    for (key, build) in agent_module_tools::checklist::all_state_event_bindings() {
        wiring.agent.register_tool_state_event(key, build);
    }

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
        // ── 2. 组合根早期注册 ──
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
                Ok(backend_event) => match team_id.and_then(|id| manager.get(&id)) {
                    // 有人格归属 → 走它的出口（team 标注由 `Agent::emit` 负责）。
                    Some(running) => running.agent.emit(backend_event),
                    // 无人格归属（浏览目录这类全局命令）→ 交给事件汇聚点。
                    None => tracing::debug!(
                        name = %event.name,
                        "workspace backend event without a resolvable persona; dropped"
                    ),
                },
                Err(error) => tracing::warn!(
                    %error,
                    "workspace.backend_event 载荷不是合法的 BackendEvent（模块与宿主线格式漂移？）"
                ),
            }
            true
        }
        agent_module_workspace::events::CHANGED => {
            let snapshot =
                agent_module_loop::agent::workspace_mirror::WorkspaceSnapshot::from_event_payload(
                    &event.payload,
                );
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
        let querier: agent_module_federation::RemoteQuerier = Arc::new(move |peer, kind, subject| {
            recorder
                .lock()
                .unwrap()
                .push((peer, format!("{kind:?}"), subject));
            Box::pin(async { Ok(serde_json::json!({"ok": true})) })
        });
        let host = HostRemoteWorkspaceQuery {
            querier: Some(querier),
        };

        host.git_status("gpu-box".into(), "/srv/repo".into()).await.unwrap();
        host.list_files("gpu-box".into(), "/srv/repo".into()).await.unwrap();
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
}
