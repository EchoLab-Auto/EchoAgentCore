//! 内置插件的**目录**（唯一归属）。
//!
//! ## 这个文件回答什么问题
//!
//! 「这个二进制里有哪些内置插件、各自的 `mount` 做什么」。此前这 378 行散在
//! `main.rs` 的八个「实化 N」段里，而 `plugins.toml` 决定的是**装哪些、什么
//! 顺序**——两者放在一起看，很难回答"新增一个内置插件要改哪里"。
//!
//! ## 与配置的关系（这是 dsh 的形态）
//!
//! ```text
//!   builtin_plugins::catalog()   ← 本文件：代码里的插件目录（构造 + mount）
//!              ↓
//!   plugins.toml 的 builtin 行   ← 配置：顺序、启用状态、每插件的 config
//!              ↓
//!   PluginHost::mount_plan_when_ready  ← 挂载：等依赖就绪，失败即启动失败
//! ```
//!
//! 构造留在代码里是**刻意的**：`mount` 闭包必须捕获运行期句柄（端口 / 令牌 /
//! 实例句柄 / broker），而那些东西在配置里表达不了。配置能表达的是"装不装、
//! 哪个先装、给它的那份 config 长什么样"——三者都在 `plugins.toml`。
//!
//! ## 迁移方向（全插件化 S3 的剩余工作）
//!
//! 本文件里的条目大多是**门控壳**：`mount` 只做 `reapply_plugin_gating`，
//! 因为真实实现已经在对应插件 crate 里（`echo-plugin-tools` / `-skills` /
//! `-subagent` / `-workspace` / `-federation` / `-management`）。壳之所以还在
//! 这里，是因为**构造参数是宿主的东西**（哪个人格、哪个端口、哪个事件出口）。
//! 要彻底消除它们，需要「插件自己声明构造参数」的能力（框架的 `factory`
//! 注册表）——那是下一步，不是这一轮。

use std::sync::Arc;

use echo_agent::agent_manager::AgentManager;
use echo_agent::plugins::{
    MANAGEMENT_PANEL_PLUGIN_ID, SKILLS_DIR_PLUGIN_ID, TOOLS_BUILTIN_PLUGIN_ID,
};
use echo_context::Disposer;
use echo_defs::plugin_instance::PluginInstance;
use echo_plugin::{BuiltinPlugin, MountContext, Plugin, PluginManifest, PluginRuntime};
use std::sync::atomic::Ordering;
use tracing::warn;

/// 构造内置插件时**宿主必须提供**的运行期句柄。
///
/// 每一项都是"配置表达不了、只有组合根知道"的东西。刻意用结构体而不是
/// 一长串参数：参数超过四个就没人读得懂调用点。
pub(crate) struct Hosts {
    /// persona 管理器（插件门控要遍历全部人格）。
    pub manager: Arc<AgentManager>,
    /// 管理面 agent（工具集 / 技能表是 per-persona 的，拿它当代表）。
    pub agent: Arc<echo_agent::Agent>,
    /// 进程级事件出口（workspace / federation 插件要它）。
    pub event_sink: echo_agent::EventSink,
    /// 联邦出口集合（workspace 的跨机查询 + federation 的命令处理）。
    pub federation: Arc<echo_agent::federation::FederationPorts>,
    /// 插件实例句柄（QQ 实例插件的启停）。
    pub instance: Option<Arc<dyn PluginInstance>>,
    /// 实例是否已接线 / 是否在运行（启停幂等）。
    pub instance_wired: Arc<std::sync::atomic::AtomicBool>,
    pub instance_running: Arc<std::sync::atomic::AtomicBool>,
    /// 平台名（实例插件 id 由它派生——组合根不持有点名平台的常量）。
    ///
    /// `None` = 配置里**没有任何** `[instances.<平台>]` 段。这不是错误（不用
    /// 平台的部署很正常），但**必须**跳过实例插件条目：否则目录里会出现一个
    /// id 为 `echo-agent.instance.`（尾随点、平台名为空）的幽灵条目，面板
    /// 插件清单里会多出一个点不动的「插件实例」。
    pub platform: Option<String>,
    /// 管理面 WS 桥（Panel 通道）。
    pub bridge: Arc<echo_agent::BackendBridge>,
    /// 管理面监听地址 / 令牌（`plugins.toml` 可覆盖 address，token 不行）。
    pub management_address: String,
    pub management_token: String,
    /// 合并模式下承接联邦入站的监听句柄（None = 独立模式）。
    pub merged_federation: Option<Arc<echo_federation::Federation>>,
    /// 外部插件条目（`plugins.toml` 的非 builtin 行）——注册成清单里的
    /// 门控壳，实现由 supervisor 拉起。
    pub external_entries: Vec<ExternalEntry>,
}

