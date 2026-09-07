//! EchoAgentCore — agent core service.
//!
//! 运行 Agent 核心：LLM agent 循环、QQ 适配器（OneBot v11 反向 WS :3131）、
//! 以及供 Panel 等前端连接的 management WebSocket（默认 :3132）。
//! 前端（TUI）在独立的 EchoAgentPanel 仓库中，通过 `echo-protocol` 定义的
//! WS/JSON 协议与本服务通信。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod agent_supervisor;
mod config;
mod handlers;
mod management;
mod qq_tools;

use config::CoreConfig;
use echo_adapter::traits::Adapter;
use echo_agent::{Agent, AgentProfile};
use echo_server::ConnectionTracker;

/// EchoAgentCore — agent core service (agent + QQ adapter + management WS).
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Path to the TOML configuration file.
    /// Default: config/echo-agent-core.toml
    #[arg(short, long)]
    config: Option<PathBuf>,
}

impl Args {
    fn config_path(&self) -> PathBuf {
        self.config
            .clone()
            .unwrap_or_else(|| PathBuf::from("config/echo-agent-core.toml"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let path = args.config_path();
    let cfg = CoreConfig::load(&path)?;
    run_core(args, cfg).await
}

// ── Core：Agent + QQ 适配器 + management WS 服务 ────────────────────────────

async fn run_core(args: Args, cfg: CoreConfig) -> Result<()> {
    init_tracing(&cfg.logging)?;

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    // Keep one sender alive for the whole `run_core` scope. The admin handler
    // (the only other sender) is stored inside the QQ adapter's handler
    // registry; if that registry is dropped during an adapter restart,
    // `shutdown_rx.changed()` would otherwise complete with a `RecvError` and
    // the select below would misinterpret it as an admin shutdown.
    let _shutdown_tx_guard = shutdown_tx.clone();
    let tracker = ConnectionTracker::default();

    // ---- agent framework ----
    let mut skills = echo_agent::SkillRegistry::discover(&cfg.agent.skills_dir)
        .map_err(|e| anyhow::anyhow!("failed to load skills: {e}"))?;
    // Apply persisted runtime disable state (survives restarts).
    for name in &cfg.agent.disabled_skills {
        if !skills.set_enabled(name, false) {
            warn!(skill = %name, "disabled_skills entry not found, skipping");
        }
    }
    info!(skills = ?skills.names(), "skills discovered");

    let mut provider_cfg = cfg.agent.clone();
    provider_cfg.apply_active_profile();
    // Fallback: if top-level provider is still empty but profiles exist, use the first one.
    if provider_cfg.provider.is_empty() {
        if let Some(first) = provider_cfg.api_profiles.first() {
            provider_cfg.provider = first.provider.clone();
            provider_cfg.model = first.model.clone();
            provider_cfg.base_url = first.base_url.clone();
            provider_cfg.api_key = first.api_key.clone();
            info!(name = %first.name, "auto-applied first API profile");
        }
    }
    provider_cfg.api_key = provider_cfg.effective_api_key();
    provider_cfg.base_url = provider_cfg.effective_base_url();

    if provider_cfg.api_key.is_empty() {
        let env_var = match provider_cfg.provider.as_str() {
            "deepseek" => "DEEPSEEK_API_KEY",
            "anthropic" | "claude" => "ANTHROPIC_API_KEY",
            _ => "OPENAI_API_KEY",
        };
        warn!("API key not set — use /api key <key> in Panel or set {env_var} env var");
    } else {
        info!(
            "API key is configured (length={})",
            provider_cfg.api_key.len()
        );
    }

    let provider = echo_agent::llm::create_provider(&provider_cfg)?;
    info!(provider = %provider_cfg.provider, model = %provider_cfg.model, "llm provider ready");

    // ---- Service context (dsh-style service locator) ----
    // Core services are registered under stable keys; extension plugins and
    // consumers resolve by key instead of importing concrete providers.
    let ctx: Arc<echo_context::Ctx> = Arc::new(echo_context::Ctx::default());
    let provider_arc: Arc<dyn echo_agent::LlmProvider> = Arc::from(provider);
    let _ctx_keep = ctx.register::<Arc<dyn echo_agent::LlmProvider>>("llm", provider_arc.clone());

    // The default turn runner (dsh: the loop is a swappable plugin, resolved
    // by key). Register it now; consumers (the agent driver) resolve it.
    let runner: Arc<echo_loop::TurnRunner> = Arc::new(echo_loop::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider_arc.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions::default(),
    ));
    let _runner_keep = ctx.register::<Arc<echo_loop::TurnRunner>>("loop", runner);

    // Adapter registry.
    let mut adapter_registry = echo_adapter::AdapterRegistry::new();

    // ---- QQ adapter (always registered, auto-started only when enabled) ----
    let qq_adapter: Arc<echo_adapter_qq::QqAdapter> =
        Arc::new(echo_adapter_qq::QqAdapter::new(cfg.qq_adapter.clone()));
    adapter_registry.register(qq_adapter.clone());
    if cfg.qq_adapter.enabled {
        info!("QQ adapter configured — will auto-start");
    } else {
        info!("QQ adapter registered — use /adapters → [s] start in Panel");
    }

    let adapters = Arc::new(adapter_registry);

    // The configured QQ owner is also an update administrator. Additional
    // administrators can be listed under `[agent.self_update]`.
    let mut agent_config = cfg.agent.clone();
    // System prompt is owned by the Core plugin, not the API config.
    agent_config.system_prompt = cfg.plugins.system_prompt.text.clone();
    if cfg.qq_adapter.owner_qq > 0 {
        let owner = cfg.qq_adapter.owner_qq as u64;
        if !agent_config.self_update.allowed_qq_users.contains(&owner) {
            agent_config.self_update.allowed_qq_users.push(owner);
        }
    }
    info!(
        memory_limit_tokens = agent_config.effective_memory_limit_tokens(),
        "agent context policy: single global trunk (token-budget trimmed)"
    );

    // Create the agent(s): multi-persona supervisor (one Agent per
    // profile; no profiles -> single default agent, legacy behavior).
    let config_store = echo_adapter::ConfigStore::new(args.config_path());

    // 多人格装配闭包：persona id + profile -> 独立 Agent 实例。
    let make_agent = {
        let provider_arc2 = provider_arc.clone();
        let skills2 = skills;
        let adapters2 = adapters.clone();
        let qq_adapter2 = qq_adapter.clone();
        let base_cfg = agent_config.clone();
        let config_store_path = args.config_path();
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        move |id: String, profile: AgentProfile| -> Arc<Agent> {
            let mut cfg = base_cfg.clone();
            // 人格系统提示词覆盖全局默认。
            if !profile.system_prompt.is_empty() {
                cfg.system_prompt = profile.system_prompt;
            }
            // 人格级记忆预算/上下文窗口覆盖全局（None = 继承全局）。
            if profile.memory_limit_tokens.is_some() {
                cfg.memory_limit_tokens = profile.memory_limit_tokens;
            }
            if profile.context_window_tokens.is_some() {
                cfg.context_window_tokens = profile.context_window_tokens;
            }
            // 每个 persona 独立的工具注册表（启动期一次性组装）。
            let mut t = echo_agent::ToolRegistry::new();
            echo_agent::tool::builtin::register_all(&mut t, adapters2.clone(), workspace.clone());
            // 平台（QQ）工具同样必须注册到每个 persona：QQ 消息的发送/查询
            // 都依赖 send_private_msg / send_group_msg 等工具，缺少它们时
            // persona 收到 QQ hook 后无法完成声明目标的投递。
            crate::qq_tools::register_qq_tools(&mut t, qq_adapter2.clone());
            // 包元数据：QQ 工具属于 "echo-agent.adapter.qq"（与对应技能同包）。
            for name in t.names() {
                if name.starts_with("send_") || name.contains("qq") || name.starts_with("get_") {
                    t.set_package(&name, "echo-agent.adapter.qq");
                }
            }
            t.set_package("checklist", echo_agent::plugins::CHECKLIST_PLUGIN_ID);
            let agent = Arc::new(echo_agent::Agent::new(
                Arc::clone(&provider_arc2),
                cfg,
                skills2.clone(),
                t,
                adapters2.clone(),
            ));
            agent.set_team_id(Some(id.clone()));
            // 能力配置在 make_agent 之后的启动阶段应用（见 start loop）。
            // 独立会话文件：echo-sessions-{id}.json（default 沿用旧文件名）。
            let file = if id == "default" {
                config_store_path.with_file_name("echo-sessions.json")
            } else {
                config_store_path.with_file_name(format!("echo-sessions-{id}.json"))
            };
            let store = echo_adapter::ConfigStore::new(file);
            agent.set_config_store(store);
            agent
        }
    };
    // 默认人格（管理面/QQ 入口）挂在原变量 `agent` 上，后续代码不变。
    let supervisor = std::sync::Arc::new(agent_supervisor::AgentSupervisor::build(
        &agent_config,
        make_agent,
    ));
    let default_id = supervisor.default_id();
    let default_persona = supervisor.resolve(None);
    let agent: Arc<echo_agent::Agent> = Arc::clone(&default_persona.agent);
    info!(agents = ?supervisor.ids(), default = %default_id, "agent supervisor ready");

    // ---- Frontend bridge (management WS) ----
    //（提前到插件挂载之前：management.panel 插件的 mount 闭包依赖它们）
    let (bridge, handle) = echo_agent::create_bridge();
    let bridge = Arc::new(bridge);
    agent.attach(Arc::new(handle));

    // ---- Sudo authorization broker ----
    // The broker is shared between the agent (run_sudo awaits a password
    // here) and the management server (sudo password frames are routed here
    // directly, bypassing the agent command queue, session log and LLM
    // context).
    let sudo_broker = Arc::new(echo_agent::SudoBroker::new());
    // 所有人格共享 sudo 授权通道。
    for persona in supervisor.personas() {
        persona.agent.attach_sudo_broker(sudo_broker.clone());
    }

    // ---- Plugin host: mount built-in modules as plugins ----
    // 插件化组合根：每个内置模块（工具集/技能/适配器/编排/管理面/LLM/Loop）
    // 以 PluginManifest + mount 闭包挂入 PluginHost。ADR-0018 第 2 步起，
    // tools.builtin / skills.dir / adapter.qq / management.panel 的 mount
    // 闭包执行真实副作用（禁用即卸载效果）；其余仍为名义挂载（Phase 3）。
    //
    // QQ 适配器的接线（hook/handler/config store）在插件挂载之后才完成；
    // qq_wired 标志保证启动期 mount 不抢跑启动适配器（启动期由接线后的
    // 门控启动负责），运行期 TogglePlugin 才真正 start/stop。
    let qq_wired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let qq_running = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        use echo_agent::plugins::{
            CHECKLIST_PLUGIN_ID, ToolSink, ADAPTER_QQ_PLUGIN_ID, MANAGEMENT_PANEL_PLUGIN_ID,
            SKILLS_DIR_PLUGIN_ID, TOOLS_BUILTIN_PLUGIN_ID,
        };
        use echo_context::Disposer;
        use echo_plugin::{BuiltinPlugin, MountContext, PluginKind, PluginManifest};
        use std::sync::atomic::Ordering;

        let plugin_host = agent.plugin_host.clone();
        // Agent 内部已有共享工具注册表（Arc），直接复用同一实例。
        plugin_host.set_mount_ctx(
            MountContext::new()
                .with_tools(Arc::new(ToolSink::new(agent.tools.clone())))
                .with_ctx(ctx.clone()),
        );

        // 先应用持久化的禁用状态再挂载：register_and_mount 对禁用插件只注册
        // 不挂载，禁用的插件在启动时不获得任何副作用。
        plugin_host
            .registry
            .apply_disabled(&cfg.agent.disabled_plugins);

        // 遍历全部 persona（运行期 TogglePlugin 时 AgentManager 已就位；
        // 启动期尚未设置，守卫为 no-op，启动期禁用恢复见 persona 循环）。
        fn for_each_agent(f: impl Fn(&echo_agent::Agent)) {
            if let Some(mgr) = echo_agent::agent_manager::global_manager() {
                for running in mgr.all() {
                    f(&running.agent);
                }
            }
        }

        let version = env!("CARGO_PKG_VERSION");
        fn register(
            plugin_host: &echo_agent::plugins::PluginHost,
            manifest: PluginManifest,
            mount: impl Fn(&MountContext) -> echo_plugin::PluginMountResult + Send + Sync + 'static,
        ) -> Result<()> {
            let id = manifest.id.clone();
            plugin_host
                .register_and_mount(Arc::new(BuiltinPlugin::new(manifest, mount)))
                .map_err(|e| anyhow::anyhow!(e).context(format!("mount plugin {id}")))?;
            Ok(())
        }

        // 名义挂载（Phase 3 实化：provider/loop 保持重启生效语义，编排依赖
        // echo-loop 迁移；UI 门控三插件由 allows_* 能力门真实生效）。
        let nominal = |entry: String| {
            move |_ctx: &MountContext| {
                let _ = entry;
                Ok(vec![])
            }
        };

        // ── 实化 1：内置工具集（包维度批量启停，跨 persona）──
        register(&plugin_host,
            PluginManifest::builtin(
                TOOLS_BUILTIN_PLUGIN_ID,
                "内置工具集",
                version,
                PluginKind::Tool,
                "builtin_tools",
                "平台无关的内置工具（计算/搜索/清单/编码/适配器管理）",
            ),
            move |_ctx| {
                for_each_agent(|a| a.apply_plugin_gating(TOOLS_BUILTIN_PLUGIN_ID, true));
                Ok(vec![Disposer::from_fn(|| {
                    for_each_agent(|a| a.apply_plugin_gating(TOOLS_BUILTIN_PLUGIN_ID, false));
                })])
            },
        )?;

        // ── 实化 2：技能目录（整表启停，跨 persona）──
        register(&plugin_host,
            PluginManifest::builtin(
                SKILLS_DIR_PLUGIN_ID,
                "技能目录",
                version,
                PluginKind::Skill,
                "skills_dir",
                "SKILL.md 技能目录（热重载）",
            ),
            move |_ctx| {
                for_each_agent(|a| a.apply_plugin_gating(SKILLS_DIR_PLUGIN_ID, true));
                Ok(vec![Disposer::from_fn(|| {
                    for_each_agent(|a| a.apply_plugin_gating(SKILLS_DIR_PLUGIN_ID, false));
                })])
            },
        )?;

        // ── 实化 3：任务清单（checklist 工具，包维度启停）──
        register(&plugin_host,
            PluginManifest::builtin(
                CHECKLIST_PLUGIN_ID,
                "任务清单",
                version,
                PluginKind::Tool,
                "checklist",
                "任务清单工具（清单保存/读取/勾选，可按人格独立启停）",
            ),
            move |_ctx| {
                for_each_agent(|a| a.apply_plugin_gating(CHECKLIST_PLUGIN_ID, true));
                Ok(vec![Disposer::from_fn(|| {
                    for_each_agent(|a| a.apply_plugin_gating(CHECKLIST_PLUGIN_ID, false));
                })])
            },
        )?;

        // ── 实化 3：QQ 适配器（启停进程 + 工具包启停）──
        {
            let qq = qq_adapter.clone();
            let wired = qq_wired.clone();
            let running = qq_running.clone();
            register(&plugin_host,
                PluginManifest::builtin(
                    ADAPTER_QQ_PLUGIN_ID,
                    "QQ 适配器",
                    version,
                    PluginKind::Adapter,
                    "qq",
                    "OneBot v11 反向 WS 适配器（含 QQ 管理工具）",
                ),
                move |_ctx| {
                    for_each_agent(|a| a.apply_plugin_gating(ADAPTER_QQ_PLUGIN_ID, true));
                    if wired.load(Ordering::SeqCst) && !running.swap(true, Ordering::SeqCst) {
                        let qq2 = qq.clone();
                        let running2 = running.clone();
                        tokio::spawn(async move {
                            if let Err(e) = qq2.start().await {
                                running2.store(false, Ordering::SeqCst);
                                warn!(error = %e, "QQ adapter start via plugin mount failed");
                            }
                        });
                    }
                    let qq = qq.clone();
                    let running = running.clone();
                    Ok(vec![Disposer::from_fn(move || {
                        for_each_agent(|a| a.apply_plugin_gating(ADAPTER_QQ_PLUGIN_ID, false));
                        if running.swap(false, Ordering::SeqCst) {
                            let qq3 = qq.clone();
                            tokio::spawn(async move {
                                if let Err(e) = qq3.stop().await {
                                    warn!(error = %e, "QQ adapter stop via plugin unmount failed");
                                }
                            });
                        }
                    })])
                },
            )?;
        }

        // ── 实化 4：管理面（management WS 起停）──
        // 注意自锁语义：禁用管理面 = 关闭 Panel 通道本身，恢复需编辑
        // core.toml 的 disabled_plugins 后重启（文档已注明）。
        {
            let mgmt_addr = cfg.core.management_address.clone();
            let mgmt_token = cfg.core.management_access_token.clone();
            let mgmt_agent = agent.clone();
            let mgmt_bridge = bridge.clone();
            let mgmt_sudo = sudo_broker.clone();
            register(&plugin_host,
                PluginManifest::builtin(
                    MANAGEMENT_PANEL_PLUGIN_ID,
                    "管理面",
                    version,
                    PluginKind::Management,
                    "panel",
                    "Panel management WS 桥接 / sudo 授权通道",
                ),
                move |_ctx| {
                    let (addr, br, ag, sudo) = (
                        mgmt_addr.clone(),
                        mgmt_bridge.clone(),
                        mgmt_agent.clone(),
                        mgmt_sudo.clone(),
                    );
                    let token = mgmt_token.clone();
                    let server = tokio::spawn(async move {
                        if let Err(e) = management::serve_with_token(&addr, br, ag, sudo, token).await {
                            warn!(error = %e, "management WS server stopped");
                        }
                    });
                    let abort = server.abort_handle();
                    Ok(vec![Disposer::from_fn(move || abort.abort())])
                },
            )?;
        }

        // ── 名义挂载（Phase 3）：orchestration / provider / loop 保持重启
        // 生效语义；三个 UI 门控插件由 allows_* 能力门真实生效。──
        let nominal_manifests: Vec<PluginManifest> = vec![
            PluginManifest::builtin(
                "echo-agent.orchestration",
                "编排",
                version,
                PluginKind::Orchestration,
                "orchestration",
                "后台任务/并行分支/子代理/定时器/框架自更新",
            ),
            PluginManifest::builtin(
                echo_agent::plugins::SINGLE_ORCHESTRATION_PLUGIN_ID,
                "单任务编排",
                version,
                PluginKind::Orchestration,
                "orchestration_single",
                "单任务编排模式：无会话管理 UI（会话卡/全局分组隐藏）、回执分支不可见（按 persona 白名单互斥推导，single 为兜底）",
            ),
            PluginManifest::builtin(
                echo_agent::plugins::CHATBOT_ORCHESTRATION_PLUGIN_ID,
                "多任务并行编排",
                version,
                PluginKind::Orchestration,
                "orchestration_chatbot",
                "多任务并行编排模式（chatbot）：会话列表/全局会话/可见回执分支的会话管理 UI 套件（与 single 互斥）",
            ),
            PluginManifest::builtin(
                "echo-agent.provider.llm",
                "LLM Provider",
                version,
                PluginKind::Provider,
                "llm",
                "LLM 提供方（deepseek/openai/anthropic/ollama 工厂）",
            ),
            PluginManifest::builtin(
                "echo-agent.loop.runner",
                "Turn Runner",
                version,
                PluginKind::Loop,
                "loop",
                "默认 turn/step 状态机与工具管道",
            ),
        ];
        for manifest in nominal_manifests {
            let entry = manifest.entry.clone();
            register(&plugin_host, manifest, nominal(entry))?;
        }

        info!(
            plugins = ?plugin_host.descriptors().iter().map(|d| d.id.clone()).collect::<Vec<_>>(),
            "plugins mounted"
        );
    }

    // ---- Background shell sessions（进程级，面板 + 工具共用）----
    {
        use echo_agent::shell::{ShellManager, ShellEvent};
        let mgr = std::sync::Arc::new(ShellManager::new());
        echo_agent::shell::set_shell_manager_global(std::sync::Arc::clone(&mgr));
        // 事件广播：接到默认 agent 的 handle，Panel 单连接即可实时可视化。
        let default_agent = agent.clone();
        echo_agent::shell::set_shell_emit(std::sync::Arc::new(move |event: ShellEvent| {
            default_agent.emit(echo_agent::shell::shell_event_to_backend(event));
        }));
        info!("background shell manager ready");
    }

    // 供编排工具（framework_update status）读取插件摘要的进程级锚点。
    // 注：default persona 已由 supervisor 持有 Arc，这里不再包一层。
    echo_agent::agent::set_plugin_host_global(agent.plugin_host.clone());
    // Restore persisted sessions and start periodic save (all personas).
    for persona in supervisor.personas() {
        let restored = persona.agent.load_sessions().await;
        if restored > 0 {
            info!(agent = %persona.id, restored, "sessions restored from disk");
        }
        if persona.agent.trunk.header().is_none() {
            persona
                .agent
                .trunk
                .set_header(echo_session::SessionHeader::top_level("trunk"));
        }
        persona.agent.apply_capabilities(&persona.profile).await;
        // 全局禁用插件（[agent].disabled_plugins）的启动期注册表效果：
        // 这些插件的 mount 已被 register_and_mount 守卫跳过，这里补齐
        // 包维度的工具/技能批量禁用（运行期启停由 mount/unmount 闭包处理）。
        for plugin_id in &cfg.agent.disabled_plugins {
            persona.agent.apply_plugin_gating(plugin_id, false);
        }
        // persona 白名单/黑名单的包维度启动期效果（运行期 unmount 闭包只在
        // AgentManager 就位后生效，启动期由这里覆盖）：白名单非空时不在
        // 名单内的"实化插件"也要批量禁用其工具/技能。
        for plugin_id in echo_agent::plugins::GATED_PLUGIN_IDS {
            let allowed = profile_allows_plugin(&persona.profile, plugin_id);
            if !allowed {
                persona.agent.apply_plugin_gating(plugin_id, false);
            }
        }
        // 全局 [agent].disabled_tools（Panel ToggleTool 持久化）应用到此
        // persona 的工具注册表。该状态序列化在 `[agent]` 段而非 persona
        // profile，因此必须在这里逐人格应用（此前应用在从未使用的全局
        // registry 上，实际从未生效）。
        for name in &cfg.agent.disabled_tools {
            if !persona.agent.tools.set_enabled(name, false).await {
                warn!(tool = %name, "disabled_tools entry not found, skipping");
            }
        }
        persona.agent.start_session_save_task();
        persona.agent.start_plugin_reload_task().await;
        persona.agent.start_orchestration_task();
    }

    // ---- Wire agent into QQ adapter ----
    qq_adapter.set_config_store(config_store.clone());
    // QQ events enter through a one-way hook. Outbound messages require tools.
    qq_adapter.set_message_hook(Arc::new(echo_agent::AgentMessageHook::new(agent.clone())));
    qq_adapter.add_handler(Box::new(handlers::EchoHandler::new(
        &cfg.bot.command_prefix,
    )));
    qq_adapter.add_handler(Box::new(handlers::HelpHandler::new(
        &cfg.bot.command_prefix,
    )));
    qq_adapter.add_handler(Box::new(handlers::AdminHandler::new(
        &cfg.bot.command_prefix,
        cfg.bot.owner_qq,
        shutdown_tx,
        tracker.clone(),
    )));

    // ---- Command pump（多 persona 路由）----
    // 默认后端通道接收 Panel 命令；SendMessage 按 agent_id 路由到对应人格，
    // 其余命令交给默认人格（管理面/QQ 入口同旧版）。
    let pump_default = agent.clone();
    let supervisor_for_pump = Arc::clone(&supervisor);
    let pump = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            while let Some(cmd) = pump_default.try_recv_command() {
                let target = match &cmd {
                    echo_agent::BackendCommand::SendMessage { team_id, .. }
                    | echo_agent::BackendCommand::CancelRequestedWork { team_id, .. } => {
                        let id = team_id.clone().unwrap_or_default();
                        if id.is_empty() {
                            Arc::clone(&pump_default)
                        } else {
                            match supervisor_for_pump.get(&id) {
                                Some(p) => Arc::clone(&p.agent),
                                None => Arc::clone(&pump_default),
                            }
                        }
                    }
                    _ => Arc::clone(&pump_default),
                };
                let branch_target = Arc::clone(&target);
                tokio::spawn(async move {
                    branch_target.apply_command(cmd).await;
                });
            }
        }
    });
    // 进程级 AgentManager 锚点（RequestAgentsList 使用）：把 supervisor 的
    // 人格注册表镜像成 echo_agent::AgentManager（共享同一 Agent 实例）。
    {
        // Keep disabled profiles in the manager registry so the Panel can
        // display and re-enable them; AgentManager itself skips instantiation
        // for `enabled=false` / `disabled_teams` entries.
        let raw = agent_config.clone();
        let mgr_arc = std::sync::Arc::new(echo_agent::AgentManager::build(&raw, |id, p| {
            // 返回 supervisor 中对应人格的 Agent（共享实例）。
            supervisor
                .get(&id)
                .map(|x| Arc::clone(&x.agent))
                .unwrap_or_else(|| {
                    // 兜底：不应发生（新人格通过 factory 创建，见下）
                    let _ = p;
                    agent.clone()
                })
        }));
        // 进程级 agent factory：运行时新增/重启人格时重建实例。
        {
            let supervisor2 = Arc::clone(&supervisor);
            echo_agent::agent_manager::set_agent_factory(move |id: String, p: AgentProfile| {
                if let Some(existing) = supervisor2.get(&id) {
                    return Arc::clone(&existing.agent);
                }
                // 新建人格：用保存的 make_agent 闭包（supervisor 提供）
                supervisor2.create(&id, p)
            });
        }
        // 配置写回：SaveAgent/DeleteAgent 持久化到 core.toml。
        {
            let store = config_store.clone();
            mgr_arc.set_config_writer(Box::new(
                move |teams: &std::collections::BTreeMap<String, echo_agent::TeamMember>| {
                    store
                        .patch(|root| {
                            let agent = echo_adapter::ensure_table(root, "agent");
                            let mut tbl = toml::map::Map::new();
                            for (id, p) in teams {
                                let mut v = toml::Value::try_from(p.clone())
                                    .map_err(|e| format!("serialize profile: {e}"))?;
                                if let Some(t) = v.as_table_mut() {
                                    // 持久化 enabled 状态（与运行期启用开关保持一致）。
                                    t.insert("enabled".into(), toml::Value::Boolean(p.enabled));
                                }
                                tbl.insert(id.clone(), v);
                            }
                            agent.insert("teams".into(), toml::Value::Table(tbl));
                            Ok(())
                        })
                        .map_err(|e| e.to_string())
                },
            ));
            echo_agent::agent_manager::set_global_manager(mgr_arc);
        }
    }
    // 事件镜像：非默认人格的事件经 Agent.event_bus 订阅 → 转发进默认
    // 人格的 handle.event_tx（Panel 单连接即可看到所有人格活动）。
    {
        let mirror_target = default_persona
            .agent
            .handle
            .try_read()
            .ok()
            .and_then(|h| h.as_ref().map(|d| d.event_tx.clone()));
        if let Some(tx) = mirror_target {
            for persona in supervisor.personas() {
                if persona.id == default_id {
                    continue;
                }
                let tx2 = tx.clone();
                let bus = persona.agent.event_bus.clone();
                let _keep = bus.observe(move |ev: &mut echo_agent::BackendEvent| {
                    let _ = tx2.send(ev.clone());
                });
                std::mem::forget(_keep);
            }
            info!("event mirror installed for non-default personas");
        }
    }

    // ---- Auto-start QQ adapter if enabled ----
    // 接线完成，标记 wired：此后 adapter.qq 插件的运行期 enable 才会真正
    // start 适配器（启动期 mount 不抢跑）。启动受配置与插件状态双重门控。
    qq_wired.store(true, std::sync::atomic::Ordering::SeqCst);
    if cfg.qq_adapter.enabled
        && agent
            .plugin_host
            .registry
            .is_enabled(echo_agent::plugins::ADAPTER_QQ_PLUGIN_ID)
    {
        qq_adapter
            .start()
            .await
            .map_err(|e| anyhow::anyhow!("QQ adapter start failed: {e}"))?;
        qq_running.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    // ---- Main loop ----
    tokio::select! {
        _ = shutdown_signal() => info!("shutdown signal received, exiting"),
        _ = shutdown_rx.changed() => info!("shutdown requested by admin command, exiting"),
    }

    // Graceful shutdown: stop the command pump and QQ adapter, cancel
    // background tasks, and flush sessions to disk.
    pump.abort();
    let _ = qq_adapter.stop().await;
    // 所有人格都要优雅关闭（drain + save_now）。只保存 default 时，非默认
    // 人格（如 self-coding）的最近会话 flush 不到磁盘——叠加此前没有周期
    // 保存任务，一次重启就会丢光非默认 agent 的上下文。
    let shutdown_tasks: Vec<_> = supervisor
        .personas()
        .into_iter()
        .map(|p| {
            let agent = Arc::clone(&p.agent);
            tokio::spawn(async move { agent.shutdown().await })
        })
        .collect();
    for task in shutdown_tasks {
        let _ = task.await;
    }
    Ok(())
}

