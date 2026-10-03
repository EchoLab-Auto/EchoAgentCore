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
mod node;
mod qq_instances;
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

    // 节点身份（federation Phase 0）：加载或生成持久化 NodeId。
    // 与配置 TOML 同目录（echo-node.json）；Phase 1 联邦链路消费。
    let node_doc = node::load_or_create(
        &args
            .config_path()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".")),
    )?;
    info!(node_id = %node_doc.node_id, "node identity ready");

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // ---- federation link（Phase 1：仅链路；工具路由 Phase 2 消费）----
    // 缺省整段关闭（[federation] enabled 缺省 false），单节点零行为变化。
    let federation = if cfg.federation.enabled {
        let peers = cfg
            .federation
            .peers
            .iter()
            .map(|(name, p)| echo_federation::PeerConfig {
                name: name.clone(),
                url: p.url.clone(),
                token: p.token.clone(),
            })
            .collect::<Vec<_>>();
        // 能力声明：本机可被远程调用的工具集合（Phase 2 由注册表实际驱动；
        // v1 先宣告内置编码工具族 + shell，与 RFC §2.2 一致）。
        let caps = echo_federation::NodeCaps {
            tools: [
                "bash",
                "read_file",
                "write_file",
                "edit_file",
                "search_code",
                "list_files",
                "shell_start",
                "shell_exec",
                "shell_stop",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            subagent: true,
            workspaces: Vec::new(),
        };
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
        let router = echo_agent::federation::InvokeRouter::new(node_doc.node_id.clone());
        // 策略表共享可变——SaveFederationPeer 运行时更新即生效（此前
        // 启动期冻结的 HashMap 会让 Panel 配的白名单静默失效到重启）。
        let peer_policies: std::sync::Arc<
            tokio::sync::RwLock<
                std::collections::HashMap<String, echo_agent::federation::ExecutorPolicy>,
            >,
        > = std::sync::Arc::new(tokio::sync::RwLock::new(
            cfg.federation
                .peers
                .iter()
                .map(|(name, p)| {
                    (
                        name.clone(),
                        echo_agent::federation::ExecutorPolicy {
                            allow_tools: p.allow_tools.clone(),
                            require_confirm: p.require_confirm.clone(),
                            allow_queries: p.allow_queries.clone(),
                            allow_subagent: p.allow_subagent,
                        },
                    )
                })
                .collect(),
        ));
        let fed = echo_federation::Federation::new(
            node_doc.node_id.clone(),
            cfg.federation
                .node_name
                .clone()
                .or(node_doc.node_name.clone()),
            caps,
            peers,
            event_tx,
            shutdown_tx.clone(),
        );
        let listen = if cfg.federation.listen.trim().is_empty() {
            None
        } else {
            Some(cfg.federation.listen.clone())
        };
        let fed_runner = fed.clone();
        tokio::spawn(async move {
            if let Err(e) = fed_runner.run(listen).await {
                warn!(error = %e, "federation run exited");
            }
        });
        let rt = Arc::new(FederationRuntime {
            federation: fed.clone(),
            router,
            peer_names: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            node_to_peer: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            config_store: echo_adapter::ConfigStore::new(args.config_path()),
            listen: cfg.federation.listen.clone(),
            node_name: cfg
                .federation
                .node_name
                .clone()
                .or(node_doc.node_name.clone()),
            peer_policies: peer_policies.clone(),
            remote_tool_keepers: std::sync::Mutex::new(Vec::new()),
        });
        // 远程 subagent 观测出口（Phase 3）：大脑侧 spawn_subagent
        // node=... 受理/完成时经 Federation 通知对端（帧：
        // SubagentSpawn 受理 / SubagentEvent 终态）。peer 名 → node_id
        // 反查走 rt.peer_names（链路 Up 时登记）。
        {
            let rt_notify = rt.clone();
            echo_agent::federation::set_remote_subagent_notifier(std::sync::Arc::new(
                move |peer, call_id, task, timeout_secs, status, result| {
                    let rt = rt_notify.clone();
                    let peer = peer.to_string();
                    let call_id = call_id.to_string();
                    let task = task.to_string();
                    tokio::spawn(async move {
                        let node_id = rt.peer_names.read().await.get(&peer).cloned();
                        let Some(node_id) = node_id else {
                            tracing::warn!(
                                target: "federation",
                                peer = %peer, "remote subagent notify: peer offline"
                            );
                            return;
                        };
                        let frame = if matches!(status, echo_federation::SubagentStatus::Running) {
                            echo_federation::FedFrame::SubagentSpawn(
                                echo_federation::SubagentSpawnRequest {
                                    call_id,
                                    task,
                                    timeout_secs,
                                },
                            )
                        } else {
                            echo_federation::FedFrame::SubagentEvent(
                                echo_federation::SubagentEventFrame {
                                    call_id,
                                    status,
                                    result,
                                },
                            )
                        };
                        let _ = rt.federation.send_to(&node_id, frame).await;
                    });
                },
            ));
        }
        // 联邦管理命令处理器（SaveFederationPeer 等）注入进程级注册表，
        // agent 命令域据此分发（联邦关闭时未注入 → 明确报错）。
        {
            let rt_handler = rt.clone();
            echo_agent::agent::set_federation_command_handler(std::sync::Arc::new(
                move |agent, cmd| {
                    let rt = rt_handler.clone();
                    let emit = agent.emit_handle();
                    Box::pin(async move {
                        handle_federation_command(emit, cmd, rt).await;
                    })
                },
            ));
        }
        Some((rt, event_rx, peer_policies))
    } else {
        None
    };
    info!(enabled = federation.is_some(), "federation link configured");
    // Keep one sender alive for the whole `run_core` scope. The admin handler
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
        echo_loop::LoopOptions {
            max_tool_iterations: cfg.agent.max_tool_iterations.max(1),
            max_tokens: cfg.agent.effective_max_tokens(),
            ..Default::default()
        },
    ));
    let _runner_keep = ctx.register::<Arc<echo_loop::TurnRunner>>("loop", runner.clone());

    // Adapter registry.
    let mut adapter_registry = echo_adapter::AdapterRegistry::new();

    // ---- QQ 实例（多实例；实例 = 适配器名 = 会话 account 维度）----
    // persona 门控：启用了 `echo-agent.adapter.qq` 的人格各自获得实例
    // （未配置实例时自动建档），legacy 单实例部署行为不变。
    let qq_enabled_personas: Vec<String> = cfg
        .agent
        .teams
        .iter()
        .filter(|(_, m)| {
            m.enabled
                && echo_agent::plugins::profile_allows_plugin(
                    m,
                    echo_agent::plugins::ADAPTER_QQ_PLUGIN_ID,
                )
        })
        .map(|(id, _)| id.clone())
        .collect();
    let qq_default_persona = qq_enabled_personas
        .first()
        .cloned()
        .unwrap_or_else(|| cfg.agent.teams.keys().next().cloned().unwrap_or_default());
    // 数据目录：实例 compose / 卷命名都从这里派生（配置同目录）。
    // 配置存储（QQ 实例端口持久化、Agent 配置写回等都依赖它）。
    let config_store = echo_adapter::ConfigStore::new(args.config_path());
    let qq_data_dir = args
        .config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let qq_instances = qq_instances::resolve_instances_in(
        &cfg.qq_adapter,
        &cfg.qq_instances,
        &qq_enabled_personas,
        &qq_default_persona,
        Some(&qq_data_dir),
    );
    let mut qq_adapters: Vec<(String, String, Arc<echo_adapter_qq::QqAdapter>)> = Vec::new();
    for instance in &qq_instances {
        let adapter = Arc::new(echo_adapter_qq::QqAdapter::with_instance(
            instance.id.clone(),
            Some(instance.persona.clone()),
            instance.config.clone(),
        ));
        adapter_registry.register(adapter.clone());
        qq_adapters.push((instance.id.clone(), instance.persona.clone(), adapter));
    }
    if !qq_instances.is_empty() {
        let ids: Vec<&str> = qq_instances.iter().map(|i| i.id.as_str()).collect();
        info!(instances = ?ids, "QQ instances registered");
    }
    // 兼容旧单实例路径：第一个实例（通常是 `qq`）。
    let qq_adapter: Arc<echo_adapter_qq::QqAdapter> = qq_adapters
        .first()
        .map(|(_, _, a)| a.clone())
        .unwrap_or_else(|| Arc::new(echo_adapter_qq::QqAdapter::new(cfg.qq_adapter.clone())));
    // 实例持久化：新分配的**归属人格与端口**写回
    // `[adapters.qq.instances.<id>]`（persona + ports），保证跨重启稳定：
    // - 端口：容器端口映射必须稳定，NapCat 才能重连到同一处；
    // - persona：自动建档实例若只写端口，重启后 persona 缺失会被重新
    //   分配给默认人格（首个启用 QQ 的人格），实例归属漂移。
    {
        struct InstancePatch {
            id: String,
            persona: String,
            ports: qq_instances::QqPorts,
            /// persona 是否需要写回（已有显式 persona 的实例跳过该字段）。
            write_persona: bool,
        }
        let mut patches: Vec<InstancePatch> = Vec::new();
        for instance in &qq_instances {
            if instance.id == qq_instances::DEFAULT_INSTANCE {
                continue; // legacy 实例沿用既定端口与默认归属，不写配置
            }
            let section = cfg.qq_instances.get(&instance.id);
            let configured_ports = section.map(|s| s.ports.clone()).unwrap_or_default();
            let has_persona = section.and_then(|s| s.persona.clone()).is_some();
            if configured_ports != instance.ports || !has_persona {
                patches.push(InstancePatch {
                    id: instance.id.clone(),
                    persona: instance.persona.clone(),
                    ports: instance.ports.clone(),
                    write_persona: !has_persona,
                });
            }
        }
        if !patches.is_empty() {
            if let Err(error) = config_store.patch(|root| {
                let adapters = echo_adapter::ensure_table(root, "adapters");
                let qq = echo_adapter::ensure_table(adapters, "qq");
                let instances = echo_adapter::ensure_table(qq, "instances");
                for patch in &patches {
                    let entry = echo_adapter::ensure_table(instances, &patch.id);
                    let value =
                        toml::Value::try_from(&patch.ports).map_err(|e| format!("ports: {e}"))?;
                    entry.insert("ports".into(), value);
                    if patch.write_persona {
                        entry.insert("persona".into(), toml::Value::String(patch.persona.clone()));
                    }
                }
                Ok(())
            }) {
                warn!(%error, "QQ instance persist failed");
            } else {
                let ids: Vec<&str> = patches.iter().map(|patch| patch.id.as_str()).collect();
                info!(instances = ?ids, "QQ instance persona/ports allocated and persisted");
            }
        }
    }

    // persona → 该人格的 QQ 实例集合（工具注册按人格隔离）。
    let qq_by_persona: std::collections::HashMap<String, Vec<Arc<echo_adapter_qq::QqAdapter>>> = {
        let mut map: std::collections::HashMap<String, Vec<Arc<echo_adapter_qq::QqAdapter>>> =
            std::collections::HashMap::new();
        for (_, persona, adapter) in &qq_adapters {
            map.entry(persona.clone())
                .or_default()
                .push(adapter.clone());
        }
        map
    };

    let adapters = Arc::new(adapter_registry);

    let mut agent_config = cfg.agent.clone();
    // System prompt is owned by the Core plugin, not the API config.
    agent_config.system_prompt = cfg.plugins.system_prompt.text.clone();
    info!(
        memory_limit_tokens = agent_config.effective_memory_limit_tokens(),
        "agent context policy: single global trunk (token-budget trimmed)"
    );

    // ---- Frontend bridge (management WS) ----
    //（提前到人格装配之前：每个 persona 建好即接入进程级事件汇聚点，
    //  不再依赖"默认人格"转接事件）
    let (bridge, handle) = echo_agent::create_bridge();
    let bridge = Arc::new(bridge);
    let handle = Arc::new(handle);
    // 进程级事件汇聚点：所有 persona 的 emit 都投递到它（即 bridge 的事件端），
    // Panel 单连接即可看到全部人格活动。
    let event_sink: echo_agent::EventSink = {
        let tx = handle.event_tx.clone();
        Arc::new(move |event| {
            let _ = tx.send(event);
        })
    };

    // 进程级插件宿主（去主 P1）：所有人格共享同一实例，插件注册/挂载/卸载
    // 与「谁挂到 Panel」同样不再寄居在某个"默认人格"上。
    let plugin_host: std::sync::Arc<echo_agent::plugins::PluginHost> =
        std::sync::Arc::new(echo_agent::plugins::PluginHost::new());
    // 进程级全局策略（工具/技能启停）：全局开关不属于任何人格。
    echo_agent::agent::set_global_policy(std::sync::Arc::new(
        echo_agent::agent::GlobalPolicy::new(
            agent_config.disabled_tools.clone(),
            agent_config.disabled_skills.clone(),
        ),
    ));

    // 多人格装配闭包：persona id + profile -> 独立 Agent 实例。
    // 用 Arc 包一层：supervisor 与"进程级核心服务代理"共用同一装配逻辑；
    // 装配所需的共享件预先 clone 好，闭包按需再 clone（Fn 语义）。
    let factory_provider = provider_arc.clone();
    let factory_skills = skills.clone();
    let factory_adapters = adapters.clone();
    let factory_qq_adapter = qq_adapter.clone();
    let factory_qq_by_persona = qq_by_persona.clone();
    let factory_base_cfg = agent_config.clone();
    let factory_event_sink = event_sink.clone();
    let factory_plugin_host = plugin_host.clone();
    let factory_config_store = config_store.clone();
    let factory_config_path = args.config_path();
    let make_agent: Arc<dyn Fn(String, AgentProfile) -> Arc<Agent> + Send + Sync> = Arc::new(
        move |id: String, profile: AgentProfile| -> Arc<Agent> {
            let provider_arc2 = factory_provider.clone();
            let skills2 = factory_skills.clone();
            let adapters2 = factory_adapters.clone();
            let qq_adapter2 = factory_qq_adapter.clone();
            let base_cfg = factory_base_cfg.clone();
            let event_sink = factory_event_sink.clone();
            let shared_plugin_host = factory_plugin_host.clone();
            let config_store_path = factory_config_path.clone();
            let agents_config_store = factory_config_store.clone();
            let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            {
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
                echo_agent::tool::builtin::register_all(
                    &mut t,
                    adapters2.clone(),
                    workspace.clone(),
                );
                // 平台（QQ）工具同样必须注册到每个 persona：QQ 消息的发送/查询
                // 都依赖 send_private_msg / send_group_msg 等工具，缺少它们时
                // persona 收到 QQ hook 后无法完成声明目标的投递。
                // 每人格注册**自己实例集合**的 QQ 工具（多实例经 account 寻址）。
                let persona_instances = factory_qq_by_persona
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| vec![qq_adapter2.clone()]);
                crate::qq_tools::register_qq_tools_multi(&mut t, persona_instances);
                // 包元数据：QQ 工具属于 "echo-agent.adapter.qq"（与对应技能同包）。
                for name in t.names() {
                    if name.starts_with("send_") || name.contains("qq") || name.starts_with("get_")
                    {
                        t.set_package(&name, "echo-agent.adapter.qq");
                    }
                }
                // 任务清单（checklist）为普通内置工具：随内置工具集包
                // （register_all 已打 echo-agent.tools.builtin 标签），
                // 插件维度已于 2026-09 移除，只受工具级白/黑名单控制。
                // 工作区会话（workspace 插件）：每 persona 独立存储 + 工具。
                // 存储文件与配置 TOML 解耦（独立 JSON，绝不同路径互写）；
                // 插件未列入该 persona 白名单时工具会被门控禁用（GATED_PLUGIN_IDS）。
                let workspace_store =
                    std::sync::Arc::new(echo_agent::workspace::WorkspaceStore::load(Some(
                        config_store_path.with_file_name(format!("echo-workspaces-{id}.json")),
                    )));
                t.register(std::sync::Arc::new(
                    echo_agent::workspace::WorkspaceTool::new(workspace_store.clone()),
                ));
                t.set_package("workspace", echo_agent::plugins::WORKSPACE_PLUGIN_ID);
                // Subagent 委派（subagent 插件）：每 persona 独立注册表；spawn
                // 执行闭包由 attach_subagent_runtime 在 agent 创建后接线
                // （需要 Arc<Agent> 弱引用），此处先注册占位工具 + 打包标签。
                let subagent_store = echo_agent::subagent::SubagentStore::new();
                t.register(std::sync::Arc::new(
                    echo_agent::subagent::SpawnSubagentTool::new(
                        subagent_store.clone(),
                        std::sync::Arc::new(|_| {}),
                    ),
                ));
                t.set_package("spawn_subagent", echo_agent::plugins::SUBAGENT_PLUGIN_ID);
                // Persona 级 API：配置了 api_profile 的 persona 在启动时构建
                // 自己的 provider（从全局池解析，不共享默认 provider）。
                let mut api_cfg_override: Option<echo_agent::AgentConfig> = None;
                let persona_provider: Option<Arc<dyn echo_agent::LlmProvider>> = if let Some(
                    ref name,
                ) =
                    profile.api_profile
                {
                    let mut resolved = cfg.clone();
                    if resolved.apply_named_profile(name) {
                        resolved.api_key = resolved.effective_api_key();
                        resolved.base_url = resolved.effective_base_url();
                        match echo_agent::llm::create_provider(&resolved) {
                            Ok(p) => {
                                api_cfg_override = Some(resolved.clone());
                                Some(Arc::from(p) as Arc<dyn echo_agent::LlmProvider>)
                            }
                            Err(e) => {
                                tracing::warn!(agent = %id, profile = %name, error = %e,
                                    "persona API profile provider build failed; falling back to default provider");
                                None
                            }
                        }
                    } else {
                        tracing::warn!(agent = %id, profile = %name,
                            "persona API profile not found in pool; falling back to default provider");
                        None
                    }
                } else {
                    None
                };
                let agent = Arc::new(echo_agent::Agent::new(
                    persona_provider.unwrap_or_else(|| Arc::clone(&provider_arc2)),
                    cfg.clone(),
                    skills2.clone(),
                    t,
                    adapters2.clone(),
                ));
                agent.set_team_id(Some(id.clone()));
                // 工作区会话存储（命令处理 + 系统提示注入共用同一实例）。
                agent.set_workspace_store(workspace_store);
                // 接入进程级事件汇聚点与共享插件宿主（运行期新建人格同样走这里）。
                agent.attach_event_sink(event_sink.clone());
                agent.set_plugin_host(shared_plugin_host.clone());
                // Persona 级 API 引用（None = 跟随全局默认；运行期重建走 apply_persona_api）。
                agent.set_persona_api_now(profile.api_profile.clone());
                if let Some(api_cfg) = api_cfg_override {
                    // 启动期已按 persona profile 解析好：把生效 model 同步给
                    // active_model（provider 已独立构建，无需重建）。
                    agent.set_model_now(api_cfg.model);
                }
                // 能力配置在 make_agent 之后的启动阶段应用（见 start loop）。
                //
                // 配置持久化（TOML）与会话持久化（JSON）解耦：
                // - config store：共享 core.toml 的 ConfigStore（[agent] section）
                // - 会话路径：独立 JSON 文件 echo-sessions-{id}.json（统一命名；
                //   旧的 default 专用 echo-sessions.json 首次启动自动改名迁移）
                // 两者文件格式不同，绝不可共用同一路径（JSON 会让 TOML parse 失败）。
                agent.set_config_store(agents_config_store.clone());
                let file = config_store_path.with_file_name(format!("echo-sessions-{id}.json"));
                if id == "default" {
                    let legacy = config_store_path.with_file_name("echo-sessions.json");
                    if !file.exists() && legacy.exists() {
                        match std::fs::rename(&legacy, &file) {
                            Ok(()) => tracing::info!(
                                from = %legacy.display(),
                                to = %file.display(),
                                "migrated legacy session file"
                            ),
                            Err(error) => tracing::warn!(
                                %error,
                                "legacy session file migration failed; starting fresh"
                            ),
                        }
                    }
                }
                agent.set_session_persist_path(file);
                agent.attach_subagent_runtime(subagent_store);
                agent
            }
        },
    );
    // 默认人格（管理面/QQ 入口）挂在原变量 `agent` 上，后续代码不变。
    // 进程级核心服务代理（不是人格）：承接全局/管理类命令（API 配置、技能/
    // 工具/插件清单与启停、适配器启停、Shell、Teams 管理…）。它不出现在
    // `[agent.teams]` 与 TeamsList 中，team_id 为空，也不接收聊天消息。
    let core_agent: Arc<echo_agent::Agent> = make_agent(
        "__core".into(),
        echo_agent::AgentProfile {
            name: "Core".into(),
            description: "进程级核心服务（非人格）：管理面命令宿主".into(),
            enabled: true,
            ..Default::default()
        },
    );
    core_agent.set_team_id(None);

    let supervisor =
        std::sync::Arc::new(agent_supervisor::AgentSupervisor::build(&agent_config, {
            let f = make_agent.clone();
            move |id: String, profile: AgentProfile| f(id, profile)
        }));
    // 去"主智能体"：不存在默认人格。全局/管理用途一律走进程级核心服务代理
    // （`core_agent`，team_id 为空、不在 TeamsList 中）；聊天与会话按 team_id
    // 显式路由到具体人格。
    let agent: Arc<echo_agent::Agent> = core_agent.clone();
    info!(agents = ?supervisor.ids(), "agent supervisor ready");

    // 联邦路由泵（Phase 2）：supervisor 就绪后启动（代理工具注册需要全部
    // persona 的注册表）。
    if let Some((rt, event_rx, peer_policies)) = federation {
        tokio::spawn(federation_router_pump(
            event_rx,
            rt,
            supervisor.clone(),
            peer_policies,
        ));
    }

    core_agent
        .apply_capabilities(&echo_agent::AgentProfile {
            enabled: true,
            ..Default::default()
        })
        .await;

    // ---- Plugin host: mount built-in modules as plugins ----
    // 插件化组合根：每个内置模块（工具集/技能/适配器/编排/管理面/LLM/Loop）
    // 以 PluginManifest + mount 闭包挂入 PluginHost。实化插件
    //（tools.builtin / skills.dir / checklist / adapter.qq / management.panel /
    // loop.runner）的 mount 闭包执行真实副作用（禁用即卸载效果）；
    // provider.llm 仍为名义挂载（重启生效）。
    //
    // QQ 适配器的接线（hook/handler/config store）在插件挂载之后才完成；
    // qq_wired 标志保证启动期 mount 不抢跑启动适配器（启动期由接线后的
    // 门控启动负责），运行期 TogglePlugin 才真正 start/stop。
    let qq_wired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let qq_running = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        use echo_agent::plugins::{
            ToolSink, ADAPTER_QQ_PLUGIN_ID, MANAGEMENT_PANEL_PLUGIN_ID, SKILLS_DIR_PLUGIN_ID,
            TOOLS_BUILTIN_PLUGIN_ID,
        };
        use echo_context::Disposer;
        use echo_plugin::{BuiltinPlugin, MountContext, PluginKind, PluginManifest};
        use std::sync::atomic::Ordering;

        let plugin_host = plugin_host.clone();
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
        fn for_each_agent(f: impl Fn(&std::sync::Arc<echo_agent::Agent>)) {
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

        // 名义挂载：provider.llm 保持重启生效语义。
        let nominal = |entry: String| {
            move |_ctx: &MountContext| {
                let _ = entry;
                Ok(vec![])
            }
        };

        // ── 实化 1：内置工具集（包维度批量启停，跨 persona）──
        register(
            &plugin_host,
            PluginManifest::builtin(
                TOOLS_BUILTIN_PLUGIN_ID,
                "内置工具集",
                version,
                PluginKind::Tool,
                "builtin_tools",
                "平台无关的内置工具（计算/搜索/清单/编码/适配器管理）",
            ),
            move |_ctx| {
                // 逐 persona 重评估：全局启用 ∧ 各 persona 白/黑名单。
                for_each_agent(|a| a.reapply_plugin_gating(TOOLS_BUILTIN_PLUGIN_ID, true));
                Ok(vec![Disposer::from_fn(|| {
                    for_each_agent(|a| a.reapply_plugin_gating(TOOLS_BUILTIN_PLUGIN_ID, false));
                })])
            },
        )?;

        // ── 实化 2：技能目录（整表启停，跨 persona）──
        register(
            &plugin_host,
            PluginManifest::builtin(
                SKILLS_DIR_PLUGIN_ID,
                "技能目录",
                version,
                PluginKind::Skill,
                "skills_dir",
                "SKILL.md 技能目录（热重载）",
            ),
            move |_ctx| {
                for_each_agent(|a| a.reapply_plugin_gating(SKILLS_DIR_PLUGIN_ID, true));
                Ok(vec![Disposer::from_fn(|| {
                    for_each_agent(|a| a.reapply_plugin_gating(SKILLS_DIR_PLUGIN_ID, false));
                })])
            },
        )?;

        // ── 实化 4：循环模式（echo-loop 可插拔循环驱动，二选一）──
        // 两个模式插件 mount 的是同一个 TurnRunner：注入各 agent 并启用
        // echo-loop 驱动（普通 TUI turn 经 turn/step 状态机执行）；
        // 差异（单会话串行 / 并行多会话）由 per-persona 白名单推导的
        // `LoopMode` 决定（见 `TeamMember::loop_mode`）。
        // 计数器保证"两个都挂载也不会提前回退"，全部卸载才恢复内置循环。
        let loop_mounts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for (id, name, description) in [
            (
                echo_agent::plugins::SINGLE_LOOP_PLUGIN_ID,
                "单会话循环",
                "单会话（默认）：同一会话的 turn 串行排队，后续输入等待前一轮结束；无会话管理 UI、回执分支不可见",
            ),
            (
                echo_agent::plugins::PARALLEL_LOOP_PLUGIN_ID,
                "并行多会话循环",
                "并行多会话：同一会话可并发多个 turn 分支，会话列表/全局会话/可见回执分支全套",
            ),
        ] {
            let runner = runner.clone();
            let loop_mounts = Arc::clone(&loop_mounts);
            register(&plugin_host,
                PluginManifest::builtin(
                    id,
                    name,
                    version,
                    PluginKind::Loop,
                    "loop",
                    description,
                ),
                move |_ctx| {
                    loop_mounts.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    for_each_agent(|a| {
                        a.set_loop_runner(runner.clone());
                        a.set_use_echo_loop(true);
                    });
                    let counter = Arc::clone(&loop_mounts);
                    Ok(vec![Disposer::from_fn(move || {
                        if counter.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
                            // 最后一个循环插件卸载：回退内置循环。
                            for_each_agent(|a| a.set_use_echo_loop(false));
                        }
                    })])
                },
            )?;
        }

        // 任务清单（checklist）已降级为普通内置工具：无插件注册，
        // 与 calculator / bash 等同层，只受工具级白/黑名单门控。

        // ── 实化 6：工作区会话（workspace 插件）──
        {
            use echo_agent::plugins::WORKSPACE_PLUGIN_ID;
            register(
                &plugin_host,
                PluginManifest::builtin(
                    WORKSPACE_PLUGIN_ID,
                    "工作区会话",
                    version,
                    PluginKind::Tool,
                    "workspace",
                    "基于工作空间的会话管理：Panel 多会话/多目录管理 + git 状态查看 + workspace 工具",
                ),
                move |_ctx| {
                    for_each_agent(|a| a.reapply_plugin_gating(WORKSPACE_PLUGIN_ID, true));
                    Ok(vec![Disposer::from_fn(|| {
                        for_each_agent(|a| a.reapply_plugin_gating(WORKSPACE_PLUGIN_ID, false));
                    })])
                },
            )?;
        }

        // 选单（present_menu）已于 2026-09 废弃移除（无插件、无 broker）。

        // ── 实化 7：subagent 委派（隔离上下文子任务 + 完成 hook 回灌）──
        {
            use echo_agent::plugins::SUBAGENT_PLUGIN_ID;
            register(
                &plugin_host,
                PluginManifest::builtin(
                    SUBAGENT_PLUGIN_ID,
                    "Subagent 委派",
                    version,
                    PluginKind::Tool,
                    "subagent",
                    "委派独立子任务给隔离上下文的子 agent：spawn_subagent 工具 + 完成后 <subagent_event> hook 回灌主 agent",
                ),
                move |_ctx| {
                    for_each_agent(|a| a.reapply_plugin_gating(SUBAGENT_PLUGIN_ID, true));
                    Ok(vec![Disposer::from_fn(|| {
                        for_each_agent(|a| a.reapply_plugin_gating(SUBAGENT_PLUGIN_ID, false));
                    })])
                },
            )?;
        }

        // ── 实化 3：QQ 适配器（启停进程 + 工具包启停）──
        {
            let qq = qq_adapter.clone();
            let wired = qq_wired.clone();
            let running = qq_running.clone();
            register(
                &plugin_host,
                PluginManifest::builtin(
                    ADAPTER_QQ_PLUGIN_ID,
                    "QQ 适配器",
                    version,
                    PluginKind::Adapter,
                    "qq",
                    "OneBot v11 反向 WS 适配器（含 QQ 管理工具）",
                ),
                move |_ctx| {
                    for_each_agent(|a| a.reapply_plugin_gating(ADAPTER_QQ_PLUGIN_ID, true));
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
                        for_each_agent(|a| a.reapply_plugin_gating(ADAPTER_QQ_PLUGIN_ID, false));
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
            let mgmt_agent = core_agent.clone();
            let mgmt_bridge = bridge.clone();
            register(
                &plugin_host,
                PluginManifest::builtin(
                    MANAGEMENT_PANEL_PLUGIN_ID,
                    "管理面",
                    version,
                    PluginKind::Management,
                    "panel",
                    "Panel management WS 桥接",
                ),
                move |_ctx| {
                    let (addr, br, ag) =
                        (mgmt_addr.clone(), mgmt_bridge.clone(), mgmt_agent.clone());
                    let token = mgmt_token.clone();
                    let server = tokio::spawn(async move {
                        if let Err(e) = management::serve_with_token(&addr, br, ag, token).await {
                            warn!(error = %e, "management WS server stopped");
                        }
                    });
                    let abort = server.abort_handle();
                    Ok(vec![Disposer::from_fn(move || abort.abort())])
                },
            )?;
        }

        // ── 名义挂载：provider.llm 保持重启生效语义。──
        {
            use echo_agent::plugins::PROVIDER_LLM_PLUGIN_ID;
            register(
                &plugin_host,
                PluginManifest::builtin(
                    PROVIDER_LLM_PLUGIN_ID,
                    "LLM Provider",
                    version,
                    PluginKind::Provider,
                    "llm",
                    "LLM 提供方（deepseek/openai/anthropic/ollama 工厂）",
                ),
                nominal("llm".to_string()),
            )?;
        }

        info!(
            plugins = ?plugin_host.descriptors().iter().map(|d| d.id.clone()).collect::<Vec<_>>(),
            "plugins mounted"
        );
    }

    // ---- Background shell sessions（进程级，面板 + 工具共用）----
    {
        use echo_agent::shell::{ShellEvent, ShellManager};
        let mgr = std::sync::Arc::new(ShellManager::new());
        echo_agent::shell::set_shell_manager_global(std::sync::Arc::clone(&mgr));
        // 事件广播：接到默认 agent 的 handle，Panel 单连接即可实时可视化。
        let shell_emitter = core_agent.clone();
        echo_agent::shell::set_shell_emit(std::sync::Arc::new(move |event: ShellEvent| {
            shell_emitter.emit(echo_agent::shell::shell_event_to_backend(event));
        }));
        info!("background shell manager ready");
    }

    // 进程级插件宿主锚点：供插件门控判定读取全局注册表
    // （与各 persona 注入的是同一实例）。
    echo_agent::agent::set_plugin_host_global(plugin_host.clone());
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
        // 启动期与运行期统一：apply_capabilities 同时承担
        // persona 白名单（工具/技能/门控插件；插件黑名单已移除）、全局
        // [agent].disabled_tools / disabled_skills、以及共享注册表里被
        // TogglePlugin 卸载的插件。
        persona.agent.apply_capabilities(&persona.profile).await;
        persona.agent.start_session_save_task();
        persona.agent.start_plugin_reload_task().await;
        // orchestration 已删除（2026-09-16）
    }

    // ---- Wire agents into QQ instances ----
    // 每个实例把入站消息投给**归属人格**（不再是"默认人格"）；实例间互相隔离。
    for (instance_id, persona_id, adapter) in &qq_adapters {
        adapter.set_config_store(config_store.clone());
        let target = match supervisor.get_exact(persona_id) {
            Some(p) => p.agent,
            None => {
                warn!(persona = %persona_id, "QQ instance persona not found; skipping wire");
                continue;
            }
        };
        // QQ events enter through a one-way hook. Outbound messages require tools.
        adapter.set_message_hook(Arc::new(echo_agent::AgentMessageHook::new(target)));
        adapter.add_handler(Box::new(handlers::EchoHandler::new(
            &cfg.bot.command_prefix,
        )));
        adapter.add_handler(Box::new(handlers::HelpHandler::new(
            &cfg.bot.command_prefix,
        )));
        let owner = adapter.get_owner_qq();
        adapter.add_handler(Box::new(handlers::AdminHandler::new(
            &cfg.bot.command_prefix,
            if owner > 0 { owner } else { cfg.bot.owner_qq },
            shutdown_tx.clone(),
            tracker.clone(),
        )));
        tracing::info!(instance = %instance_id, persona = %persona_id, "QQ instance wired to persona");
    }
    // legacy 变量（下方自动启动/停止路径仍按第一个实例语义使用）。
    if let Some((_, _, first)) = qq_adapters.first() {
        let _ = first;
    }

    // ---- Command pump（多 persona 路由）----
    // 命令从进程级通道读取（不再是"默认人格"的 mailbox）：
    // - 聊天/取消：按 team_id 路由到目标人格；
    // - 会话类（timeline/context/历史维护）：暂交默认人格处理，由处理侧按
    //   team_id 解析目标（P3 起 team_id 必填）；
    // - 其余（全局/管理类）：交给进程级核心服务代理（非人格）。
    let pump_core = core_agent.clone();
    let pump_handle = handle.clone();
    let pump = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            while let Some(cmd) = pump_handle.try_recv_command() {
                // 所有命令统一交核心服务代理；聊天/取消类由它在
                // `apply_command` 内按 team_id 路由（缺失 → 明确报错，
                // 不再有"默认人格"兜底）。
                let target = Arc::clone(&pump_core);
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
    // ---- Auto-start QQ instances if enabled ----
    // 接线完成，标记 wired：此后 adapter.qq 插件的运行期 enable 才会真正
    // start 适配器（启动期 mount 不抢跑）。启动受配置与插件状态双重门控。
    qq_wired.store(true, std::sync::atomic::Ordering::SeqCst);
    let qq_plugin_enabled = agent
        .plugin_host()
        .registry
        .is_enabled(echo_agent::plugins::ADAPTER_QQ_PLUGIN_ID);
    if cfg.qq_adapter.enabled && qq_plugin_enabled {
        // 多实例：逐个生成 compose（非 legacy）并启动；单个失败不影响其余。
        let data_dir = qq_data_dir.clone();
        for (instance_id, persona_id, adapter) in &qq_adapters {
            if let Some(instance) = qq_instances.iter().find(|i| &i.id == instance_id) {
                if instance.id != qq_instances::DEFAULT_INSTANCE {
                    if let Err(error) =
                        qq_instances::write_compose(&data_dir, &instance.id, &instance.ports)
                    {
                        warn!(instance = %instance.id, %error, "compose write failed");
                    }
                }
            }
            match adapter.start().await {
                Ok(()) => {
                    info!(instance = %instance_id, persona = %persona_id, "QQ instance started");
                }
                Err(error) => {
                    warn!(instance = %instance_id, %error, "QQ instance start failed");
                }
            }
        }
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

/// 联邦路由上下文（Phase 2）：路由泵与代理工具注册共用的句柄束。
struct FederationRuntime {
    federation: Arc<echo_federation::Federation>,
    router: Arc<echo_agent::federation::InvokeRouter>,
    /// peer 配置名 → node_id（Link Up 时登记；代理工具命名用配置名）。
    peer_names: std::sync::Arc<tokio::sync::RwLock<std::collections::HashMap<String, String>>>,
    /// 反向映射：node_id → peer 配置名（remove_peer 断链用）。
    node_to_peer: std::sync::Arc<tokio::sync::RwLock<std::collections::HashMap<String, String>>>,
    /// 联邦管理命令所需的配置写回与本机信息。
    config_store: echo_adapter::ConfigStore,
    listen: String,
    node_name: Option<String>,
    /// per-peer 执行策略（allow_tools/queries 等）——SaveFederationPeer
    /// 运行时更新即生效（此前启动期冻结会让白名单静默失效到重启）。
    peer_policies: std::sync::Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<String, echo_agent::federation::ExecutorPolicy>,
        >,
    >,
    /// 远程代理工具的注册保活句柄（Disposer drop 即注销——必须持有到
    /// 进程结束；Down 整包禁用不注销，Up 重注册前先清空旧句柄）。
    remote_tool_keepers: std::sync::Mutex<Vec<echo_context::Disposer>>,
}

impl FederationRuntime {
    /// 组装 `FederationStatus` 事件（token 脱敏为 token_set 标记）。
    async fn status_event(&self) -> echo_protocol::BackendEvent {
        let active: std::collections::HashSet<String> =
            self.federation.active_peers().await.into_iter().collect();
        let configs = self.federation.peer_configs().await;
        let mut peers = Vec::with_capacity(configs.len());
        for p in configs {
            {
                // 在线判定：dial 侧按配置名反查 node_id；连入侧（占位
                // peer / 仅接受连入条目）链路 peer_name = 对端 node_id，
                // 其 token 与该占位一致——按 token 匹配活跃链路的
                // node_id（链路表 token 不可查，退而求其次：该 peer 的
                // token 与任一活跃链路 token 匹配即在线）。
                let online = self
                    .node_to_peer
                    .try_read()
                    .map(|m| {
                        m.iter()
                            .any(|(node, name)| name == &p.name && active.contains(node))
                    })
                    .unwrap_or(false)
                    || self.federation.is_link_token_online(&p.token).await;
                // 策略字段回传运行时值（此前恒空/false——前端基于状态
                // 回写保存会抹掉已配置的白名单）。
                let policy = self.peer_policies.read().await.get(&p.name).cloned();
                peers.push(echo_protocol::FederationPeerInfo {
                    name: p.name.clone(),
                    url: p.url,
                    token: String::new(),
                    token_set: !p.token.is_empty(),
                    allow_tools: policy
                        .as_ref()
                        .map(|x| x.allow_tools.clone())
                        .unwrap_or_default(),
                    allow_subagent: policy
                        .as_ref()
                        .map(|x| x.allow_subagent)
                        .unwrap_or_default(),
                    require_confirm: policy
                        .as_ref()
                        .map(|x| x.require_confirm.clone())
                        .unwrap_or_default(),
                    allow_queries: policy
                        .as_ref()
                        .map(|x| x.allow_queries.clone())
                        .unwrap_or_default(),
                    link: if online {
                        echo_protocol::FederationLinkState::Online
                    } else {
                        echo_protocol::FederationLinkState::Offline
                    },
                });
            }
        }
        echo_protocol::BackendEvent::FederationStatus {
            enabled: true,
            node_id: self.federation.local_node_id().to_string(),
            node_name: self.node_name.clone(),
            listen: self.listen.clone(),
            peers,
        }
    }
}

/// 联邦路由泵（Phase 2）：链路事件 → InvokeRouter 派发 + 代理工具注册/摘除
/// + 执行端裁决。
///
/// - `Up`：登记 per-peer 白名单策略；按对端 caps ∩ 首批支持集（6 个无状态
///   工具）给**所有 persona** 注册 `<peer名>:<工具>` 代理（per-peer package
///   标签，插件门控可整包启停）
/// - `Down`：整包禁用该 peer 代理工具 + 使在飞调用失败
/// - `Frame`：`Invoke` 走执行端（白名单裁决 → 本机注册表执行 → 结果经
///   outbound 通道回传）；其余帧派发给出站等待表
#[allow(clippy::too_many_arguments)]
async fn federation_router_pump(
    mut rx: tokio::sync::mpsc::Receiver<echo_federation::LinkEvent>,
    rt: Arc<FederationRuntime>,
    personas: Arc<agent_supervisor::AgentSupervisor>,
    peer_policies: std::sync::Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<String, echo_agent::federation::ExecutorPolicy>,
        >,
    >,
) {
    // 执行端结果回传通道：handle_invoke spawn 的执行任务 → 本泵统一发送。
    let (outbound_tx, mut outbound_rx) =
        tokio::sync::mpsc::channel::<(String, echo_federation::FedFrame)>(64);
    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    echo_federation::LinkEvent::Up(info) => {
                        tracing::info!(
                            target: "federation",
                            peer = %info.node_id, name = %info.peer_name,
                            version = %info.version, tools = ?info.caps.tools,
                            "link up"
                        );
                        rt.peer_names
                            .write()
                            .await
                            .insert(info.peer_name.clone(), info.node_id.clone());
                        rt.node_to_peer
                            .write()
                            .await
                            .insert(info.node_id.clone(), info.peer_name.clone());
                        let policy = peer_policies
                            .read()
                            .await
                            .get(&info.peer_name)
                            .cloned()
                            .unwrap_or_default();
                        rt.router.set_policy(&info.node_id, policy);
                        // 邀请占位配对成功即清理：占位条目（invite-*）的
                        // token 与任一活跃链路匹配 → 该占位使命完成。
                        cleanup_paired_invite_placeholders(&rt).await;
                        register_remote_tools(&rt, &personas, &info).await;
                        broadcast_federation_status(&rt, &personas).await;
                    }
                    echo_federation::LinkEvent::Down { peer_node_id, reason } => {
                        tracing::info!(target: "federation", peer = %peer_node_id, %reason, "link down");
                        rt.router.drop_peer(&peer_node_id);
                        let package = peer_package(&rt, &peer_node_id).await;
                        for persona in personas.personas() {
                            persona.agent.tools.set_package_enabled(&package, false);
                        }
                        if let Some(name) = rt.node_to_peer.write().await.remove(&peer_node_id) {
                            rt.peer_names.write().await.remove(&name);
                        }
                        // 链路状态变化 → 面板联邦页实时刷新。
                        broadcast_federation_status(&rt, &personas).await;
                    }
                    echo_federation::LinkEvent::Frame { from, frame } => {
                        if let echo_federation::FedFrame::Invoke(request) = frame {
                            // 执行端：逐个 persona 注册表找工具（工具集按人格
                            // 装配略有差异；取第一个能解析到的）。
                            let registry = personas
                                .personas()
                                .into_iter()
                                .map(|p| p.agent.tools.clone())
                                .next();
                            let Some(registry) = registry else {
                                tracing::warn!(target: "federation", "no persona registry for invoke");
                                continue;
                            };
                            // 沙箱根并集：全部 persona 工作区目录（本地
                            // 目录取 path；远程目录标注占位不参战）。
                            let mut roots: Vec<std::path::PathBuf> = Vec::new();
                            for persona in personas.personas() {
                                if let Some(store) = persona.agent.workspace_store() {
                                    let (sessions, _) = store.snapshot();
                                    for s in sessions {
                                        for d in s.directories {
                                            if !d.is_remote() {
                                                roots.push(std::path::PathBuf::from(d.path()));
                                            }
                                        }
                                    }
                                }
                            }
                            let reply = rt.router.handle_invoke(
                                &from, request, registry, outbound_tx.clone(), &roots,
                            );
                            if rt.federation.send_to(&from, reply).await.is_err() {
                                tracing::warn!(target: "federation", peer = %from, "invoke reply send failed");
                            }
                        } else if let echo_federation::FedFrame::Query(req) = frame {
                            // 只读查询执行端（Phase 5）：白名单裁决 → 本机采集 →
                            // QueryResult 回传。
                            let reply = handle_federation_query(&rt, &personas, &from, req).await;
                            if rt.federation.send_to(&from, reply).await.is_err() {
                                tracing::warn!(target: "federation", peer = %from, "query reply send failed");
                            }
                        } else if let echo_federation::FedFrame::SubagentSpawn(req) = &frame {
                            // 执行端观测（Phase 3）：对端大脑委派的远程
                            // 子任务在本机开始——记审计（Panel 后台任务
                            // 列表的远程条目展示留待后续迭代）。
                            tracing::info!(
                                target: "federation",
                                from = %from, call_id = %req.call_id,
                                task = %req.task.chars().take(120).collect::<String>(),
                                "remote subagent spawned on this node"
                            );
                        } else if let echo_federation::FedFrame::SubagentEvent(ev) = &frame {
                            tracing::info!(
                                target: "federation",
                                from = %from, call_id = %ev.call_id, status = ?ev.status,
                                "remote subagent event"
                            );
                        } else {
                            rt.router.dispatch_frame(&from, &frame);
                        }
                    }
                }
            }
            // 执行端完成帧回传。
            Some((peer, frame)) = outbound_rx.recv() => {
                if rt.federation.send_to(&peer, frame).await.is_err() {
                    tracing::warn!(target: "federation", peer = %peer, "invoke result send failed");
                }
            }
        }
    }
}