/// 一个外部插件条目（`plugins.toml` 的 `kind != "builtin"` 行）。
pub(crate) struct ExternalEntry {
    pub id: String,
    pub name: String,
    pub kind: String,
}

/// 遍历全部 persona（插件门控用）。
fn for_each_agent(mgr: &Arc<AgentManager>, f: impl Fn(&Arc<echo_agent::Agent>)) {
    for running in mgr.all() {
        f(&running.agent);
    }
}

/// 插件 id 的**形状**校验（装配 bug 早失败胜过面板里出现怪条目）。
///
/// 真实踩到过的一个：没配置 `[instances.<平台>]` 时，实例插件的 id 会被派生成
/// `echo-agent.instance.`（尾随点、平台名为空）——它在 `plugins mounted` 日志
/// 与面板清单里都是一个点不动的幽灵插件，而当时没有任何断言拦得住。
fn validate_plugin_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() {
        return Err(anyhow::anyhow!("builtin plugin id must not be empty"));
    }
    let last = id.rsplit('.').next().unwrap_or("");
    if last.is_empty() {
        // 派生 id 的空段几乎一定是"平台名/名字没取到"的装配 bug。
        return Err(anyhow::anyhow!(
            "builtin plugin id {id:?} ends with an empty segment (derived name missing?)"
        ));
    }
    Ok(())
}

/// 压入一个只有 manifest + mount 闭包的插件。
fn push_builtin(
    catalog: &mut Vec<Arc<dyn Plugin>>,
    manifest: PluginManifest,
    mount: impl Fn(&MountContext) -> echo_plugin::PluginMountResult + Send + Sync + 'static,
) -> anyhow::Result<()> {
    let id = manifest.id.clone();
    validate_plugin_id(&id)?;
    if catalog.iter().any(|p| p.manifest().id == id) {
        // 装配 bug（同 id 两条构造）——早失败胜过静默覆盖。
        return Err(anyhow::anyhow!("duplicate builtin plugin id {id}"));
    }
    catalog.push(Arc::new(BuiltinPlugin::new(manifest, mount)));
    Ok(())
}

/// 压入一个**已构造好**的插件（带贡献：命令 / 文本块 / 事件…）。
///
/// 与 [`push_builtin`] 的分工：后者只给 manifest + mount 闭包，构造器的额外
/// 能力（`with_commands` / `with_text_blocks` / …）用不上。
fn push_plugin(catalog: &mut Vec<Arc<dyn Plugin>>, plugin: Arc<dyn Plugin>) -> anyhow::Result<()> {
    let id = plugin.manifest().id.clone();
    validate_plugin_id(&id)?;
    if catalog.iter().any(|p| p.manifest().id == id) {
        return Err(anyhow::anyhow!("duplicate builtin plugin id {id}"));
    }
    catalog.push(plugin);
    Ok(())
}