/// persona 能力白名单语义（与 `Agent::allows_reply_branches` 等一致）：
/// 白名单非空时只允许名单内插件；黑名单再收紧。
fn profile_allows_plugin(profile: &AgentProfile, plugin_id: &str) -> bool {
    if !profile.enabled_plugins.is_empty() && !profile.enabled_plugins.iter().any(|p| p == plugin_id)
    {
        return false;
    }
    if profile.disabled_plugins.iter().any(|p| p == plugin_id) {
        return false;
    }
    true
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[derive(Clone)]
struct RotatingLogWriter {
    state: Arc<std::sync::Mutex<RotatingLogState>>,
}

struct RotatingLogState {
    path: PathBuf,
    file: std::fs::File,
    bytes_written: u64,
    max_bytes: u64,
    max_files: usize,
}

struct RotatingLogGuard {
    state: Arc<std::sync::Mutex<RotatingLogState>>,
}

impl RotatingLogWriter {
    fn open(path: PathBuf, max_bytes: u64, max_files: usize) -> Result<Self> {
        use anyhow::Context as _;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create log directory {}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open log file {}", path.display()))?;
        let bytes_written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        Ok(Self {
            state: Arc::new(std::sync::Mutex::new(RotatingLogState {
                path,
                file,
                bytes_written,
                max_bytes,
                max_files,
            })),
        })
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RotatingLogWriter {
    type Writer = RotatingLogGuard;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingLogGuard {
            state: Arc::clone(&self.state),
        }
    }
}

impl std::io::Write for RotatingLogGuard {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("log writer mutex poisoned"))?;
        state.rotate_if_needed(buffer.len() as u64)?;
        let written = std::io::Write::write(&mut state.file, buffer)?;
        state.bytes_written = state.bytes_written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("log writer mutex poisoned"))?;
        std::io::Write::flush(&mut state.file)
    }
}