/// per-peer 代理工具的 package 标签（整包启停键）。
async fn peer_package(rt: &FederationRuntime, peer_node: &str) -> String {
    let name = rt
        .node_to_peer
        .read()
        .await
        .get(peer_node)
        .cloned()
        .unwrap_or_else(|| peer_node.to_string());
    format!("echo-agent.federation.{name}")
}

/// 经进程级汇聚点广播联邦状态（任一人格的 emit 都到 Panel；用第一个
/// persona 出口，无 persona 时跳过——核心服务代理装配早于泵启动）。
async fn broadcast_federation_status(
    rt: &FederationRuntime,
    personas: &agent_supervisor::AgentSupervisor,
) {
    if let Some(persona) = personas.personas().into_iter().next() {
        persona.agent.emit(rt.status_event().await);
    }
}

/// 联邦管理命令处理（SaveFederationPeer / DeleteFederationPeer /
/// RequestFederationStatus / RequestFederationInvite）。
async fn handle_federation_command(
    emit: std::sync::Arc<dyn Fn(echo_protocol::BackendEvent) + Send + Sync>,
    cmd: echo_protocol::BackendCommand,
    rt: Arc<FederationRuntime>,
) {
    match cmd {
        echo_protocol::BackendCommand::RequestFederationStatus => {
            emit(rt.status_event().await);
        }
        echo_protocol::BackendCommand::SaveFederationPeer { peer } => {
            let name = peer.name.trim().to_string();
            if name.is_empty() {
                emit(echo_protocol::BackendEvent::Error {
                    session_id: None,
                    message: "peer 名称不能为空".into(),
                });
                return;
            }
            // 运行时生效（更新场景 token 为空 = 保留旧值）。
            let existing = rt
                .federation
                .peer_configs()
                .await
                .into_iter()
                .find(|p| p.name == name);
            let token = if peer.token.is_empty() {
                existing.map(|p| p.token).unwrap_or_default()
            } else {
                peer.token.clone()
            };
            // 空 token 校验：新建 peer 无 token 时 accept 侧
            // `!known.is_empty()` 永远拒绝、dial 侧不发头——静默无法
            // 连接且无提示（🟡 修复：直接报错）。
            if token.is_empty() {
                emit(echo_protocol::BackendEvent::Error {
                    session_id: None,
                    message: "联邦 peer token 不能为空（新建时必须提供共享密钥）".into(),
                });
                return;
            }
            rt.federation
                .add_peer(echo_federation::PeerConfig {
                    name: name.clone(),
                    url: peer.url.trim().to_string(),
                    token: token.clone(),
                })
                .await;
            // 配置原子写回。
            let (url, allow_tools, allow_subagent, require_confirm, allow_queries) = (
                peer.url.trim().to_string(),
                peer.allow_tools.clone(),
                peer.allow_subagent,
                peer.require_confirm.clone(),
                peer.allow_queries.clone(),
            );
            let write_name = name.clone();
            if let Err(e) = rt.config_store.patch(move |root| {
                let federation = echo_adapter::ensure_table(root, "federation");
                let peers = echo_adapter::ensure_table(federation, "peers");
                let entry = echo_adapter::ensure_table(peers, &write_name);
                entry.insert("url".into(), toml::Value::String(url));
                entry.insert("token".into(), toml::Value::String(token));
                entry.insert(
                    "allow_tools".into(),
                    toml::Value::Array(allow_tools.into_iter().map(toml::Value::String).collect()),
                );
                entry.insert(
                    "allow_subagent".into(),
                    toml::Value::Boolean(allow_subagent),
                );
                entry.insert(
                    "require_confirm".into(),
                    toml::Value::Array(
                        require_confirm
                            .into_iter()
                            .map(toml::Value::String)
                            .collect(),
                    ),
                );
                entry.insert(
                    "allow_queries".into(),
                    toml::Value::Array(
                        allow_queries.into_iter().map(toml::Value::String).collect(),
                    ),
                );
                Ok(())
            }) {
                emit(echo_protocol::BackendEvent::Error {
                    session_id: None,
                    message: format!("联邦 peer 配置写回失败: {e}"),
                });
                return;
            }
            // 运行时策略同步（allow_tools/queries/subagent 立即生效，无需
            // 重启）——此前仅启动装配期填充，Panel 保存的白名单静默失效。
            rt.peer_policies.write().await.insert(
                name.clone(),
                echo_agent::federation::ExecutorPolicy {
                    allow_tools: peer.allow_tools.clone(),
                    require_confirm: peer.require_confirm.clone(),
                    allow_queries: peer.allow_queries.clone(),
                    allow_subagent: peer.allow_subagent,
                },
            );
            // 链路已 Up 时立即刷新路由策略。
            if let Some(node) = rt.peer_names.read().await.get(&name).cloned() {
                let policy = rt
                    .peer_policies
                    .read()
                    .await
                    .get(&name)
                    .cloned()
                    .unwrap_or_default();
                rt.router.set_policy(&node, policy);
            }
            emit(rt.status_event().await);
        }
        echo_protocol::BackendCommand::DeleteFederationPeer { name } => {
            // 断链（node_id 反查）→ 运行时移除 → 配置移除。
            let node_id = rt.peer_names.read().await.get(&name).cloned();
            rt.federation.remove_peer(&name).await;
            if let Some(node) = node_id {
                rt.federation.drop_link(&node).await;
            }
            let write_name = name.clone();
            if let Err(e) = rt.config_store.patch(move |root| {
                if let Some(peers) = root
                    .get_mut("federation")
                    .and_then(|f| f.get_mut("peers"))
                    .and_then(|p| p.as_table_mut())
                {
                    peers.remove(&write_name);
                }
                Ok(())
            }) {
                emit(echo_protocol::BackendEvent::Error {
                    session_id: None,
                    message: format!("联邦 peer 配置移除失败: {e}"),
                });
                return;
            }
            emit(rt.status_event().await);
        }
        echo_protocol::BackendCommand::RequestFederationInvite => {
            // 邀请串 = 本机 listen 地址 + 新随机 token。token 不落配置——
            // 对端保存后由**对端**作为其 peer 条目的 token；本端需接受
            // 该 token 的连入：写入一个「仅接受连入」的 peer 条目（url 空）。
            let host = advertise_host(&rt.listen);
            let Some(host_port) = host else {
                emit(echo_protocol::BackendEvent::Error {
                    session_id: None,
                    message: "联邦 listen 未配置或不可用于邀请（需非 0.0.0.0 地址；请先在配置中写本机可达地址）".into(),
                });
                return;
            };
            let token = echo_federation::generate_token();
            let invite =
                echo_federation::encode_invite(&host_port, &token, rt.node_name.as_deref());
            // 待对端回连：占位 peer 仅注册进运行时，**不写配置**——配对
            // 成功即清理；未配对的邀请在重启后自然失效（一次性邀请语义），
            // 避免配置文件堆积垃圾占位条目。
            let placeholder = format!("invite-{}", &token[..8]);
            rt.federation
                .add_peer(echo_federation::PeerConfig {
                    name: placeholder.clone(),
                    url: String::new(),
                    token: token.clone(),
                })
                .await;
            emit(echo_protocol::BackendEvent::FederationInvite { invite });
        }
        _ => {}
    }
}

