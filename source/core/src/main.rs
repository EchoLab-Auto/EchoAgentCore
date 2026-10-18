//! EchoAgentCore — agent core service.
//!
//! 运行 Agent 核心：LLM agent 循环、平台适配器（独立进程插件）、
//! 以及供 Panel 等前端连接的 management WebSocket（默认 :3132）。
//! 前端（TUI）在独立的 EchoAgentPanel 仓库中，通过 `echo-protocol` 定义的
//! WS/JSON 协议与本服务通信。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod agent_supervisor;
mod config;
mod federation_import;
mod federation_rt;
mod instance_deploy;
mod instance_manifests;
mod instance_tools;
mod logging;
mod management;
mod node;
mod plugins_runtime;
mod port_cmd;
mod security;

use config::CoreConfig;
use echo_agent::{Agent, AgentProfile};
use echo_defs::plugin_instance::PluginInstance;
#[allow(unused_imports)]
use federation_rt::*;
use logging::init_tracing;

/// EchoAgentCore — agent core service (agent + instances + management WS).
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Path to the TOML configuration file.
    /// Default: config/echo-agent-core.toml
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// 管理子命令（缺省 = 启动服务）。
    #[command(subcommand)]
    command: Option<SubCommand>,
}

#[derive(Debug, clap::Subcommand)]
enum SubCommand {
    /// 打印当前 management_access_token（Panel 连接 Core 的 Bearer
    /// token；--rotate 重新生成并写回，下次重启生效）。
    Token {
        /// 重新生成随机 token（32 字节 hex）并写回配置文件。
        #[arg(long)]
        rotate: bool,
    },
    /// 查看 / 修改各监听端口（写回配置文件，重启生效）。
    /// 不带子命令 = 列出全部端口。
    Port {
        #[command(subcommand)]
        action: Option<port_cmd::PortSubCommand>,
    },
}

impl Args {
    fn config_path(&self) -> PathBuf {
        self.config
            .clone()
            .unwrap_or_else(|| PathBuf::from("config/echo-agent-core.toml"))
    }
}