impl RotatingLogState {
    fn rotate_if_needed(&mut self, incoming_bytes: u64) -> std::io::Result<()> {
        if self.bytes_written == 0
            || self.bytes_written.saturating_add(incoming_bytes) <= self.max_bytes
        {
            return Ok(());
        }
        std::io::Write::flush(&mut self.file)?;
        let oldest = rotated_log_path(&self.path, self.max_files);
        if oldest.exists() {
            std::fs::remove_file(oldest)?;
        }
        for index in (1..self.max_files).rev() {
            let source = rotated_log_path(&self.path, index);
            let destination = rotated_log_path(&self.path, index + 1);
            if source.exists() {
                let _ = std::fs::rename(source, destination);
            }
        }
        if self.path.exists() {
            std::fs::rename(&self.path, rotated_log_path(&self.path, 1))?;
        }
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.bytes_written = 0;
        Ok(())
    }
}

fn rotated_log_path(path: &std::path::Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

/// Core defaults to stdout/journald, but honors an explicit `log_file` with
/// rotating files.
fn init_tracing(cfg: &config::LoggingSection) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.level));
    if !cfg.log_file.is_empty() {
        let path = PathBuf::from(&cfg.log_file);
        eprintln!("logging to: {}", path.display());
        let writer = RotatingLogWriter::open(
            path,
            cfg.max_file_size_mb.saturating_mul(1024 * 1024),
            cfg.max_files,
        )?;
        init_subscriber(cfg, filter, writer, false)
    } else {
        init_subscriber(cfg, filter, std::io::stdout, true)
    }
}