/// 只读查询执行端（Phase 5）。
///
/// - `NodeStatus`：恒允许（无害遥测）
/// - `WorkspaceFiles`：`subject` = 绝对路径；复用 workspace 插件的
///   canonical 前缀校验（**限本机各 persona 工作区目录的并集**）后
///   `collect_dir_files` 采集
/// - `SessionSnapshot`：`subject` = 会话 id（可带 `node://` 前缀——剥离
///   按本机处理）；复用 trunk 时间线快照 + since_seq/limit 分页
async fn handle_federation_query(
    rt: &FederationRuntime,
    personas: &agent_supervisor::AgentSupervisor,
    from: &str,
    req: echo_federation::QueryRequest,
) -> echo_federation::FedFrame {
    use echo_federation::{FedFrame, QueryKind, QueryResultFrame};
    let call_id = req.call_id.clone();
    let ok = |payload: serde_json::Value| {
        FedFrame::QueryResult(QueryResultFrame {
            call_id: call_id.clone(),
            success: true,
            payload,
        })
    };
    let err = |message: &str| {
        FedFrame::QueryResult(QueryResultFrame {
            call_id: call_id.clone(),
            success: false,
            payload: serde_json::json!({"error": message}),
        })
    };
    // 授权（无配置 peer = 仅 NodeStatus）
    let allowed = rt
        .router
        .policy_of(from)
        .map(|p| p.query_allowed(req.kind))
        .unwrap_or(matches!(req.kind, QueryKind::NodeStatus));
    if !allowed {
        tracing::info!(target: "federation", from = %from, kind = ?req.kind, "query rejected by policy");
        return err("该查询种类未被对端白名单允许");
    }
    match req.kind {
        QueryKind::NodeStatus => {
            let agents = personas.personas();
            let active_turns: usize = agents
                .iter()
                .map(|p| p.agent.active_turn_session_ids().len())
                .sum();
            ok(serde_json::json!({
                "node_id": rt.federation.local_node_id(),
                "version": env!("CARGO_PKG_VERSION"),
                "peers_online": rt.federation.active_peers().await.len(),
                "personas": agents.len(),
                "active_turns": active_turns,
            }))
        }
        QueryKind::WorkspaceFiles => {
            // 本机全部 persona 工作区目录并集内的 canonical 校验。
            let dir = req.subject.trim().to_string();
            let mut all_dirs: Vec<echo_protocol::WorkspaceDirectory> = Vec::new();
            for persona in personas.personas() {
                if let Some(store) = persona.agent.workspace_store() {
                    let (sessions, _) = store.snapshot();
                    for s in sessions {
                        all_dirs.extend(s.directories);
                    }
                }
            }
            let collected = tokio::task::spawn_blocking(move || {
                let resolved = echo_agent::workspace::resolve_within_directories(&all_dirs, &dir)?;
                echo_agent::workspace::collect_dir_files(&resolved)
            })
            .await;
            match collected {
                Ok(Ok(entries)) => ok(serde_json::json!({"entries": entries})),
                Ok(Err(message)) => err(&message),
                Err(e) => err(&format!("采集失败: {e}")),
            }
        }
        QueryKind::SessionSnapshot => {
            // 会话归属解析：`node://` 前缀剥离按本机处理；team 维度经
            // manager 逐 persona 查找。
            let (_, key) = echo_defs::NodeId::split_ref(&req.subject);
            let session_id = key;
            for persona in personas.personas() {
                let agent = &persona.agent;
                if let Some(session) = agent.trunk.get(session_id) {
                    let (messages, seq) = if req.since_seq > 0 {
                        match agent.trunk.timeline_snapshot_since(req.since_seq) {
                            Some(snap) => snap,
                            None => match agent.trunk.timeline_snapshot() {
                                Some(m) => (m, agent.trunk.timeline_seq()),
                                // 快照降级（锁竞争超时）：回错误让对端重试，
                                // 不发空数据（防止对端误当空全量）。
                                None => return err("timeline snapshot busy, retry later"),
                            },
                        }
                    } else {
                        match agent.trunk.timeline_snapshot() {
                            Some(m) => (m, agent.trunk.timeline_seq()),
                            None => return err("timeline snapshot busy, retry later"),
                        }
                    };
                    let limit = if req.limit == 0 {
                        50
                    } else {
                        req.limit as usize
                    };
                    let mut messages = messages;
                    if messages.len() > limit {
                        messages = messages.split_off(messages.len() - limit);
                    }
                    return ok(serde_json::json!({
                        "session_id": session.id,
                        "team_id": persona.id,
                        "seq": seq,
                        "messages": messages,
                    }));
                }
            }
            err(&format!("会话 {session_id} 不存在"))
        }
    }
}