/// 构造**全部内置插件**（顺序即目录顺序；实际挂载顺序由 `plugins.toml` 定）。
pub(crate) fn catalog(
    version: &str,
    hosts: &Hosts,
) -> anyhow::Result<Vec<Arc<dyn Plugin>>> {
    let Hosts {
        manager: gating_manager,
        agent,
        event_sink,
        federation: federation_ports,
        instance: qq_instance,
        instance_wired: qq_wired,
        instance_running: qq_running,
        platform,
        bridge,
        management_address,
        management_token,
        merged_federation: merge_fed,
        external_entries,
    } = hosts;
    let mgmt_fed = merge_fed.clone();
    let mut catalog: Vec<Arc<dyn Plugin>> = Vec::new();
    // 名义挂载：provider.llm 保持重启生效语义。
    let nominal = |entry: String| {
        move |_ctx: &MountContext| {
            let _ = entry;
            Ok(vec![])
        }
    };

// ── 实化 1：内置工具集（包维度批量启停，跨 persona）──
//
// 清单与装配都在插件里（`echo-plugin-tools`，全插件化 S1）：这里只给
// 一个门控回调 + 挂载。`mount` 会把 `BuiltinToolProvider` provide 到
// `tools.builtin` 服务键——之后创建的 persona 从服务表解析它，
// 而不是由组合根指名。
{
    let gating_manager_1 = gating_manager.clone();
    // Shell 命令域的宿主接缝：按 `team_id` 解析人格（当前 agent 兜底）。
    // 插件因此不认识 `Agent`——它只要"管理器 / 出口 / 归属 / 进度回调"。
    let shell_command_host: std::sync::Arc<
        dyn echo_agent::tool::builtin::ShellCommandHostAlias,
    > = std::sync::Arc::new(echo_agent::agent::shell_host::AgentShellCommandHost::new(
        agent.clone(),
    ));
    // 工具管理面：注册表 per-persona、启停要持久化 + 逐人格重算。
    let tools_command_host: std::sync::Arc<
        dyn echo_agent::tool::builtin::ToolsCommandHostAlias,
    > = std::sync::Arc::new(echo_agent::agent::tools_host::AgentToolsCommandHost::new(
        agent.clone(),
    ));
    // 实例命令域：注册表是进程级单例，出口就是 `Agent::emit`。
    let instance_command_host: std::sync::Arc<
        dyn echo_agent::tool::builtin::InstanceCommandHostAlias,
    > = std::sync::Arc::new(echo_agent::agent::shell_host::AgentInstanceCommandHost::new(
        agent.clone(),
    ));
    push_plugin(
        &mut catalog,
        echo_agent::tool::builtin::builtin_plugin::plugin(
            version,
            shell_command_host,
            instance_command_host,
            tools_command_host,
            move |enable| {
                // 逐 persona 重评估：全局启用 ∧ 各 persona 白/黑名单。
                for_each_agent(&gating_manager_1, |a| {
                    a.reapply_plugin_gating(TOOLS_BUILTIN_PLUGIN_ID, enable)
                });
            },
        ),
    )?;
}

// ── 实化 2：技能目录（整表启停 + 命令域，跨 persona）──
//
// 清单与命令域都在插件里（全插件化 S3）：宿主只注入 `SkillsCommandHost`
// （per-persona 技能表 / 写目录 / 重载 / 启停持久化 / 事件出口）。
{
    let gating_manager_2 = gating_manager.clone();
    let skills_host: std::sync::Arc<
        dyn echo_agent::skill::commands::SkillsCommandHost,
    > = std::sync::Arc::new(echo_agent::agent::skills_host::AgentSkillsCommandHost::new(
        agent.clone(),
    ));
    push_plugin(
        &mut catalog,
        echo_agent::skill::plugin(version, skills_host, move |enable| {
            for_each_agent(&gating_manager_2, |a| {
                a.reapply_plugin_gating(SKILLS_DIR_PLUGIN_ID, enable)
            });
        }),
    )?;
}

// ── 实化 4：循环模式（二选一）──
// 声明已外迁到 `echo-plugin-loop`（Phase 3 ⑦）：驱动本体只有一个
// （echo-loop TurnRunner，2026-10 单轨化），两个模式插件只表达**策略**
// （per-persona 白名单推导的 `LoopMode`，见 `TeamMember::loop_mode`）；
// mount/unmount 不切换驱动、不注册任何运行期资源。
//
// 协议化：插件族声明 `Service{key:"loop"}` 贡献；真正的 runner 实例
// 在本文件更早处（`ctx.register::<Arc<TurnRunner>>("loop", …)`）构造，
// 带计量/脱敏装饰，服务键与贡献声明同源。
for plugin in echo_plugin_loop::plugins(version) {
    let id = plugin.manifest().id.clone();
    if catalog.iter().any(|p| p.manifest().id == id) {
        return Err(anyhow::anyhow!("duplicate builtin plugin id {id}"));
    }
    catalog.push(plugin);
}
tracing::debug!(
    service = echo_plugin_loop::service_contribution().key.as_str(),
    plugins = ?echo_plugin_loop::PLUGIN_IDS,
    "loop mode plugins queued (pure policy; service declared)"
);

// 任务清单（checklist）已降级为普通内置工具：无插件注册，
// 与 calculator / bash 等同层，只受工具级白/黑名单门控。

// ── 实化 6：工作区会话（workspace 插件）──
//
// **整个命令域 + 提示词块都在插件里**（本轮迁移完成）。宿主这边只剩
// **一处**接线：注入 `WorkspaceHost`（四个方法，见
// `agent/workspace_host.rs`）。于是：
// - 7 条 `*Workspace*` 命令由 `dispatch_command` 路由到插件，
//   `commands.rs` 里不再有 workspace 的 match 分支（只剩"插件没装"兜底）；
// - 提示词块 `workspace` 由插件的 `Contribution::Text` 提供，
//   装配函数不再有 workspace 位置参数；
// - persona 门控（白名单）由宿主的 gate 回调表达——那是宿主的配置概念。
{
    use echo_agent::plugins::WORKSPACE_PLUGIN_ID;
    let gating_manager_3 = gating_manager.clone();
    // 宿主能力：store 解析 / 事件出口 / 浏览根 / 联邦查询。
    //
    // store 走**进程级共享表**（`team_id → store`）：进程级只有一个
    // workspace 插件，而 store 是 per-persona 的。提示词块与命令处理
    // 都要按 `team_id` 取自己那一份。
    let workspace_host: std::sync::Arc<dyn echo_agent::workspace::commands::WorkspaceHost> =
        std::sync::Arc::new(
            echo_agent::agent::workspace_host::AgentWorkspaceHost::from_agents(
                {
                    let manager = Arc::clone(gating_manager);
                    move || {
                        manager
                            .all()
                            .into_iter()
                            .map(|running| Arc::clone(&running.agent))
                            .collect()
                    }
                },
                event_sink.clone(),
                // 联邦查询出口：直接用组合根持有的那份（`federation_ports`
                // 在装配期构造、与插件同寿命），不必借 `factory_federation`
                // ——后者已经被 agent factory 闭包移走了。
                Some(echo_agent::federation::workspace_query(
                    federation_ports.querier.clone(),
                )),
            ),
        );
    let plugin = echo_agent::workspace::plugin(version, workspace_host, move |enable| {
        for_each_agent(&gating_manager_3, |a| {
            a.reapply_plugin_gating(WORKSPACE_PLUGIN_ID, enable)
        });
    });
    push_plugin(&mut catalog, plugin)?;
}

// ── 实化 6b：联邦（对等链路：远程工具 / 跨机委派 / 会话迁移）──
//
// 实现（路由表 / 代理工具 / 聚合器 / 跨机查询 / 出口集合，1,698 行）
// 已整块外迁到 `echo-plugin-federation`（全插件化 S3）。宿主这边只剩：
// 一份 `FederationPorts`（装配期构造，含组合根实现的命令处理器）+
// 进程级事件出口。于是"联邦"可独立禁用、可独立替换。
{
    use echo_agent::plugins::FEDERATION_PLUGIN_ID;
    let gating_manager_f = gating_manager.clone();
    let plugin = echo_plugin_federation::plugin(
        version,
        std::sync::Arc::clone(&federation_ports),
        event_sink.clone(),
        move |enable| {
            for_each_agent(&gating_manager_f, |a| {
                a.reapply_plugin_gating(FEDERATION_PLUGIN_ID, enable)
            });
        },
    );
    push_plugin(&mut catalog, plugin)?;
}

// 选单（present_menu）已于 2026-09 废弃移除（无插件、无 broker）。

// ── 实化 7：subagent 委派（隔离上下文子任务 + 完成 hook 回灌）──
{
    use echo_agent::plugins::SUBAGENT_PLUGIN_ID;
    let gating_manager_4 = gating_manager.clone();
    push_builtin(
        &mut catalog,
        PluginManifest::builtin(
            SUBAGENT_PLUGIN_ID,
            "Subagent 委派",
            version,
            "tool",
            "subagent",
            "委派独立子任务给隔离上下文的子 agent：spawn_subagent 工具 + 完成后 <subagent_event> hook 回灌主 agent",
        ),
        move |_ctx| {
                for_each_agent(&gating_manager_4, |a| a.reapply_plugin_gating(SUBAGENT_PLUGIN_ID, true));
            let gating_manager_u4 = gating_manager_4.clone();
            Ok(vec![Disposer::from_fn(move || {
                for_each_agent(&gating_manager_u4, |a| a.reapply_plugin_gating(SUBAGENT_PLUGIN_ID, false));
            })])
        },
    )?;
}

// ── 实化 3：QQ 插件实例（启停进程 + 工具包启停）──
//
// **只在配置了平台时存在**：平台名是 id 的派生来源（`echo-agent.instance.<平台>`），
// 没有平台就没有这个插件——而不是造一个 id 为空后缀的幽灵条目。
if let Some(platform) = platform.clone() {
    // 实例句柄（独立进程实例插件；无实例 = None，启停为 no-op）。
    let qq = qq_instance.clone();
    let wired = qq_wired.clone();
    let running = qq_running.clone();
    // id / 名称 / entry 全部由**平台名**派生——组合根不再持有点名平台的常量。
    let instance_plugin_id = crate::instance_manifests::plugin_id_for(&platform);
    let instance_plugin_name = format!("{platform} 插件实例");
    let instance_plugin_id_for_mount = instance_plugin_id.clone();
    let gating_manager_5 = gating_manager.clone();
    push_builtin(
        &mut catalog,
        PluginManifest::builtin(
            &instance_plugin_id,
            &instance_plugin_name,
            version,
            "instance",
            platform.clone(),
            format!("{platform} 平台实例插件（独立进程）"),
        )
        // 实例插件化收尾（P1.9，2026-10）：实现由独立进程
        // `echo-instance-qq` 提供，这里只是门控壳。必须显式声明，
        // 否则 Panel 按 builtin=true 把它显示成「内部插件」。
        .with_runtime(PluginRuntime::Subprocess),
        move |_ctx| {
            let plugin_id = instance_plugin_id_for_mount.clone();
            for_each_agent(&gating_manager_5, |a| {
                a.reapply_plugin_gating(&plugin_id, true)
            });
            if let Some(qq) = qq.clone() {
                if wired.load(Ordering::SeqCst) && !running.swap(true, Ordering::SeqCst) {
                    let running2 = running.clone();
                    tokio::spawn(async move {
                        if let Err(e) = qq.start().await {
                            running2.store(false, Ordering::SeqCst);
                            warn!(error = %e, "QQ adapter start via plugin mount failed");
                        }
                    });
                }
            }
            let qq = qq.clone();
            let running = running.clone();
            let plugin_id_unmount = instance_plugin_id_for_mount.clone();
            let gating_manager_u5 = gating_manager_5.clone();
            Ok(vec![Disposer::from_fn(move || {
                for_each_agent(&gating_manager_u5, |a| {
                    a.reapply_plugin_gating(&plugin_id_unmount, false)
                });
                if running.swap(false, Ordering::SeqCst) {
                    if let Some(qq) = qq.clone() {
                        tokio::spawn(async move {
                            if let Err(e) = qq.stop().await {
                                warn!(error = %e, "QQ adapter stop via plugin unmount failed");
                            }
                        });
                    }
                }
            })])
        },
    )?;
}

// ── 实化 4：管理面（management WS 起停 + 会话视图命令域）──
// 实现已外迁为独立 crate `echo-plugin-management`（Phase 3 ⑧）：它只
// 认 `echo-protocol` 的线契约，**不需要 Agent 句柄**（入站命令塞
// bridge、事件从 bridge 出），组合根这里只负责起停与接缝注入。
//
// 注意自锁语义：禁用管理面 = 关闭 Panel 通道本身，恢复需编辑
// core.toml 的 disabled_plugins 后重启（文档已注明）。
//
// 全插件化 S5：面板的 5 条**会话视图命令**（RequestState / RequestContext /
// RequestTrunkTimeline / RequestTimelineEarlier / RequestSessionHistory）的
// 归属也在这里——"面板看到什么"与显示时间线的投影策略同源。宿主侧只注入
// `AgentSessionViewHost`（读窗口 / 读事件日志 / 跨人格遍历）。
{
    let mgmt_addr = management_address.clone();
    let mgmt_token = management_token.clone();
    let mgmt_bridge = bridge.clone();
    let view_host: std::sync::Arc<
        dyn echo_plugin_management::session_view::SessionViewHost,
    > = std::sync::Arc::new(echo_agent::agent::session_view_host::AgentSessionViewHost::new(
        agent.clone(),
    ));
    let view_handler = std::sync::Arc::new(
        echo_plugin_management::session_view::SessionViewHandler::new(view_host),
    );
    let mgmt_manifest = PluginManifest::builtin(
        MANAGEMENT_PANEL_PLUGIN_ID,
        "管理面",
        version,
        "management",
        "panel",
        "Panel management WS 桥接（合并模式下同时承接联邦入站）+ 会话视图命令域",
    );
    let mgmt_plugin = BuiltinPlugin::new(mgmt_manifest, move |mctx| {
            // **配置下发的第一个真实消费者**（2026-11）：管理面端口
            // 可以由 `plugins.toml` 覆盖，不必改 core.toml 再重启：
            //
            //     [[plugin]]
            //     id = "echo-agent.management.panel"
            //     kind = "builtin"
            //     [plugin.config]
            //     address = "127.0.0.1:8737"
            //
            // 只有 `address` 走这条路：**token 不从这里读**——插件
            // 配置会进插件清单接口与日志排查面，把凭证放这里等于
            // 主动泄漏（token 仍只认 core.toml / 环境变量）。
            let addr = mctx
                .config_str("address")
                .map(str::to_string)
                .unwrap_or_else(|| mgmt_addr.clone());
            if addr != mgmt_addr {
                tracing::info!(
                    configured = %mgmt_addr,
                    effective = %addr,
                    "management address overridden by plugins.toml"
                );
            }
            let br = mgmt_bridge.clone();
            let token = mgmt_token.clone();
            let fed_opt = mgmt_fed.clone();
            let fed_merged = fed_opt.is_some();
            let server = tokio::spawn(async move {
                let result = match fed_opt {
                    Some(fed) => {
                        crate::management::serve_with_federation(&addr, br, token, fed).await
                    }
                    None => crate::management::serve_with_token(&addr, br, token).await,
                };
                if let Err(e) = result {
                    // 管理面监听退出 = Panel 通道失效（合并模式下联邦
                    // 入站一并失效）；升级为 error 并给出排查线索
                    // （2026-10 审计：此前仅 warn，运行期静默）。
                    tracing::error!(
                        error = %e,
                        "management WS server stopped — Panel channel{} down;                                  check the bind address is free (ss -tlnp)",
                        if fed_merged { " and federation inbound" } else { "" }
                    );
                }
            });
            let abort = server.abort_handle();
            Ok(vec![Disposer::from_fn(move || abort.abort())])
    })
    .with_commands(echo_plugin_management::session_view::COMMANDS)
    .with_command_handler_async(move |name, payload| {
        let view_handler = view_handler.clone();
        Box::pin(async move {
            view_handler
                .handle(&name, payload)
                .await
                .unwrap_or(Ok(serde_json::Value::Null))
        })
    });
    push_plugin(&mut catalog, std::sync::Arc::new(mgmt_plugin))?;
}

// ── 实化 5：LLM provider（命令域在这里，全插件化 S5）──
//
// 此前是**名义挂载**（mount 空转、改配置重启才生效）：provider 的构造归
// 宿主，插件只是个身份。现在它有了真正的命令域——13 条 API / provider 命令
// 的注册关系归它（`echo_plugin_provider_llm::commands`），宿主只给一个接缝
// （`AgentProviderCommandHost`：读配置 / 重建 provider / 落盘 / 广播）。
//
// **仍然不是**"插件构造 provider"：构造要 `AgentConfig`、要重建链、要通知
// 全部会话——那是宿主的动作。插件的"启用"含义是**它的命令域可被派发**；
// 禁用后这些命令回落到宿主兜底分支，并明确报"插件未启用"。
{
    let provider_host: std::sync::Arc<
        dyn echo_plugin_provider_llm::commands::ProviderCommandHost,
    > = std::sync::Arc::new(echo_agent::agent::provider_host::AgentProviderCommandHost::new(
        agent.clone(),
    ));
    push_plugin(
        &mut catalog,
        echo_plugin_provider_llm::plugin(version, provider_host),
    )?;
}

// ── 实化 6：自更新（命令域 + updater 单元形状）──
//
// 全插件化 S5：`RequestSelfUpdate` / `RequestSelfUpdateStatus` 的归属与实现
// 一起搬出组合根（此前在 `federation_rt.rs`，且因在 `HOST_RESERVED` 名单里
// 永远走不到——见插件文档）。updater 单元名与状态文件布局是**部署形状**，
// 换部署方式就该换掉，正是插件的形状。
{
    let sink = event_sink.clone();
    let self_update_host: std::sync::Arc<
        dyn echo_plugin_self_update::commands::SelfUpdateHost,
    > = std::sync::Arc::new(echo_plugin_self_update::commands::SinkHost::new(sink));
    push_plugin(
        &mut catalog,
        echo_plugin_self_update::plugin(version, self_update_host),
    )?;
}

// ── plugins.toml 的**外部条目**：注册门控壳，让它们出现在插件清单里 ──
//
// 此前这些条目的子进程由 `plugins_runtime` 直接拉起（工具注册进
// agent），但**从不进 `PluginHost`**——于是面板的插件清单里根本看不到
// 用户装的外部插件，文案里却写着「外部组 = 来自插件目录」。
//
// 壳的语义是**名义挂载**（与 provider.llm 同款）：子进程已由
// supervisor 持有，registry 侧不再启停它——`enabled = false` 写进
// disabled_plugins 后需重启 Core 生效；重启按钮对这类条目直接给
// 明确的「请重启 Core」而不是假装能热重载。
//
// `builtin = false`：来源维度上它们不是编译进本二进制的（面板的
// 来源徽标读这个字段，分组读 `runtime`——两者正交，别互相顶替）。
for entry in external_entries {
    push_builtin(
            &mut catalog,
            PluginManifest::builtin(
                &entry.id,
                &entry.name,
                version,
                "tool",
                &entry.id,
                format!("外部插件（plugins.toml，{}）", entry.kind),
            )
            .with_runtime(PluginRuntime::Subprocess)
            .with_builtin_flag(false),
        nominal(entry.id.clone()),
    )?;
}
    reject_duplicate_commands(&catalog)?;
    reject_reserved_commands(&catalog)?;
    Ok(catalog)
}