/// `echo-agent-core token [--rotate]`：读/生成 management_access_token。
/// 直接操作 TOML 文本（保留注释与其他段）——不加载完整配置。
fn token_command(config_path: &std::path::Path, rotate: bool) -> Result<()> {
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read config {}", config_path.display()))?;
    let current = text
        .lines()
        .find_map(|line| {
            line.trim_start()
                .strip_prefix("management_access_token")
                .and_then(|rest| rest.trim_start_matches(['=', ' ']).trim().strip_prefix('"'))
                .and_then(|v| v.strip_suffix('"'))
                .map(str::to_string)
        })
        .unwrap_or_default();
    if !rotate {
        if current.is_empty() {
            println!("# management_access_token 为空（无认证——仅本机回环监听场景安全）");
        } else {
            println!("{current}");
        }
        return Ok(());
    }
    // --rotate：生成 32 字节随机 hex 并写回（原地替换该行；行不存在则
    // 追加到 [core] 段尾）。
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).context("generate random token")?;
    let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let new_line = format!("management_access_token = \"{token}\"");
    let mut out = String::with_capacity(text.len() + 96);
    let mut replaced = false;
    let mut in_core = false;
    let mut appended = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            if in_core && !replaced && !appended {
                out.push_str(&new_line);
                out.push('\n');
                appended = true;
            }
            in_core = trimmed == "[core]";
        }
        if in_core && trimmed.starts_with("management_access_token") {
            out.push_str(&new_line);
            replaced = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !replaced && !appended {
        if !in_core {
            out.push_str("\n[core]\n");
        }
        out.push_str(&new_line);
        out.push('\n');
    }
    // tmp+rename 原子写（与配置写回其他路径同约定）。
    let tmp = config_path.with_extension("toml.token-tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, config_path)?;
    println!("{token}");
    eprintln!(
        "# 已写回 {}——下次重启 echo-agent-core 生效",
        config_path.display()
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();
    if let Some(command) = args.command.take() {
        match command {
            SubCommand::Token { rotate } => return token_command(&args.config_path(), rotate),
            SubCommand::Port { action } => return port_cmd::run(&args.config_path(), action),
        }
    }
    let path = args.config_path();
    let cfg = CoreConfig::load(&path)?;
    run_core(args, cfg).await
}

// ── Core：Agent + 适配器 + management WS 服务 ──────────────────────────────

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
    // 「运行区域」名：显式 [core].region_name → [federation].node_name →
    // 主机名 → NodeId 短码。区域 id 恒为 NodeId（稳定）；名字仅用于展示与
    // Panel 的 agent 分组/放置。
    let region_name = {
        let configured = cfg.core.region_name.trim();
        if !configured.is_empty() {
            configured.to_string()
        } else if let Some(name) = cfg
            .federation
            .node_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            name.to_string()
        } else {
            std::env::var("HOSTNAME")
                .ok()
                .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| node_doc.node_id.chars().take(14).collect())
        }
    };
    info!(node_id = %node_doc.node_id, region = %region_name, "node identity ready");
    // 注入进程级区域身份：SessionInfo/TeamInfo 等线格式携带它（agent 的
    // 「运行区域」属性），供多节点聚合客户端区分同名会话/人格。
    echo_agent::set_region(node_doc.node_id.clone(), region_name);

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // ── 端口合并判定（2026-10；「缺省合并」——联邦默认不再有独立端口）──
    // 缺省（未显式配置 [federation].listen）：联邦监听自动跟随管理地址
    // （单监听按令牌命名空间分流；见 federation-port-merge.md）。保守回退
    // 独立监听：管理地址回环 / 空令牌且非回环 / 令牌与 peer 冲突（供回退
    // 判定的 peer 闭包用配置表——启动期与 fed 运行时表同源）。显式配置
    // listen = 按显式值（同址 = 合并；空串 = 纯连出）。
    let (merged, fed_listen_effective, merged_fallback) = management::plan_federation_listen(
        &cfg.core.management_address,
        &cfg.core.management_access_token,
        &cfg.federation.listen,
        cfg.federation.listen_explicit,
        |t| cfg.federation.peers.values().any(|p| p.token == t),
    );
    if let Some(reason) = merged_fallback {
        warn!("联邦缺省合并已回退为独立监听：{reason}");
    }

    // ---- federation link（工具路由 / 远程委派 / 只读查询均已接线）----
    // 联邦**无独立开关**：零配置**缺省合并至管理地址**（保守场景回退
    // `0.0.0.0:3133`，见 plan_federation_listen；显式 `listen = ""` 才
    // 退回纯连出）；无 peer 则不连出、且 accept 侧靠 per-peer token 认证
    // 不放行任何已知链路；联邦管理面永远可用。
    let federation = {
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
                // shell 三件套与进程级 ShellManager 耦合深，远程化留待后续
                // ——不宣告（对端按 caps 预期会落空）。
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            subagent: true,
            workspaces: Vec::new(),
            // 启动时无活跃 turn；调度器应经 Query(NodeStatus) 拉实时值。
            active_turns: 0,
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
        // 本机对外可拨地址（ws://host:port）：与邀请串同源推导
        // （listen/advertise）。握手 Hello 携带——对端学习后双方都能
        // 主动重连（配对无方向性，2026-10）。
        // 合并时监听地址即管理地址（advertise 推导与邀请串同源）。
        let listen_for_advertise = fed_listen_effective.as_deref().unwrap_or("");
        let local_advertise = invite_host(listen_for_advertise, &cfg.federation.advertise)
            .map(|host_port| format!("ws://{host_port}"));
        let fed = echo_federation::Federation::new(
            node_doc.node_id.clone(),
            cfg.federation
                .node_name
                .clone()
                .or(node_doc.node_name.clone()),
            caps,
            peers,
            local_advertise,
            event_tx,
            shutdown_tx.clone(),
        );
        // ── 端口合并安全校验（判定在块外；fail-closed，非法拒绝启动）──
        if merged {
            management::validate_merged(
                cfg.core.management_address.trim(),
                &cfg.core.management_access_token,
                |t| fed.has_peer_token_sync(t),
            )?;
            if cfg
                .agent
                .disabled_plugins
                .iter()
                .any(|p| p == echo_agent::plugins::MANAGEMENT_PANEL_PLUGIN_ID)
            {
                warn!(
                    "端口合并已生效（联邦与管理面共用监听端口），但管理面插件被禁用——\
                     管理通道与联邦入站均不会启动（仅出站联邦继续工作）"
                );
            }
        }
        // 合并模式下联邦不自行监听（入站挂在管理面监听端）；独立模式按
        // 规划值监听；纯连出（显式空串）为 None。
        let listen = if merged {
            None
        } else {
            fed_listen_effective.clone()
        };
        let fed_runner = fed.clone();
        tokio::spawn(async move {
            if let Err(e) = fed_runner.run(listen).await {
                // 独立监听退出（bind 失败如端口占用、或运行期错误）＝入站
                // 静默失效；此前仅 warn（2026-10 审计 P1-5b），升级为
                // error 并给出排查线索。
                tracing::error!(
                    error = %e,
                    "federation listener exited — inbound links will NOT be accepted;                      check the listen port is free (ss -tlnp) and not used by another Core"
                );
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
            config_store: echo_config::ConfigStore::new(args.config_path()),
            listen: fed_listen_effective.clone().unwrap_or_default(),
            advertise: cfg.federation.advertise.clone(),
            node_name: cfg
                .federation
                .node_name
                .clone()
                .or(node_doc.node_name.clone()),
            peer_policies: peer_policies.clone(),
            remote_tool_keepers: std::sync::Mutex::new(std::collections::HashMap::new()),
            merged_management_token: if merged {
                Some(cfg.core.management_access_token.clone())
            } else {
                None
            },
            invite_placeholders: std::sync::Mutex::new(std::collections::HashMap::new()),
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
                        let node_id = rt.resolve_peer_node(&peer).await;
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
        // 远程文件调用出口（P3-1 node:// 分流）：peer 名 → node_id 反查
        // → InvokeRouter::invoke + Federation::send_to → 等终态。
        {
            let rt_invoke = rt.clone();
            // 远程只读查询出口（跨机工作区 git/文件浏览用）：与
            // remote_invoker 同生命周期（peer 离线即失败，fail-closed）。
            {
                let rt_query = rt.clone();
                echo_agent::federation::set_remote_querier(std::sync::Arc::new(
                    move |peer, kind, subject| {
                        let rt = rt_query.clone();
                        Box::pin(async move {
                            let Some(node_id) = rt.resolve_peer_node(&peer).await else {
                                return Err(format!(
                                    "远程节点「{peer}」不在线或不存在（跨机工作区查询）"
                                ));
                            };
                            let (request, pending) =
                                rt.router
                                    .query(&node_id, kind, subject, String::new(), 0, 0);
                            let call_id = pending.call_id.clone();
                            if rt
                                .federation
                                .send_to(&node_id, echo_federation::FedFrame::Query(request))
                                .await
                                .is_err()
                            {
                                // 显式终结等待条目（2026-10 审计 P1-5）：
                                // 此前仅返回错误，pending 滞留至 sweep/超时。
                                rt.router.fail_pending(&call_id, "联邦链路发送失败");
                                return Err(format!("远程节点「{peer}」链路发送失败"));
                            }
                            let result = tokio::time::timeout(
                                std::time::Duration::from_secs(30),
                                pending.wait(),
                            )
                            .await
                            .map_err(|_| format!("远程节点「{peer}」查询超时"))??;
                            if !result.success {
                                return Err(result.output);
                            }
                            serde_json::from_str(&result.output)
                                .map_err(|e| format!("远程查询结果解析失败: {e}"))
                        })
                    },
                ));
            }
            echo_agent::federation::set_remote_invoker(std::sync::Arc::new(
                move |peer, tool, args| {
                    let rt = rt_invoke.clone();
                    Box::pin(async move {
                        let Some(node_id) = rt.resolve_peer_node(&peer).await else {
                            return Err(format!(
                                "远程节点「{peer}」不在线或不存在（node:// 分流）"
                            ));
                        };
                        let (request, pending) = rt.router.invoke(
                            &node_id,
                            &tool,
                            args,
                            Some(std::time::Duration::from_secs(120)),
                        );
                        let call_id = pending.call_id.clone();
                        if rt
                            .federation
                            .send_to(&node_id, echo_federation::FedFrame::Invoke(request))
                            .await
                            .is_err()
                        {
                            // 显式终结等待条目（2026-10 审计 P1-5；与
                            // remote_tool.rs 的同类处理对齐）。
                            rt.router.fail_pending(&call_id, "联邦链路发送失败");
                            return Err(format!("远程节点「{peer}」链路不可用"));
                        }
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(180),
                            pending.wait(),
                        )
                        .await
                        {
                            Ok(Ok(result)) if result.success => Ok(result.output),
                            Ok(Ok(result)) => Err(result.output),
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err(format!("远程节点「{peer}」响应超时")),
                        }
                    })
                },
            ));
        }
        (rt, event_rx, peer_policies)
    };
    info!("federation link configured (no switch; zero peers = passive listen)");
    // 端口合并用联邦句柄（rt 随后被路由泵消费；此处预克隆——仅合并
    // 模式下由管理面插件消费）。
    let merge_fed = federation.0.federation.clone();
    // 联邦 / 自更新命令处理器（联邦始终开启，无需条件注册）。
    {
        let rt_handler = Arc::clone(&federation.0);
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
    // Keep one sender alive for the whole `run_core` scope. The admin handler
    // Keep one sender alive for the whole `run_core` scope. The admin handler
    // (the only other sender) is stored inside the instance plugin's handler
    // registry; if that registry is dropped during an instance restart,
    // `shutdown_rx.changed()` would otherwise complete with a `RecvError` and
    // the select below would misinterpret it as an admin shutdown.
    let _shutdown_tx_guard = shutdown_tx.clone();

    // ---- agent framework ----
    // 技能分层（2026-10）：出厂层（skills_dirs，只读）在前、用户层
    // （skills_dir，读写）在后——同名技能用户层覆盖出厂版本。
    let mut skill_dirs = cfg.agent.skills_dirs.clone();
    skill_dirs.push(cfg.agent.skills_dir.clone());
    let mut skills = echo_agent::SkillRegistry::discover_many(&skill_dirs)
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
    // 指标归属：base provider 对应生效配置名（与 QueryApiBalance /
    // QueryApiMetrics 的命名空间一致；fallback 分支 = 首个 profile 名）。
    let mut base_profile_key = provider_cfg.active_api.clone();
    // Fallback: if top-level provider is still empty but profiles exist, use the first one.
    if provider_cfg.provider.is_empty() {
        if let Some(first) = provider_cfg.api_profiles.first() {
            provider_cfg.provider = first.provider.clone();
            provider_cfg.model = first.model.clone();
            provider_cfg.base_url = first.base_url.clone();
            provider_cfg.api_key = first.api_key.clone();
            base_profile_key = first.name.clone();
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

    // ---- 敏感信息脱敏（sanitize 服务，2026-10）----
    // 先于 provider 构建：provider 以脱敏装饰器包装后分发给全部下游
    //（LLM 请求出口卡口）；同时把脱敏器注册进服务定位器（"sanitizer"），
    // 供进程内插件与消费者解析。关闭需编辑 core.toml + 重启（无运行期开关）。
    let redactor: Option<Arc<dyn echo_defs::sanitize::Redactor>> = if cfg.security.sanitize.enabled
    {
        Some(crate::security::build_redactor(&cfg))
    } else {
        warn!(
                "security.sanitize.enabled=false — 敏感信息脱敏已关闭（不推荐；编辑 core.toml 后重启生效）"
            );
        None
    };
    // 敏感目录（适配器外发文件闸门用；core.toml / 会话 / 凭据目录）。
    let sensitive_dirs = crate::security::sensitive_dirs(&args.config_path());

    // ---- API 指标存储（余额快照 / token 用量；面板图表数据源）----
    // 文件与 core.toml 同目录：echo-balances.jsonl / echo-usage.jsonl。
    let metrics_store = std::sync::Arc::new(echo_agent::metrics::MetricsStore::new(
        args.config_path()
            .parent()
            .map(|path| path.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from(".")),
    ));

    let provider = echo_agent::llm::create_provider(&provider_cfg)?;
    info!(provider = %provider_cfg.provider, model = %provider_cfg.model, "llm provider ready");

    // ---- Service context (dsh-style service locator) ----
    // Core services are registered under stable keys; extension plugins and
    // consumers resolve by key instead of importing concrete providers.
    let ctx: Arc<echo_context::Ctx> = Arc::new(echo_context::Ctx::default());
    // LLM 出口装饰：先包计量（token 用量记录），再包脱敏（请求出口卡口），
    // 之后才分发给一切下游（runner / agent / subagent）。
    let base_provider: Arc<dyn echo_agent::LlmProvider> = Arc::from(provider);
    let base_provider =
        echo_agent::llm::wrap_metering(base_provider, metrics_store.clone(), base_profile_key);
    let provider_arc: Arc<dyn echo_agent::LlmProvider> = match &redactor {
        Some(redactor) => echo_agent::llm::wrap_redacting(base_provider, redactor.clone()),
        None => base_provider,
    };
    let _ctx_keep = ctx.register::<Arc<dyn echo_agent::LlmProvider>>("llm", provider_arc.clone());
    // 脱敏服务（"sanitizer" 键；进程内插件经 MountContext.ctx 解析）。
    let _sanitizer_keep: Option<echo_context::Disposer> = redactor.as_ref().map(|redactor| {
        ctx.register::<Arc<dyn echo_defs::sanitize::Redactor>>("sanitizer", redactor.clone())
    });

    // The default turn runner (dsh: the loop is a swappable plugin, resolved
    // by key). Register it now; consumers (the agent driver) resolve it.
    let runner: Arc<echo_loop::TurnRunner> = Arc::new(echo_loop::TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider_arc.clone(),
        Arc::new(echo_loop::ToolPipeline::new()),
        echo_loop::LoopOptions {
            max_tool_iterations: cfg.agent.max_tool_iterations.max(1),
            max_tokens: cfg.agent.effective_max_tokens(),
        },
    ));
    let _runner_keep = ctx.register::<Arc<echo_loop::TurnRunner>>("loop", runner.clone());

    // PluginInstance registry.
    let instance_registry = echo_defs::plugin_instance::InstanceRegistry::new();

    // ---- 适配器实例（多实例；实例 = 适配器名 = 会话 account 维度）----
    // persona 门控：启用了 `echo-agent.instance.qq` 的人格各自获得实例
    // （未配置实例时自动建档），legacy 单实例部署行为不变。
    let qq_enabled_personas: Vec<String> = cfg
        .agent
        .teams
        .iter()
        .filter(|(_, m)| {
            m.enabled
                && echo_agent::plugins::profile_allows_plugin(
                    m,
                    crate::instance_manifests::ADAPTER_QQ_PLUGIN_ID,
                )
        })
        .map(|(id, _)| id.clone())
        .collect();
    let qq_default_persona = qq_enabled_personas
        .first()
        .cloned()
        .unwrap_or_else(|| cfg.agent.teams.keys().next().cloned().unwrap_or_default());
    // 数据目录：实例 compose / 卷命名都从这里派生（配置同目录）。
    // 配置存储（实例端口持久化、Agent 配置写回等都依赖它）。
    let config_store = echo_config::ConfigStore::new(args.config_path());
    let qq_data_dir = args
        .config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    // 本仓库当前只**编排** QQ 这一种平台的实例（容器化部署知识仍在
    // `qq_instances`，属后续阶段）；平台配置本身已泛化到 `cfg.instances`，
    // 新增平台无需改这里也能读到配置。
    // `platform` = 当前被**容器编排**的平台。容器部署知识（`napcat`）目前只覆盖
    // QQ，因此这里只取它；配置层早已平台中立（`cfg.instances` 是任意平台名的
    // 映射），多平台编排是后续阶段的事——但下面的插件 id / 状态文件 / 命令名
    // 一律由 `platform` 派生，不再写死字符串。
    let platform = "qq";
    let qq_host = cfg.instances.get(platform).cloned().unwrap_or_default();
    let qq_instances = instance_deploy::resolve_instances_in(
        &qq_host,
        &qq_host.instances,
        &qq_enabled_personas,
        &qq_default_persona,
        Some(&qq_data_dir),
    );
    // 适配器仅以独立进程插件运行（P1.9，2026-10 适配器插件化收尾——
    // 内置路径已移除；`builtin` 模式配置在加载期告警并按 plugin 处理）。
    let mut qq_handles: Vec<(
        String,
        String,
        Arc<dyn echo_defs::plugin_instance::PluginInstance>,
    )> = Vec::new();
    info!(
        instances = qq_instances.len(),
        "adapter instances launch as plugins"
    );
    // 插件实例 specs：每实例一条 `LaunchSpec`——`Hello.config` 携带实例身份 /
    // 归属人格 / 运行期状态文件（门控 / 名单 / owner 落盘回灌）与完整适配器
    // 配置（共享层 ⊕ 实例覆盖 + 端口分配结果；TOML 表透传）。
    let qq_plugin_specs: Vec<echo_plugin_host::bootstrap::LaunchSpec> = {
        let launch = &qq_host.launch;
        let command = if launch.command.trim().is_empty() {
            // 约定：插件可执行文件名为 `echo-instance-<platform>`
            format!("echo-adapter-{platform}")
        } else {
            launch.command.trim().to_string()
        };
        qq_instances
            .iter()
            .map(|instance| {
                let state_file = qq_data_dir.join(format!("{platform}-{}.toml", instance.id));
                echo_plugin_host::bootstrap::LaunchSpec {
                    plugin_id: format!("adapter-{platform}-{}", instance.id),
                    command: command.clone(),
                    args: launch.args.clone(),
                    env: launch.env.clone(),
                    config: serde_json::json!({
                        "name": instance.id,
                        "persona": instance.persona,
                        "state_file": state_file.to_string_lossy(),
                        "config": instance.config,
                    }),
                }
            })
            .collect()
    };
    // 实例持久化：新分配的**归属人格与端口**写回
    // `[instances.qq.instances.<id>]`（persona + ports），保证跨重启稳定：
    // - 端口：容器端口映射必须稳定，适配器容器才能重连到同一处；
    // - persona：自动建档实例若只写端口，重启后 persona 缺失会被重新
    //   分配给默认人格（首个启用适配器插件的人格），实例归属漂移。
    {
        struct InstancePatch {
            id: String,
            persona: String,
            ports: instance_deploy::QqPorts,
            /// persona 是否需要写回（已有显式 persona 的实例跳过该字段）。
            write_persona: bool,
        }
        let mut patches: Vec<InstancePatch> = Vec::new();
        for instance in &qq_instances {
            if instance.id == instance_deploy::DEFAULT_INSTANCE {
                continue; // legacy 实例沿用既定端口与默认归属，不写配置
            }
            let section = qq_host.instances.get(&instance.id);
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
                let instances = echo_config::ensure_table(root, "adapters");
                let qq = echo_config::ensure_table(instances, platform);
                let instances = echo_config::ensure_table(qq, "instances");
                for patch in &patches {
                    let entry = echo_config::ensure_table(instances, &patch.id);
                    let value =
                        toml::Value::try_from(&patch.ports).map_err(|e| format!("ports: {e}"))?;
                    entry.insert("ports".into(), value);
                    if patch.write_persona {
                        entry.insert("persona".into(), toml::Value::String(patch.persona.clone()));
                    }
                }
                Ok(())
            }) {
                warn!(%error, "adapter instance persist failed");
            } else {
                let ids: Vec<&str> = patches.iter().map(|patch| patch.id.as_str()).collect();
                info!(instances = ?ids, "adapter instance persona/ports allocated and persisted");
            }
        }
    }

    let instances = Arc::new(instance_registry);

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

    // ── 子进程插件（解耦计划 P3/P4）：plugins.toml 缺省休眠 ──
    // P2：插件启动前注册宿主服务——子进程插件经 HostCall 回呼
    // `"sanitizer"`（redact / scan / register_secret），与进程内插件
    // 共用同一脱敏器实例。
    let plugin_runtime = plugins_runtime::start_with_specs(
        &args.config_path(),
        |services| {
            if let Some(redactor) = &redactor {
                services.register(
                    "sanitizer",
                    std::sync::Arc::new(crate::security::SanitizerHostService::new(
                        redactor.clone(),
                    )),
                );
            }
        },
        qq_plugin_specs,
    )
    .await;
    // P1（2026-10 适配器插件化）：把插件的适配器贡献注册进
    // `InstanceRegistry`（与内置适配器同册）。
    let plugin_instances: Vec<plugins_runtime::RemoteInstanceHandle> = plugin_runtime
        .as_ref()
        .map(|r| r.instances())
        .unwrap_or_default();
    // plugin 模式的适配器实例：远程适配器融入统一实例表（工具注册 /
    // 自启停 / 默认句柄共用），入站队列（`inbound_rx`）留给 wiring 段的
    // 串行 worker（IncomingMessage → AgentMessageHook）。
    let mut qq_plugin_workers: Vec<(String, String, plugins_runtime::RemoteInstanceHandle)> =
        Vec::new();
    for handle in plugin_instances {
        let name = handle.instance.name.clone();
        let platform = handle.instance.platform().to_string();
        instances.register(handle.instance.clone());
        tracing::info!(
            instance = %name,
            platform = %platform,
            "plugin adapter registered"
        );
        // 实例匹配放宽（泛化 S6 / 01b §D.4）：命中实例表 → 用其归属人格；
        // 未命中（其他平台的适配器插件 / 未登记实例）→ 默认人格，仍注册
        // 入站 worker 与消息工具，不再跳过。
        let persona = match qq_instances.iter().find(|instance| instance.id == name) {
            Some(instance) => instance.persona.clone(),
            None => {
                tracing::info!(
                    instance = %name,
                    persona = %qq_default_persona,
                    "plugin adapter has no instance entry; wired to the default persona"
                );
                qq_default_persona.clone()
            }
        };
        qq_handles.push((
            name.clone(),
            persona.clone(),
            handle.instance.clone() as Arc<dyn echo_defs::plugin_instance::PluginInstance>,
        ));
        qq_plugin_workers.push((name, persona, handle));
    }
    // 进程级默认句柄：第一个实例（通常是 `qq`；自动启动/停机与登录兜底用）；
    // 无实例时 `None`（不再有内置空壳）。
    let qq_instance: Option<Arc<dyn echo_defs::plugin_instance::PluginInstance>> =
        qq_handles.first().map(|(_, _, instance)| instance.clone());
    // persona → 该人格的适配器实例集合（工具注册按人格隔离；实例统一表）。
    let instances_by_persona: std::collections::HashMap<
        String,
        Vec<Arc<dyn echo_defs::plugin_instance::PluginInstance>>,
    > = {
        let mut map: std::collections::HashMap<
            String,
            Vec<Arc<dyn echo_defs::plugin_instance::PluginInstance>>,
        > = std::collections::HashMap::new();
        for (_, persona, instance) in &qq_handles {
            map.entry(persona.clone())
                .or_default()
                .push(instance.clone());
        }
        map
    };
    let plugin_tools: std::sync::Arc<Vec<std::sync::Arc<echo_plugin_host::RemoteTool>>> =
        std::sync::Arc::new(
            plugin_runtime
                .as_ref()
                .map(|r| r.tools())
                .unwrap_or_default(),
        );
    let factory_plugin_tools = plugin_tools.clone();

    // 多人格装配闭包：persona id + profile -> 独立 Agent 实例。
    // 用 Arc 包一层：supervisor 与"进程级核心服务代理"共用同一装配逻辑；
    // 装配所需的共享件预先 clone 好，闭包按需再 clone（Fn 语义）。
    let factory_provider = provider_arc.clone();
    let factory_skills = skills.clone();
    let factory_instances = instances.clone();
    let factory_default_instances: Vec<Arc<dyn echo_defs::plugin_instance::PluginInstance>> =
        qq_instance.iter().cloned().collect();
    let factory_instances_by_persona = instances_by_persona.clone();
    let factory_base_cfg = agent_config.clone();
    let factory_event_sink = event_sink.clone();
    let factory_plugin_host = plugin_host.clone();
    let factory_config_store = config_store.clone();
    let factory_redactor = redactor.clone();
    let factory_metrics = metrics_store.clone();
    let factory_sensitive_dirs = sensitive_dirs.clone();
    let factory_outbound_policy =
        crate::security::OutboundPolicy::from_config(&cfg.security.sanitize.outbound);
    let factory_config_path = args.config_path();
    let make_agent: Arc<dyn Fn(String, AgentProfile) -> Arc<Agent> + Send + Sync> = Arc::new(
        move |id: String, profile: AgentProfile| -> Arc<Agent> {
            let provider_arc2 = factory_provider.clone();
            let skills2 = factory_skills.clone();
            let instances2 = factory_instances.clone();
            let default_instances = factory_default_instances.clone();
            let base_cfg = factory_base_cfg.clone();
            let event_sink = factory_event_sink.clone();
            let shared_plugin_host = factory_plugin_host.clone();
            let redactor2 = factory_redactor.clone();
            let metrics2 = factory_metrics.clone();
            let sensitive_dirs2 = factory_sensitive_dirs.clone();
            let outbound_policy = factory_outbound_policy;
            let config_store_path = factory_config_path.clone();
            let agents_config_store = factory_config_store.clone();
            let pt = factory_plugin_tools.clone();
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
                    instances2.clone(),
                    workspace.clone(),
                    agents_config_store.clone(),
                );
                // 工具结果出口卡口（2026-10 安全）：本 persona 注册表注入
                // 脱敏器——所有工具（内置 / 插件 / 远程 / 子代理直呼）的
                // 结果与错误消息统一脱敏后再交给任何消费者。
                t.set_redactor(redactor2.clone());
                // 适配器工具（消息发送/查询）必须注册到每个 persona：缺少
                // send_private_msg / send_group_msg 等工具时，persona 收到
                // 适配器入站 hook 后无法完成声明目标的投递。
                // 每人格注册**自己实例集合**的适配器工具（多实例经 account
                // 寻址）；按能力（send_text）过滤在注册函数内完成——无出站
                // 能力的适配器不注册任何工具（泛化 S6 / 01b §D.4）。
                let persona_instances = factory_instances_by_persona
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| default_instances.clone());
                // 适配器出站闸门（2026-10 安全）：send_* 的文本命中密钥按策略
                // 阻断/替换；文件参数落在敏感目录（配置 / 凭据目录）直接拒发。
                let outbound_gate = redactor2.as_ref().map(|redactor| {
                    std::sync::Arc::new(crate::instance_tools::OutboundGate {
                        redactor: redactor.clone(),
                        protected_paths: sensitive_dirs2.clone(),
                        policy: outbound_policy,
                    })
                });
                let instance_tool_names = crate::instance_tools::register_instance_tools_multi(
                    &mut t,
                    persona_instances,
                    outbound_gate,
                );
                // 包元数据：**按注册返回值**精确打标（不按 `send_`/`get_`
                // 前缀猜——前缀猜测等于假设「工具名遵循某平台命名习惯」，
                // 换平台换命名就漏标）。包 id 取适配器插件 id：适配器工具与
                // 对应技能同包，面板按包批量勾选时是一组。
                // 包 id 由插件在工具声明里给（`ToolSpec.package`）——
                // Core 不猜平台名。
                for (name, package) in &instance_tool_names {
                    if let Some(package) = package {
                        t.set_package(name, package);
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
                // 子进程插件工具（plugins.toml；RemoteTool 已实现 Tool trait）。
                for tool in pt.iter() {
                    let name = tool.tool_name().to_string();
                    t.register(tool.clone());
                    if let Some(pkg) = tool.package() {
                        t.set_package(&name, pkg);
                    }
                }
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
                                let provider: Arc<dyn echo_agent::LlmProvider> = Arc::from(p);
                                let provider = echo_agent::llm::wrap_metering(
                                    provider,
                                    metrics2.clone(),
                                    name.clone(),
                                );
                                let provider = match &redactor2 {
                                    Some(redactor) => {
                                        echo_agent::llm::wrap_redacting(provider, redactor.clone())
                                    }
                                    None => provider,
                                };
                                Some(provider)
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
                // 声明式装配（AgentBuilder，框架优化议题 2）：必填槽
                // （plugin_host/event_sink）缺失即装配 bug 直接 panic；
                // 工作区会话存储（命令处理 + 系统提示注入共用同一实例）、
                // 进程级事件汇聚点与共享插件宿主（运行期新建人格同样走这里）、
                // Persona 级 API 引用（None = 跟随全局默认；运行期重建走
                // apply_persona_api）一次声明完成。
                let mut builder = echo_agent::agent::builder::AgentBuilder::new(
                    persona_provider.unwrap_or_else(|| Arc::clone(&provider_arc2)),
                    cfg.clone(),
                    skills2.clone(),
                    t,
                    instances2.clone(),
                )
                .plugin_host(shared_plugin_host.clone())
                .event_sink(event_sink.clone())
                .team_id(Some(id.clone()))
                .workspace_store(workspace_store)
                .persona_api(profile.api_profile.clone())
                .config_store(agents_config_store.clone())
                .subagent_store(subagent_store.clone())
                .metrics(metrics2.clone());
                // 敏感信息脱敏器（2026-10 安全）：Agent 侧入站 / 事件 /
                // 结果出口统一脱敏。
                if let Some(redactor) = &redactor2 {
                    builder = builder.redactor(redactor.clone());
                }
                if let Some(api_cfg) = api_cfg_override {
                    // 启动期已按 persona profile 解析好：把生效 model 同步给
                    // active_model（provider 已独立构建，无需重建）。
                    builder = builder.active_model(api_cfg.model);
                }
                // 能力配置在 make_agent 之后的启动阶段应用（见 start loop）。
                //
                // 配置持久化（TOML）与会话持久化（JSON）解耦：
                // - config store：共享 core.toml 的 ConfigStore（[agent] section）
                // - 会话路径：独立 JSON 文件 echo-sessions-{id}.json（统一命名；
                //   旧的 default 专用 echo-sessions.json 首次启动自动改名迁移）
                // 两者文件格式不同，绝不可共用同一路径（JSON 会让 TOML parse 失败）。
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
                builder.session_persist_path(file).build()
            }
        },
    );
    // 默认人格（管理面/适配器入口）挂在原变量 `agent` 上，后续代码不变。
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
    {
        let (rt, event_rx, peer_policies) = federation;
        // 负载上报注入（P2 调度器）：握手 hello 时实时取全部 persona
        // 的活跃 turn 总数——对端调度器拿到实时负载而非启动快照。
        let load_supervisor = supervisor.clone();
        rt.federation
            .set_load_reporter(std::sync::Arc::new(move || {
                load_supervisor
                    .personas()
                    .iter()
                    .map(|p| p.agent.active_turn_session_ids().len() as u32)
                    .sum()
            }))
            .await;
        // P3-3 聚合投递出口：大脑侧 settle 产出的摘要经此向父会话
        // timeline 追加 system 消息（supervisor 句柄在 echo-agent crate
        // 不可达，组合根注入）。
        {
            let deliver_supervisor = supervisor.clone();
            echo_agent::federation::set_aggregate_deliver(std::sync::Arc::new(move |summary| {
                let sup = deliver_supervisor.clone();
                tokio::spawn(async move {
                    federation_import::deliver_aggregate_summary(&sup, summary).await;
                });
            }));
        }
        tokio::spawn(federation_router_pump(
            event_rx,
            rt,
            supervisor.clone(),
            core_agent.clone(),
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
    //（tools.builtin / skills.dir / checklist / instance.qq / management.panel /
    // loop.runner）的 mount 闭包执行真实副作用（禁用即卸载效果）；
    // provider.llm 仍为名义挂载（重启生效）。
    //
    // 适配器实例的接线（hook/handler/config store）在插件挂载之后才完成；
    // qq_wired 标志保证启动期 mount 不抢跑启动适配器（启动期由接线后的
    // 门控启动负责），运行期 TogglePlugin 才真正 start/stop。
    let qq_wired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let qq_running = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        use crate::instance_manifests::ADAPTER_QQ_PLUGIN_ID;
        use echo_agent::plugins::{
            ToolSink, MANAGEMENT_PANEL_PLUGIN_ID, SKILLS_DIR_PLUGIN_ID, TOOLS_BUILTIN_PLUGIN_ID,
        };
        use echo_context::Disposer;
        use echo_plugin::{BuiltinPlugin, MountContext, PluginKind, PluginManifest, PluginRuntime};
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

        // ── 实化 4：循环模式（二选一）──
        // 驱动本体只有一个：echo-loop TurnRunner（2026-10 单轨化，内置循环
        // 已删除）；它在人格装配时无条件注入（见下方回填与 agent factory）。
        // 两个模式插件只表达**策略**（单会话串行 / 并行多会话），由
        // per-persona 白名单推导的 `LoopMode` 决定（见 `TeamMember::loop_mode`）；
        // mount/unmount 不再切换驱动。
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
            register(&plugin_host,
                PluginManifest::builtin(
                    id,
                    name,
                    version,
                    PluginKind::Loop,
                    "loop",
                    description,
                ),
                // 纯策略插件：无运行期资源（模式由白名单推导，每次读取）。
                move |_ctx| Ok(Vec::new()),
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
            // 实例句柄（独立进程适配器插件；无实例 = None，启停为 no-op）。
            let qq = qq_instance.clone();
            let wired = qq_wired.clone();
            let running = qq_running.clone();
            register(
                &plugin_host,
                PluginManifest::builtin(
                    ADAPTER_QQ_PLUGIN_ID,
                    "QQ 适配器",
                    version,
                    PluginKind::PluginInstance,
                    "qq",
                    "OneBot v11 反向 WS 适配器插件（独立进程；含 QQ 管理工具）",
                )
                // 适配器插件化收尾（P1.9，2026-10）：实现由独立进程
                // `echo-instance-qq` 提供，这里只是门控壳。必须显式声明，
                // 否则 Panel 按 builtin=true 把它显示成「内部插件」。
                .with_runtime(PluginRuntime::Subprocess),
                move |_ctx| {
                    for_each_agent(|a| a.reapply_plugin_gating(ADAPTER_QQ_PLUGIN_ID, true));
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
                    Ok(vec![Disposer::from_fn(move || {
                        for_each_agent(|a| a.reapply_plugin_gating(ADAPTER_QQ_PLUGIN_ID, false));
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

        // ── 实化 4：管理面（management WS 起停）──
        // 注意自锁语义：禁用管理面 = 关闭 Panel 通道本身，恢复需编辑
        // core.toml 的 disabled_plugins 后重启（文档已注明）。
        {
            let mgmt_addr = cfg.core.management_address.clone();
            let mgmt_token = cfg.core.management_access_token.clone();
            let mgmt_agent = core_agent.clone();
            let mgmt_bridge = bridge.clone();
            // 合并模式：监听端承接联邦入站（令牌分流）；独立模式：None。
            let mgmt_fed = if merged {
                Some(merge_fed.clone())
            } else {
                None
            };
            register(
                &plugin_host,
                PluginManifest::builtin(
                    MANAGEMENT_PANEL_PLUGIN_ID,
                    "管理面",
                    version,
                    PluginKind::Management,
                    "panel",
                    "Panel management WS 桥接（合并模式下同时承接联邦入站）",
                ),
                move |_ctx| {
                    let (addr, br, ag) =
                        (mgmt_addr.clone(), mgmt_bridge.clone(), mgmt_agent.clone());
                    let token = mgmt_token.clone();
                    let fed_opt = mgmt_fed.clone();
                    let fed_merged = fed_opt.is_some();
                    let server = tokio::spawn(async move {
                        let result = match fed_opt {
                            Some(fed) => {
                                management::serve_with_federation(&addr, br, ag, token, fed).await
                            }
                            None => management::serve_with_token(&addr, br, ag, token).await,
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
    // Turn 驱动注入：所有人格共享组合根的 TurnRunner（含计量/脱敏装饰的
    // provider 与配置预算）。运行期新建人格由 set_agent_factory 的包装覆盖。
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
        persona.agent.set_loop_runner(runner.clone());
        persona.agent.start_session_save_task();
        persona.agent.start_plugin_reload_task().await;
        // orchestration 已删除（2026-09-16）
    }

    // 余额快照周期任务（全局管理代理，只此一份）：按
    // `[agent].balance_snapshot_secs`（默认 600s）采样每个可查询 profile，
    // 写入本地 echo-balances.jsonl（面板余额/用量图表的数据源）。
    if cfg.agent.balance_snapshot_secs > 0 {
        core_agent.start_balance_snapshot_task();
    }

    // ---- Wire agents into instance instances ----
    // 每个实例把入站消息投给**归属人格**（不再是"默认人格"）；实例间互相隔离。
    // 每实例一个串行 worker（入站队列 → AgentMessageHook）。队列由
    // `core_api` 的反压回执语义保序（插件侧退避重试）。
    for (instance_id, persona_id, handle) in qq_plugin_workers {
        let target = match supervisor.get_exact(&persona_id) {
            Some(p) => p.agent,
            None => {
                warn!(persona = %persona_id, "adapter plugin persona not found; skipping wire");
                continue;
            }
        };
        let hook = Arc::new(echo_agent::AgentMessageHook::new(target));
        let worker_instance = instance_id.clone();
        let mut inbound_rx = handle.inbound_rx;
        tokio::spawn(async move {
            while let Some(message) = inbound_rx.recv().await {
                let incoming = echo_plugin_host::instance::message_to_incoming(message);
                if let Err(error) = hook.on_incoming_message(incoming).await {
                    tracing::warn!(
                        instance = %worker_instance,
                        %error,
                        "plugin inbound message hook failed"
                    );
                }
            }
            tracing::debug!(instance = %worker_instance, "plugin inbound worker stopped");
        });
        tracing::info!(instance = %instance_id, persona = %persona_id, "adapter plugin instance wired to persona");
    }
    // plugin 实例的连接态（`report_state`）→ `InstanceStateChanged` 事件
    //（Panel 连接状态卡的实时数据源；persona 按实例映射动态解析，容忍
    //  运行期人格重启——查不到时静默跳过）。
    if let Some(rt) = &plugin_runtime {
        let mut states = rt.core_api().subscribe_states();
        let supervisor_for_states = Arc::clone(&supervisor);
        let persona_by_instance: std::collections::HashMap<String, String> = qq_instances
            .iter()
            .map(|instance| (instance.id.clone(), instance.persona.clone()))
            .collect();
        tokio::spawn(async move {
            while let Some(change) = states.recv().await {
                let Some(persona) = persona_by_instance.get(&change.instance) else {
                    continue;
                };
                let Some(entry) = supervisor_for_states.get_exact(persona) else {
                    continue;
                };
                entry
                    .agent
                    .emit(echo_protocol::BackendEvent::InstanceStateChanged {
                        instance_name: change.instance,
                        connected: change.connected,
                        self_id: change.self_id,
                    });
            }
        });
    }
    // 插件 UI 状态（`report_ui_state`）→ `InstanceState` 事件（泛化 01a §1.2
    // S4：推送接线，与连接态通道分离）。
    if let Some(rt) = &plugin_runtime {
        let mut ui_states = rt.core_api().subscribe_ui_states();
        let supervisor_for_ui = Arc::clone(&supervisor);
        let ui_fallback_agent: Arc<echo_agent::Agent> = core_agent.clone();
        let ui_persona_by_instance: std::collections::HashMap<String, String> = qq_instances
            .iter()
            .map(|instance| (instance.id.clone(), instance.persona.clone()))
            .collect();
        let ui_default_persona = qq_default_persona.clone();
        tokio::spawn(async move {
            while let Some(change) = ui_states.recv().await {
                // persona 解析（01a §1.2-3）：优先状态文档自带
                // `instance.persona`（插件上报），否则按实例名查映射；查不到
                // 回退默认 persona——未登记实例的适配器不能丢状态。
                let persona = change
                    .state
                    .pointer("/instance/persona")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                    .or_else(|| ui_persona_by_instance.get(&change.instance).cloned())
                    .unwrap_or_else(|| ui_default_persona.clone());
                // 目标入口：人格 entry → 回落进程级核心代理（所有 agent 共享
                // 同一事件汇聚点，兜底不丢状态；不做 continue）。
                let entry = supervisor_for_ui
                    .get_exact(&persona)
                    .map(|entry| entry.agent)
                    .unwrap_or_else(|| ui_fallback_agent.clone());
                entry.emit(echo_protocol::BackendEvent::InstanceState {
                    instance: change.instance,
                    state: change.state,
                });
            }
        });
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
    // 进程级 AgentManager 锚点（RequestAgentsList / 命令路由 / 运行期启停）：
    // supervisor 的**同一个**注册表——单一真源，不再镜像第二份。
    // 此前 supervisor 与 manager 各持一份、对"全部人格被禁用时是否合成
    // default"处理不一致，导致 Panel 报「智能体 default 不存在」。
    {
        let mgr_arc = supervisor.manager();
        // 进程级 agent factory：运行时新增/重启人格时重建实例。与启动期
        // 使用同一装配闭包（make_agent），实例接线完全一致。
        {
            let f = make_agent.clone();
            let runner = runner.clone();
            echo_agent::agent_manager::set_agent_factory(move |id: String, p: AgentProfile| {
                let agent = f(id, p);
                // 运行期新建人格：与启动期同一共享 runner。
                agent.set_loop_runner(runner.clone());
                agent
            });
        }
        // 配置写回：SaveAgent/DeleteAgent 持久化到 core.toml。
        {
            let store = config_store.clone();
            mgr_arc.set_config_writer(Box::new(
                move |teams: &std::collections::BTreeMap<String, echo_agent::TeamMember>| {
                    store
                        .patch(|root| {
                            let agent = echo_config::ensure_table(root, "agent");
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
    // ---- Auto-start instance instances if enabled ----
    // 接线完成，标记 wired：此后 instance.qq 插件的运行期 enable 才会真正
    // start 适配器（启动期 mount 不抢跑）。启动受配置与插件状态双重门控。
    qq_wired.store(true, std::sync::atomic::Ordering::SeqCst);
    let qq_plugin_enabled = agent
        .plugin_host()
        .registry
        .is_enabled(crate::instance_manifests::ADAPTER_QQ_PLUGIN_ID);
    if qq_host.enabled && qq_plugin_enabled {
        // 多实例：逐个生成 compose（非 legacy）并启动；单个失败不影响其余。
        let data_dir = qq_data_dir.clone();
        for (instance_id, persona_id, instance) in &qq_handles {
            if let Some(instance) = qq_instances.iter().find(|i| &i.id == instance_id) {
                if instance.id != instance_deploy::DEFAULT_INSTANCE {
                    if let Err(error) = instance_deploy::napcat::write_compose(
                        &data_dir,
                        &instance.id,
                        &instance.ports,
                    ) {
                        warn!(instance = %instance.id, %error, "compose write failed");
                    }
                }
            }
            match instance.start().await {
                Ok(()) => {
                    info!(instance = %instance_id, persona = %persona_id, "adapter instance started");
                }
                Err(error) => {
                    warn!(instance = %instance_id, %error, "adapter instance start failed");
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

    // Graceful shutdown: stop the command pump and instance handle, cancel
    // background tasks, and flush sessions to disk.
    pump.abort();
    if let Some(qq) = &qq_instance {
        let _ = qq.stop().await;
    }
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
    // 子进程插件优雅关停（在人格 shutdown 之后、最终返回之前）。
    if let Some(rt) = &plugin_runtime {
        rt.shutdown().await;
    }
    Ok(())
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