/// 配对成功的邀请占位清理：`invite-*` 占位 peer 的 token 已有活跃链路
/// （= 对端已用该邀请串连入）→ 运行时移除（占位本就不落配置，一次性
/// 邀请语义）。
async fn cleanup_paired_invite_placeholders(rt: &FederationRuntime) {
    let configs = rt.federation.peer_configs().await;
    // 配对判定：占位 token 已有活跃链路，**且**链路对端的 node_id 已知
    // （连入握手完成）——仅凭 token 在线会误清「同一 token 被多个占位复用」
    // 场景；此处 token 是一次性的，token 在线即配对成功。
    let mut paired: Vec<String> = Vec::new();
    for p in &configs {
        if !p.name.starts_with("invite-") {
            continue;
        }
        if rt.federation.is_link_token_online(&p.token).await {
            paired.push(p.name.clone());
        }
    }
    for name in paired {
        rt.federation.remove_peer(&name).await;
        tracing::info!(target: "federation", %name, "invite placeholder paired and removed");
    }
}

/// 邀请用的对外地址：listen 为具体 IP/主机名时直接用；`0.0.0.0` 不可取
/// 首个非回环 IPv4（无则 None 提示手工配置）。
fn advertise_host(listen: &str) -> Option<String> {
    let (host, port) = listen.rsplit_once(':')?;
    if host != "0.0.0.0" && host != "[::]" && !host.is_empty() {
        return Some(format!("{host}:{port}"));
    }
    // 枚举本机非回环 IPv4（无第三方依赖：读 /proc/net/fib_trie 复杂，
    // 用 `hostname -I` 最稳）。
    let output = std::process::Command::new("hostname")
        .arg("-I")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let ip = text
        .split_whitespace()
        .find(|s| s.parse::<std::net::Ipv4Addr>().is_ok())?;
    Some(format!("{ip}:{port}"))
}