/// 声明了**宿主保留命令**的插件是死代码：命令泵在分发前就把它挡掉了
/// （`command_is_host_reserved`），插件永远收不到，且**没有任何提示**。
///
/// 这正是 `RequestSelfUpdate` 曾经的形状——保留在名单里 + 实现只在插件侧 =
/// 必失败。装配期报出来，这类"声明了但收不到"就不可能再溜进去。
fn reject_reserved_commands(catalog: &[Arc<dyn Plugin>]) -> anyhow::Result<()> {
    for plugin in catalog {
        let id = plugin.manifest().id.clone();
        for command in plugin.handled_commands() {
            if echo_agent::plugins::command_is_host_reserved(&command) {
                return Err(anyhow::anyhow!(
                    "{id} declares host-reserved command {command}: the dispatcher will never                      route it to the plugin (remove the declaration or the reservation)"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 派生 id 的空段必须被拦下（真实踩到过：无 `[instances]` 配置时出现
    /// 一个叫 `echo-agent.instance.` 的幽灵插件）。
    #[test]
    fn plugin_id_shape_is_validated() {
        assert!(validate_plugin_id("echo-agent.instance.qq").is_ok());
        assert!(validate_plugin_id("echo-agent.tools.builtin").is_ok());
        assert!(validate_plugin_id("").is_err());
        assert!(validate_plugin_id("echo-agent.instance.").is_err());
        // 中间的空段是既有 id 形态（`<prefix>.<name>` 之外没人管），不拦。
        assert!(validate_plugin_id("echo-agent..x").is_ok());
    }

    /// 同一条命令被两个插件声明 → 装配期失败（否则先挂上的静默赢）。
    #[test]
    fn duplicate_command_declarations_are_rejected() {
        let one = Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("a.one", "a", "0", "tool", "a", "a"),
            |_| Ok(vec![]),
        )
        .with_commands(["Shared"]));
        let two = Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("a.two", "b", "0", "tool", "b", "b"),
            |_| Ok(vec![]),
        )
        .with_commands(["Shared"]));
        let err = reject_duplicate_commands(&[one, two]).expect_err("重名必须报错");
        assert!(err.to_string().contains("Shared"), "{err}");
    }

    /// 声明宿主保留命令 → 装配期失败（分发器永远不会路由给它）。
    #[test]
    fn reserved_command_declarations_are_rejected() {
        let plugin = Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("a.bad", "bad", "0", "tool", "bad", "bad"),
            |_| Ok(vec![]),
        )
        .with_commands(["TogglePlugin"]));
        let err = reject_reserved_commands(&[plugin]).expect_err("保留命令必须报错");
        assert!(err.to_string().contains("TogglePlugin"), "{err}");
    }
}

/// 同一条命令被两个内置插件声明 = **静默遮蔽**（先挂上的那个赢，另一个永远
/// 收不到）——这是注册关系层面的装配 bug，启动期就报出来。
///
/// 为什么要在这里查：命令名与归属都是各插件 crate 自己声明的（这正是 S5 的
/// 成果），框架分发时只是"问一遍谁认领"。跨 crate 的重名没有任何编译器守得住。
fn reject_duplicate_commands(catalog: &[Arc<dyn Plugin>]) -> anyhow::Result<()> {
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for plugin in catalog {
        let id = plugin.manifest().id.clone();
        for command in plugin.handled_commands() {
            if let Some(previous) = seen.insert(command.clone(), id.clone()) {
                return Err(anyhow::anyhow!(
                    "command {command} is claimed by both {previous} and {id}"
                ));
            }
        }
    }
    Ok(())
}