fn init_subscriber<W>(
    cfg: &config::LoggingSection,
    filter: EnvFilter,
    writer: W,
    ansi: bool,
) -> Result<()>
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + 'static + Send + Sync,
{
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_ansi(ansi)
        .with_writer(writer);
    match cfg.format.as_str() {
        "json" => builder.json().try_init().map_err(anyhow::Error::msg)?,
        _ => builder.try_init().map_err(anyhow::Error::msg)?,
    }
    Ok(())
}

#[cfg(test)]
mod logging_tests {
    use super::*;
    use std::io::Write as _;
    use tracing_subscriber::fmt::MakeWriter as _;

    #[test]
    fn rotating_writer_keeps_numbered_history() {
        let path = std::env::temp_dir().join(format!(
            "echo-agent-rotation-test-{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(rotated_log_path(&path, 1));
        let writer = RotatingLogWriter::open(path.clone(), 8, 2).unwrap();
        {
            let mut guard = writer.make_writer();
            guard.write_all(b"12345678").unwrap();
            guard.write_all(b"next").unwrap();
            guard.flush().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "next");
        assert_eq!(
            std::fs::read_to_string(rotated_log_path(&path, 1)).unwrap(),
            "12345678"
        );
        drop(writer);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(rotated_log_path(&path, 1));
    }
}