/// 按对端 caps 给全部 persona 注册代理工具（本机 schema 复制；注册即
/// 启用——包级禁用只在 Down 时发生，重连 Up 负责整包重新启用）。
async fn register_remote_tools(
    rt: &FederationRuntime,
    personas: &agent_supervisor::AgentSupervisor,
    info: &echo_federation::PeerInfo,
) {
    let candidates = echo_agent::federation::remote_tool_candidates(&info.caps);
    if candidates.is_empty() {
        return;
    }
    let package = format!("echo-agent.federation.{}", info.peer_name);
    let mut registered = 0usize;
    let mut keepers: Vec<echo_context::Disposer> = Vec::new();
    for persona in personas.personas() {
        let definitions = persona.agent.tools.definitions().await;
        for remote_name in &candidates {
            let (description, parameters) =
                echo_agent::federation::proxy_schema(&definitions, remote_name, &info.peer_name);
            let tool = std::sync::Arc::new(echo_agent::federation::RemoteTool::new(
                &info.peer_name,
                &info.node_id,
                remote_name,
                description,
                parameters,
                rt.federation.clone(),
                rt.router.clone(),
            ));
            let disposer = persona.agent.tools.register_reversible(tool);
            keepers.push(disposer); // 保活：drop 即注销，绝不能当临时值
            persona
                .agent
                .tools
                .set_package(&format!("{}:{remote_name}", info.peer_name), &package);
        }
        registered = candidates.len();
        // 重连语义：Down 时整包禁用，Up 必须整包重新启用（首次注册时
        // 包不在禁用集，本调用无害幂等）。
        persona.agent.tools.set_package_enabled(&package, true);
    }
    // 保活句柄入库：先清旧（重注册场景），再存新。
    {
        let mut slot = rt.remote_tool_keepers.lock().expect("keepers poisoned");
        slot.clear();
        slot.extend(keepers);
    }
    tracing::info!(
        target: "federation",
        peer = %info.node_id, package = %package, tools = registered,
        "remote proxy tools registered"
    );
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
