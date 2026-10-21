//! EchoAgentCore — agent core service.
//!
//! 运行 Agent 核心：LLM agent 循环、平台插件（独立进程插件）、
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

mod builtin_plugins;
mod host_services;
mod agent_supervisor;
mod config;
mod federation_admin_host;
mod federation_import;
mod federation_rt;
mod instance_deploy;
mod instance_manifests;
mod instance_tools;
mod logging;
mod node;
mod plugins_runtime;
mod port_cmd;

/// 管理面插件（Phase 3 ⑧：已外迁为独立 crate，组合根只负责启动它）。
use echo_plugin_management as management;
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
    ///
    /// `global`：子命令（token / port / dump-config）也能用，且**前置后置
    /// 皆可**——`-c x dump-config` 与 `dump-config -c x` 等价。不设时
    /// clap 要求选项必须写在子命令之前（`dump-config -c x` 会报
    /// "unexpected argument"），对 `dump-config` 这类"只想指一个配置
    /// 看一眼"的命令尤其别扭。
    #[arg(short, long, global = true)]
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
    /// 打印 plugins.toml 的**分层合成结果**（profile → bundles → patches →
    /// 行内条目；每行上方注释标出来自哪一层）。
    ///
    /// 只合成不启动：不拉起插件、不监听端口。用途是排查"这行配置到底
    /// 被哪一层覆盖成了现在这样"，以及提交前确认合成树符合预期。
    /// plugins.toml 不存在时说明特性休眠（退出码 0，不算错误）。
    DumpConfig,
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
            SubCommand::DumpConfig => return dump_config_command(&args.config_path()),
        }
    }
    let path = args.config_path();
    let cfg = CoreConfig::load(&path)?;
    run_core(args, cfg).await
}

/// `echo-agent-core dump-config`：打印 plugins.toml 的分层合成结果。
///
/// 输出到 stdout（可直接 `>` 成文件、或 `diff` 两份合成结果）；诊断信息
/// 走 stderr，保证管道里拿到的只有 TOML 正文。
fn dump_config_command(config_path: &std::path::Path) -> Result<()> {
    let plugins_path = config_path.with_file_name("plugins.toml");
    match plugins_runtime::dump_config(config_path) {
        Ok(Some(text)) => {
            print!("{text}");
            Ok(())
        }
        Ok(None) => {
            println!(
                "# {} 不存在——外部插件特性休眠（启动等同未启用）",
                plugins_path.display()
            );
            Ok(())
        }
        Err(error) => Err(anyhow::anyhow!(error)).with_context(|| {
            format!(
                "合成 {} 失败（修好它，启动路径会报同一个错）",
                plugins_path.display()
            )
        }),
    }
}

// ── Core：Agent + 插件实例 + management WS 服务 ──────────────────────────────

/// 当前 Unix 毫秒（插件事件的时间戳；面板排序用）。
fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 服务变更的一行描述（日志用；`plugin tree reconciled` 的 `service` 字段）。
///
/// 不直接 `{:?}` 整个 `Change`：日志里要一眼看出"哪个键、哪一层"，
/// 结构体调试格式会把这两条埋在花括号里。
fn describe_change(change: &echo_context::Change) -> String {
    match change {
        echo_context::Change::Provided { key, depth } => format!("+{key}@d{depth}"),
        echo_context::Change::Disposed { key, depth } => format!("-{key}@d{depth}"),
    }
}

async fn run_core(args: Args, cfg: CoreConfig) -> Result<()> {
    let log_reload = init_tracing(&cfg.logging)?;
    // 级别热更新（2026-10 日志治理）：`SIGHUP` 重读 `[logging].level`，
    // 免去为开 debug 而重启进程（重启要等在途回复 drain，最长 180s）。
    logging::spawn_reload_on_sighup(args.config_path(), log_reload);

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
    // 构造本节点身份（SessionInfo/TeamInfo 等线格式携带它——agent 的
    // 「运行区域」属性，供多节点聚合客户端区分同名会话/人格）。
    // P3 终态：**按值构造、显式下发**给 AgentManager 与每个 Agent
    // （`make_agent` 闭包），不再写进程级 kernel cell。
    let node_identity = echo_agent::NodeIdentity::new(node_doc.node_id.clone(), region_name);

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
    // 进程级事件出口的延迟槽位：联邦命令处理器在装配期就要绑好（端口集合
    // 是值，构造后不再改），而 bridge 与 `event_sink` 在更后面才建立。
    // 一次写、多次读。
    let federation_emit_slot: std::sync::Arc<
        std::sync::OnceLock<echo_agent::EventSink>,
    > = std::sync::Arc::new(std::sync::OnceLock::new());
    // Shell 子系统句柄（技债清理：替代两个进程级锚点）。工具注册与 agent
    // 构造都早于 ShellManager 本体，所以先建空槽、就绪后再 bind。
    let shell_handle = echo_agent::shell::ShellHandle::new();
    // manager 弱引用槽（技债清理：替代 `global_manager` 进程级锚点）。
    // rt 随后被解构进路由泵，而 manager 更晚就绪——外层留同一份 Arc，
    // 延迟写入即可；rt 内部读的是同一个槽。
    let agent_manager_slot: std::sync::Arc<
        std::sync::RwLock<std::sync::Weak<echo_agent::agent_manager::AgentManager>>,
    > = Default::default();
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
        // 能力声明：本机可被远程调用的工具集合。
        //
        // 这里**不再有工具名清单**（2026-11 S8）：真源是每个工具自己的
        // `remote_capable()` 声明，supervisor 就绪后经
        // `Federation::set_tool_reporter` 注入注册表查询，握手时现取。
        // 此前这里写死 6 个名字、联邦插件里再写同样的 6 个名字取交集——
        // 插件新加的工具永远进不了远程面，而任一工具改名会静默掉出去
        // （对端照旧宣告、本机照旧忽略，两边都没有任何报错）。
        let caps = echo_federation::NodeCaps {
            tools: Vec::new(),
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
        let local_advertise =
            echo_plugin_federation::admin::invite_host(listen_for_advertise, &cfg.federation.advertise)
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
        // **延后**起监听/连出（2026-11 S8）：握手里的 `caps.tools` 来自
        // 各 persona 的工具注册表，而注册表要等插件挂载 + supervisor 就绪
        // （本函数后半段）。此前在这里就 `run()`，于是"对端在我们启动过程中
        // 连入"会拿到空工具集，对端侧**不会**再重问——远程工具一直缺失到
        // 下次断线重连（实测：两节点同时重启时稳定复现）。
        // 监听晚起几百毫秒的代价远小于静默少一半远程面。
        let fed_runner = fed.clone();
        let listen_for_run = listen.clone();
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
            session_import_acks: std::sync::Mutex::new(std::collections::HashMap::new()),
            // 人格登记表（2026-11 S14）：由每个人格构造时自己写入（弱引用），
            // 于是联邦侧不再需要 `AgentSupervisor` / `AgentManager` 句柄。
            personas: tokio::sync::RwLock::new(Vec::new()),
            event_fallback: std::sync::RwLock::new(None),
        });
        // 远程 subagent 观测出口（Phase 3）：大脑侧 spawn_subagent
        // node=... 受理/完成时经 Federation 通知对端（帧：
        // SubagentSpawn 受理 / SubagentEvent 终态）。peer 名 → node_id
        // 反查走 rt.peer_names（链路 Up 时登记）。
        // 聚合摘要投递需要 supervisor 句柄，而 supervisor 在联邦装配之后才
        // 建立——用一次性槽位延后绑定（联邦泵启动晚于 supervisor，任何摘要
        // 都不可能在槽位就绪前产生）。
        // ── 联邦出口集合（P3 终态：构造一份、显式下发；不再经 kernel cell）──
        // 三个出口都持有本节点的 Federation / InvokeRouter 句柄，只能在
        // 组合根这里造；造好后经 `AgentBuilder::federation` 下发给每个
        // persona，并经同一份 `ports` 供联邦泵销账。
        let mut ports = echo_agent::federation::FederationPorts::unlinked();
        // 联邦 / 自更新命令处理器（联邦始终开启，无需条件注册）：需要
        // Federation 句柄与 ConfigStore，只能在组合根实现。
        // 出口在**装配期**固定成进程级汇聚点：`command_handler` 的类型已不再
        // 引用 `Agent`（全插件化 S3），所以不必等某个 persona 出现再取它的
        // emit——此前每个 persona 都会往同一个进程级 ports 里塞"自己的 emit"，
        // 谁的装配最后跑谁生效。
        // 管理面的**状态迁移**出口（2026-11 S11）：命令的判定与文案已经搬进
        // `echo-plugin-federation::admin`，这里只提供"改内存与磁盘"的原子动作。
        // 事件出口由插件侧从服务表取（进程级汇聚点），所以这里不需要再传 emit。
        ports.admin = Some(std::sync::Arc::new(
            crate::federation_admin_host::RuntimeAdminHost::new(Arc::clone(&rt)),
        ));
        ports.deliver = Some(std::sync::Arc::new({
            let rt_deliver = rt.clone();
            move |summary| {
                // 投递目标直接从人格登记表里找（"哪个 agent 有这个会话"），
                // 不再需要 supervisor 句柄（2026-11 S14）。
                let rt = rt_deliver.clone();
                tokio::spawn(async move {
                    federation_import::deliver_aggregate_summary(&rt, summary).await;
                });
            }
        }));
        {
            let rt_notify = rt.clone();
            ports.notifier = Some(std::sync::Arc::new(
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
                ports.querier = Some(std::sync::Arc::new(move |peer, kind, subject| {
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
                }));
            }
            ports.invoker = Some(std::sync::Arc::new(move |peer, tool, args| {
                let rt = rt_invoke.clone();
                Box::pin(async move {
                    let Some(node_id) = rt.resolve_peer_node(&peer).await else {
                        return Err(format!("远程节点「{peer}」不在线或不存在（node:// 分流）"));
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
                    match tokio::time::timeout(std::time::Duration::from_secs(180), pending.wait())
                        .await
                    {
                        Ok(Ok(result)) if result.success => Ok(result.output),
                        Ok(Ok(result)) => Err(result.output),
                        Ok(Err(e)) => Err(e),
                        Err(_) => Err(format!("远程节点「{peer}」响应超时")),
                    }
                })
            }));
        }
        (
            rt,
            event_rx,
            peer_policies,
            std::sync::Arc::new(ports),
            fed_runner,
            listen_for_run,
        )
    };
    info!("federation link configured (no switch; zero peers = passive listen)");
    // 端口合并用联邦句柄（rt 随后被路由泵消费；此处预克隆——仅合并
    // 模式下由管理面插件消费）。
    let merge_fed = federation.0.federation.clone();
    // 联邦出口集合（含 agent 侧需要的 invoker/querier/notifier/deliver/
    // aggregator）：所有 persona 共用同一份。
    let federation_ports = federation.3.clone();
    // 联邦 / 自更新命令处理器（联邦始终开启，无需条件注册）。

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
    // 敏感目录（插件实例外发文件闸门用；core.toml / 会话 / 凭据目录）。
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
    let _ctx_keep = ctx.register::<Arc<dyn echo_agent::LlmProvider>>(
        echo_defs::llm::LLM_PROVIDER_SERVICE_KEY,
        provider_arc.clone(),
    );
    // 脱敏服务（"sanitizer" 键；进程内插件经 MountContext.ctx 解析）。
    let _sanitizer_keep: Option<echo_context::Disposer> = redactor.as_ref().map(|redactor| {
        ctx.register::<Arc<dyn echo_defs::sanitize::Redactor>>("sanitizer", redactor.clone())
    });

    // 回合驱动（`loop` 服务）：**由 `echo-plugin-loop` 插件在 mount 里构造并
    // provide**，组合根不再自己建一个再借插件的名义登记（2026-11 S2）。
    //
    // 它需要的前置都在上下文里：`llm`（上面已注册）+ `plugins.toml` 的
    // `[plugin.config]`（可覆盖 max_tool_iterations / max_tokens）。装配顺序由
    // 声明式 inject 保证——插件声明了 `llm` 为硬依赖，缺失即拒绝挂载。
    //
    // 注意：**每个 agent 仍然自己建 runner 跑 turn**（`agent/turn.rs`），
    // 因为 `TurnRunner` 绑定了 provider / bus / pipeline 三件 per-persona 的
    // 东西。进程级 `loop` 服务是"驱动可由插件提供"这条契约的落脚点，也是
    // 面板/诊断里"循环服务由谁提供"的答案。

    // PluginInstance registry.
    let instance_registry = echo_defs::plugin_instance::InstanceRegistry::new();

    // ---- 插件实例（多实例；实例 = 插件实例名 = 会话 account 维度）----
    // 本组合根当前只**接线一个平台**：取配置里第一个 `enabled = true` 的
    // `[instances.<name>]`（都没有则取第一个配置段，再无则空串）。
    //
    // **不写死平台名**：Core 二进制不认识 "qq"——平台名完全来自用户配置。
    // 多平台同时接线是后续工作（下面整段接线按单平台推导：一个 manifest、
    // 一组 specs）；今天只有一个平台被编排时行为与从前一致。
    let platform = cfg
        .instances
        .iter()
        .find(|(_, host)| host.enabled)
        .or_else(|| cfg.instances.iter().next())
        .map(|(name, _)| name.clone())
        .unwrap_or_default();
    // persona 门控：启用了 `echo-agent.instance.<platform>` 的人格各自获得实例
    // （未配置实例时自动建档），legacy 单实例部署行为不变。
    let qq_enabled_personas: Vec<String> = cfg
        .agent
        .teams
        .iter()
        .filter(|(_, m)| {
            m.enabled
                && echo_agent::plugins::profile_allows_plugin(
                    m,
                    &crate::instance_manifests::plugin_id_for(&platform),
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
    let qq_host = cfg.instances.get(&platform).cloned().unwrap_or_default();
    let qq_instances = instance_deploy::resolve_instances(
        &platform,
        &qq_host,
        &qq_host.instances,
        &qq_enabled_personas,
        &qq_default_persona,
    );
    // 插件实例仅以独立进程插件运行（P1.9，2026-10 实例插件化收尾——
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
    // 归属人格 / 运行期状态文件（门控 / 名单 / owner 落盘回灌）与完整插件实例
    // 配置（共享层 ⊕ 实例覆盖 + 端口分配结果；TOML 表透传）。
    let qq_plugin_specs: Vec<echo_plugin_host::bootstrap::LaunchSpec> = {
        let launch = &qq_host.launch;
        let command = if launch.command.trim().is_empty() {
            // 约定：插件可执行文件名为 `echo-adapter-<platform>`（与插件仓库同名）
            format!("echo-adapter-{platform}")
        } else {
            launch.command.trim().to_string()
        };
        qq_instances
            .iter()
            .map(|instance| {
                let state_file = qq_data_dir.join(format!("{platform}-{}.toml", instance.id));
                echo_plugin_host::bootstrap::LaunchSpec {
                    plugin_id: format!("instance-{platform}-{}", instance.id),
                    command: command.clone(),
                    args: launch.args.clone(),
                    env: launch.env.clone(),
                    // 实例插件的限额：与 `[adapters.<platform>.plugin]` 同源
                    // （`launch.limits`），不设 = 不限制。
                    limits: launch.limits,
                    // 三个字段各有读者，**都不再由 Core 派生**：
                    // - `shared` / `overrides`：新插件自己深合并 + 分配端口 +
                    //   派生容器名 / compose（NapCat 部署形状归插件）；
                    // - `config`：合并结果，只给**已部署的旧插件**读（过渡窗口）。
                    config: serde_json::json!({
                        "name": instance.id,
                        "persona": instance.persona,
                        "state_file": state_file.to_string_lossy(),
                        "data_dir": qq_data_dir.to_string_lossy(),
                        "shared": qq_host.shared,
                        "overrides": instance.overrides,
                        "config": instance_deploy::legacy_merged_config(
                            &qq_host,
                            &instance.overrides,
                        ),
                    }),
                }
            })
            .collect()
    };
    // 实例持久化：自动建档实例的**归属人格**写回
    // `[instances.qq.instances.<id>]`，保证跨重启稳定（否则重启后 persona
    // 缺失会被重新分配给默认人格，实例归属漂移）。
    //
    // 端口不再写回——端口由插件自行分配并持久化到它自己的 state_file。
    {
        let mut patches: Vec<(String, String)> = Vec::new();
        for instance in &qq_instances {
            if instance.id == platform {
                continue; // legacy 实例沿用默认归属，不写配置
            }
            let has_persona = qq_host
                .instances
                .get(&instance.id)
                .and_then(|s| s.persona.clone())
                .is_some();
            if !has_persona {
                patches.push((instance.id.clone(), instance.persona.clone()));
            }
        }
        if !patches.is_empty() {
            if let Err(error) = config_store.patch(|root| {
                let section = instance_deploy::instance_section_key(root, &platform);
                let instances = echo_config::ensure_table(root, section);
                let qq = echo_config::ensure_table(instances, &platform);
                let instances = echo_config::ensure_table(qq, "instances");
                for (id, persona) in &patches {
                    let entry = echo_config::ensure_table(instances, id);
                    entry.insert("persona".into(), toml::Value::String(persona.clone()));
                }
                Ok(())
            }) {
                warn!(%error, "instance persona persist failed");
            } else {
                let ids: Vec<&str> = patches.iter().map(|(id, _)| id.as_str()).collect();
                info!(instances = ?ids, "instance persona assigned and persisted");
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
    // 联邦命令处理器的出口（装配期已绑好槽位，这里交付实体）。
    let _ = federation_emit_slot.set(event_sink.clone());

    // 进程级插件宿主（去主 P1）：所有人格共享同一实例，插件注册/挂载/卸载
    // 与「谁挂到 Panel」同样不再寄居在某个"默认人格"上。
    let plugin_host: std::sync::Arc<echo_agent::plugins::PluginHost> =
        std::sync::Arc::new(echo_agent::plugins::PluginHost::new());
    // 全局禁用策略（工具/技能启停）：全局开关不属于任何人格。构造**一份**，
    // 注入每个 persona（不再走进程级锚点——2026-10 技术债清理）。
    let global_policy = std::sync::Arc::new(echo_agent::agent::GlobalPolicy::new(
        agent_config.disabled_tools.clone(),
        agent_config.disabled_skills.clone(),
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
    // P1（2026-10 实例插件化）：把插件的插件实例贡献注册进
    // `InstanceRegistry`（与内置插件实例同册）。
    let plugin_instances: Vec<plugins_runtime::RemoteInstanceHandle> = plugin_runtime
        .as_ref()
        .map(|r| r.instances())
        .unwrap_or_default();
    // plugin 模式的插件实例：远程插件实例融入统一实例表（工具注册 /
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
        // 未命中（其他平台的实例插件 / 未登记实例）→ 默认人格，仍注册
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
    // persona → 该人格的插件实例集合（工具注册按人格隔离；实例统一表）。
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
    let plugin_tools: std::sync::Arc<Vec<std::sync::Arc<echo_plugin_adapter::RemoteTool>>> =
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
    // 节点身份（NodeId + 区域名）：所有 persona 共用一份，装配期下发。
    let factory_node_identity = node_identity.clone();
    // 联邦出口集合：同上（全部 persona 看同一个聚合器与同一组链路出口）。
    let factory_federation = federation_ports.clone();
    // 联邦运行时（2026-11 S13）：新建人格要补装**已经在线的**远程代理工具。
    // 克隆在这里（`federation` 元组稍后会被解构进路由泵）。
    let factory_federation_runtime = federation.0.clone();
    let factory_shell_handle = shell_handle.clone();
    // 全局禁用策略：所有 persona 共用一份（开关变化只需改这份）。
    let factory_global_policy = global_policy.clone();
    // 工具集插件（`echo-agent.tools.builtin`）的服务表：agent factory 在
    // "插件是否已挂载"上只问一次——挂上了就解析 provider，没挂上直接构造
    // 同一实现（见 `tools_builtin` 模块文档）。
    let tool_ctx = ctx.clone();
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
            let tool_ctx = tool_ctx.clone();
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
                //
                // **清单在插件里**（`echo-plugin-tools` 的 `BuiltinToolProvider`），
                // 这里只递端口。服务表里有 provider 就用它（插件已挂载），
                // 否则直接构造同一实现（`core_agent` 早于插件挂载——S1b 前的
                // 唯一无服务表路径，见 `tools_builtin` 模块文档）。
                let persona_instances = factory_instances_by_persona
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| default_instances.clone());
                let mut t = echo_agent::tool::builtin::build_registry(
                    instances2.clone(),
                    workspace.clone(),
                    agents_config_store.clone(),
                    factory_federation.invoker.clone(),
                    factory_shell_handle.clone(),
                    echo_agent::tool::builtin::resolve_provider(&tool_ctx),
                );
                // 工具结果出口卡口（2026-10 安全）：本 persona 注册表注入
                // 脱敏器——所有工具（内置 / 插件 / 远程 / 子代理直呼）的
                // 结果与错误消息统一脱敏后再交给任何消费者。
                t.set_redactor(redactor2.clone());
                // 插件实例工具（消息发送/查询）必须注册到每个 persona：缺少
                // send_private_msg / send_group_msg 等工具时，persona 收到
                // 插件实例入站 hook 后无法完成声明目标的投递。
                // 每人格注册**自己实例集合**的插件实例工具（多实例经 account
                // 寻址）；按能力（send_text）过滤在注册函数内完成——无出站
                // 能力的插件实例不注册任何工具（泛化 S6 / 01b §D.4）。
                // 插件实例出站闸门（2026-10 安全）：send_* 的文本命中密钥按策略
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
                // 换平台换命名就漏标）。包 id 取实例插件 id：插件实例工具与
                // 对应技能同包，面板按包批量勾选时是一组。
                // 包 id 由插件在工具声明里给（`ToolSpec.package`）——
                // Core 不猜平台名。
                for (name, package) in &instance_tool_names {
                    if let Some(package) = package {
                        t.set_package(name, package);
                    }
                }
                // 任务清单（checklist）为普通内置工具：随内置工具集包
                // （插件装配时已打 echo-agent.tools.builtin 标签），
                // 插件维度已于 2026-09 移除，只受工具级白/黑名单控制。
                // 工作区会话（workspace 插件）：每 persona 独立存储 + 工具。
                // 存储文件与配置 TOML 解耦（独立 JSON，绝不同路径互写）；
                // 插件未列入该 persona 白名单时工具会被门控禁用（GATED_PLUGIN_IDS）。
                let workspace_store =
                    std::sync::Arc::new(echo_agent::workspace::WorkspaceStore::load(Some(
                        config_store_path.with_file_name(format!("echo-workspaces-{id}.json")),
                    )));
                t.register(std::sync::Arc::new(
                    echo_agent::workspace::WorkspaceTool::new(workspace_store.clone())
                        // 跨机查询经宿主适配器注入（Phase 3 ⑤：插件不认识联邦；
                        // 联邦未接线时插件自动降级为「远程不可采集」）。
                        .with_remote_query(echo_agent::federation::workspace_query(
                            factory_federation.querier.clone(),
                        )),
                ));
                t.set_package("workspace", echo_agent::plugins::WORKSPACE_PLUGIN_ID);
                // Subagent 委派（subagent 插件）：**注册归插件接线**（2026-11 S7）。
                //
                // 此前这里手工注册占位工具 + 打包标签，而真正的执行闭包由
                // `attach_subagent_runtime` 在 agent 创建后才接线——于是"工具
                // 存在"与"能力可用"是两件事，宿主只能靠工具名特判把它们对上。
                // 现在 `attach_subagent_runtime_with` 一次做完：注册工具、打
                // 包标签、接执行闭包。这里只把 store 交给 builder。
                let subagent_store = echo_agent::subagent::SubagentStore::new();
                // 子进程插件工具（plugins.toml；RemoteTool 已实现 Tool trait）。
                //
                // 语义在 `ToolRegistry::register_plugin_tools`（可单测的纯逻辑）：
                // **已注册的名字是宿主保留名**，同名插件项被跳过并逐条回报，
                // 内置实现保持不变——否则 `register` 的覆盖语义会让插件悄悄
                // 顶掉内置实现（内置工具外迁为插件后 `calculator` /
                // `web_search` 曾经如此，线上只能看到"功能还在但行为变了"）。
                let registration = t.register_plugin_tools(pt.iter().map(|tool| {
                    echo_agent::PluginTool::new(
                        tool.clone() as std::sync::Arc<dyn echo_agent::Tool>,
                        tool.package().map(str::to_string),
                    )
                }));
                for (name, package) in &registration.skipped {
                    tracing::warn!(
                        persona = %id,
                        tool = %name,
                        package = package.as_deref().unwrap_or("<unlabeled>"),
                        "外部插件工具与已注册工具重名，已跳过该插件工具"
                    );
                }
                // 装配完成即检查工具重名：`insert` 是覆盖语义，撞名会让后
                // 注册的实现无声顶掉前者（典型：启用 `echo-plugin-example`
                // 后它的 `calculator` / `web_search` 与内置工具同名）。
                // 这里升级为 error 并点名，避免「功能悄悄换了个实现」。
                let conflicts = t.name_conflicts();
                if !conflicts.is_empty() {
                    let detail = conflicts
                        .iter()
                        .map(|(name, via)| format!("{name}（via {via}）"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    tracing::error!(
                        persona = %id,
                        conflicts = %detail,
                        "工具重名：同名工具被后者覆盖——请改名其一（内置工具与插件工具不应同名）"
                    );
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
                // 远程委派聚合：插件不认识联邦，聚合登记经这个接缝交出去。
                .subagent_remote_sink(std::sync::Arc::new(
                    echo_agent::federation::aggregator::AggregatorDelegationSink::from_ports(
                        &factory_federation,
                    ),
                ))
                .metrics(metrics2.clone())
                .node_identity(factory_node_identity.clone())
                .global_policy(factory_global_policy.clone())
                .federation(factory_federation.clone());
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
                let built = builder.session_persist_path(file).build();
                // **新建人格补装远程工具**（2026-11 S13）。
                //
                // 此前 `register_remote_tools` 只在链路 Up 时跑一次：链路已通
                // 之后新建的人格拿不到任何 `peer__*` 工具，面板看得见它、模型
                // 看不见，且没有任何报错——要等下次断线重连才补上。
                //
                // factory 是**同步**闭包，所以这里 spawn：注册本身是异步的
                // （要读 PeerInfo），而"人格刚建好"到"它开始跑第一个 turn"
                // 之间隔着面板操作，几十毫秒的窗口不会造成可感知的缺失。
                {
                    let rt = factory_federation_runtime.clone();
                    let agent = built.clone();
                    let persona_id = id.clone();
                    tokio::spawn(async move {
                        // ① 登记进联邦侧的人格表（**人格自己报**，2026-11 S14）：
                        //    联邦不再持有 supervisor / manager 句柄。
                        rt.register_persona(&persona_id, &agent).await;
                        // ② `__core` 同时充当"无人格时的事件兜底出口"。
                        if persona_id == "__core" {
                            rt.set_event_fallback(&agent);
                        }
                        // ③ 补装已在线的远程代理工具（2026-11 S13）。
                        crate::federation_rt::adopt_active_links(&rt, &agent, &persona_id).await;
                    });
                }
                built
            }
        },
    );
    // 默认人格（管理面/插件实例入口）挂在原变量 `agent` 上，后续代码不变。
    // 进程级核心服务代理（不是人格）：承接全局/管理类命令（API 配置、技能/
    // 工具/插件清单与启停、插件实例启停、Shell、Teams 管理…）。它不出现在
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
    // 节点身份注入注册表：`AgentsList` 的每条 TeamInfo 都带它（多节点聚合
    // 客户端据此区分同名人格/区域）。
    supervisor
        .manager()
        .set_node_identity(node_identity.clone());
    // 去"主智能体"：不存在默认人格。全局/管理用途一律走进程级核心服务代理
    // （`core_agent`，team_id 为空、不在 TeamsList 中）；聊天与会话按 team_id
    // 显式路由到具体人格。
    let agent: Arc<echo_agent::Agent> = core_agent.clone();
    info!(agents = ?supervisor.ids(), "agent supervisor ready");

    // 联邦路由泵（Phase 2）：supervisor 就绪后启动（代理工具注册需要全部
    // persona 的注册表）。
    {
        let (rt, event_rx, peer_policies, _ports, fed_runner, listen_for_run) = federation;
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
        // 远程可调用工具的真源（S8）：工具自己的 `remote_capable()` 声明。
        //
        // 取**全部 persona 注册表的名字并集**：执行端按第一个 persona 的
        // 注册表执行（见路由泵的 Invoke 分支），而各 persona 装的是同一批
        // 插件——并集是"本机任意 persona 能接的活"，与实际接活能力一致；
        // 少了任何一个，对端就少注册一个代理工具。
        let caps_supervisor = supervisor.clone();
        rt.federation
            .set_tool_reporter(std::sync::Arc::new(move || {
                let mut names: Vec<String> = Vec::new();
                for persona in caps_supervisor.personas().iter() {
                    for name in persona.agent.tools.remote_capable_names() {
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                }
                names.sort();
                names
            }))
            .await;
        // 工具上报就绪后才起联邦监听/连出（顺序即协议契约：见上面延后说明）。
        tokio::spawn(async move {
            if let Err(e) = fed_runner.run(listen_for_run).await {
                // 独立监听退出（bind 失败如端口占用、或运行期错误）＝入站
                // 静默失效；此前仅 warn（2026-10 审计 P1-5b），升级为
                // error 并给出排查线索。
                tracing::error!(
                    error = %e,
                    "federation listener exited — inbound links will NOT be accepted; check the listen port is free (ss -tlnp) and not used by another Core"
                );
            }
        });
        tokio::spawn(federation_router_pump(
            event_rx,
            rt,
            peer_policies,
            federation_ports.clone(),
        ));
    }

    core_agent
        .apply_capabilities(&echo_agent::AgentProfile {
            enabled: true,
            ..Default::default()
        })
        .await;

    // ---- Plugin host: mount built-in modules as plugins ----
    // 插件化组合根：每个内置模块（工具集/技能/插件实例/编排/管理面/LLM/Loop）
    // 以 PluginManifest + mount 闭包挂入 PluginHost。实化插件
    //（tools.builtin / skills.dir / checklist / instance.qq / management.panel /
    // loop.runner）的 mount 闭包执行真实副作用（禁用即卸载效果）；
    // provider.llm 仍为名义挂载（重启生效）。
    //
    // 插件实例的接线（hook/handler/config store）在插件挂载之后才完成；
    // qq_wired 标志保证启动期 mount 不抢跑启动插件实例（启动期由接线后的
    // 门控启动负责），运行期 TogglePlugin 才真正 start/stop。
    let qq_wired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let qq_running = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // 回合驱动：**由 `echo-plugin-loop` 在挂载时 provide**，本块结束时解析出来。
    // 声明在这里（块外）是为了后面每个人格接线（`set_loop_runner`）用同一实例。
    let loop_runner: Arc<echo_loop::TurnRunner>;
    // 宿主能力的注册句柄：**必须活到进程结束**。
    //
    // 服务是 RAII 的（`Disposer` 落下即注销），而插件在 `mount` 里按服务键
    // 解析宿主能力、注册表在服务变化时会 `reconcile`——把这个 Vec 放在内层
    // block 里，block 一结束服务就全被注销，**插件会被级联卸载**（真实踩到：
    // 管理面 WS 起监听后立刻被卸掉，面板连不上）。与 `loop_runner` 同一模式：
    // 声明在外层、赋值在内层。
    let _host_service_keep: Vec<echo_context::Disposer>;
    {
        use echo_plugin::MountContext;

        let plugin_host = plugin_host.clone();
        // persona 管理器：门控（挂载/卸载事件的反应）与宿主能力注册都要它。
        let gating_manager = supervisor.manager();
        // 挂载上下文只带服务定位器（框架层零领域依赖；`tools`/`skills` 两个
        // sink 是死代码，2026-10 M2 删除——见 echo_agent::plugins 模块文档）。
        plugin_host.set_mount_ctx(MountContext::new().with_ctx(ctx.clone()));
        // 插件事件出口（plugin-framework v0.7）：插件 `emit` 的东西（含注册表
        // 自己发的 `plugin.mounted` / `plugin.unmounted`）转成
        // `BackendEvent::PluginEvent` 走进程级汇聚点到面板。
        //
        // **必须在任何 mount 之前**：挂载本身就会 emit，晚接线会漏掉"启动时
        // 谁挂上了"这一段（那正是排查装配问题时最想看的部分）。
        {
            let sink = event_sink.clone();
            // 门控接在这里（全插件化 S6）：`plugin.mounted` / `plugin.unmounted`
            // → 逐 persona 重算可用性。此前是 6 个插件各收一个 `gate` 闭包，
            // 于是"新增插件"要记得写第 7 份；现在一处逻辑覆盖全部插件——包括
            // 组合根根本不认识的外部插件。
            //
            // 安全前提：`reapply_plugin_gating` 内部会再算一次
            // `persona_allows_plugin`，所以"挂上 = 全局可用"不会突破白名单。
            let gating_manager = gating_manager.clone();
            plugin_host.set_event_sink(std::sync::Arc::new(
                move |event: echo_plugin::PluginEvent| {
                    crate::host_services::gating_from_plugin_event(&gating_manager, &event);
                    sink(echo_protocol::BackendEvent::PluginEvent {
                        plugin: event.plugin,
                        name: event.name,
                        payload: event.payload,
                        at_ms: epoch_millis(),
                    });
                },
            ));
        }

        // 插件清单的来源层（面板展示「这行配置来自 plugins.toml 的哪一层」）：
        // 配置未提及的插件保持空串 = 编译内置的缺省行为。
        plugin_host.set_config_origins(plugins_runtime::config_origins(&args.config_path()));
        // 配置下发（2026-11）：`plugins.toml` 里 `kind = "builtin"` 行的
        // `[plugin.config]` 按 id 送进注册表，`mount` 时注入
        // `MountContext.config`——内置插件因此能"改配置不改代码"，且各拿
        // 各的那一份（不必自己去解析分层合成结果）。
        // 必须在任何 mount 之前：`mount` 取的是当时登记的值。
        plugin_host.set_configs(plugins_runtime::builtin_configs(&args.config_path()));

        // 先应用持久化的禁用状态再挂载：register_and_mount 对禁用插件只注册
        // 不挂载，禁用的插件在启动时不获得任何副作用。
        plugin_host
            .registry
            .apply_disabled(&cfg.agent.disabled_plugins);
        // `plugins.toml` 里 `enabled = false` 的外部条目：注册进清单但**不挂载**。
        // 必须在 mount_plan 之前收敛——mount_plan 会跳过处于禁用态的条目。
        // 这是配置选择（不是故障），面板显示「已禁用」；要真正停掉子进程得
        // 重启 Core（子进程的生死由 `plugins_runtime` 在启动期决定）。
        if let Some(runtime) = &plugin_runtime {
            plugin_host.disable_plugins(
                &runtime
                    .entries
                    .iter()
                    .filter(|entry| entry.kind != "builtin" && !entry.enabled)
                    .map(|entry| entry.id.clone())
                    .collect::<Vec<_>>(),
            );
        }

        let version = env!("CARGO_PKG_VERSION");
        // 配置化组合（P4）：内置插件的**构造**留在代码里（mount 闭包要
        // 捕获运行期句柄：端口 / token / 实例句柄 / broker…），但**集合、
        // 顺序、启用状态**交给 plugins.toml——构造结果先进"内置目录"，
        // 循环结束后由一次 mount_plan 按计划挂载。
        // ── 内置插件目录（构造在 `builtin_plugins`，装载计划在 `plugins.toml`）──
        //
        // 这一段此前是八个「实化 N」段（378 行）。搬走的理由不是"main 太长"，
        // 而是**归属**：`plugins.toml` 决定"装哪些、什么顺序"，代码决定"目录
        // 里有什么、各自 mount 做什么"。两者放在一起时，"新增一个内置插件要
        // 改哪里"没有答案。
        // ── 宿主能力 → 服务表（全插件化 S6）──
        //
        // 插件构造不再带参数：每个插件在自己的 `mount` 里按**它声明的键**
        // 解析宿主能力（`MountContext::require`）。组合根因此只剩一件事——
        // 把手里这些"配置表达不了、只有装配期知道"的句柄按同一批键注册。
        //
        // 顺序要求：**必须在任何 mount 之前**。注册晚了的表现为插件
        // `MissingService{available}` 挂不上（错误里带着当时已有的键，好查）。
        _host_service_keep = crate::host_services::register(
            &ctx,
            crate::host_services::HostWiring {
                manager: gating_manager.clone(),
                agent: agent.clone(),
                event_sink: event_sink.clone(),
                federation: federation_ports.clone(),
                instance: qq_instance.clone(),
                instance_wired: qq_wired.clone(),
                instance_running: qq_running.clone(),
                bridge: bridge.clone(),
                management_address: cfg.core.management_address.clone(),
                management_token: cfg.core.management_access_token.clone(),
                merged_federation: if merged { Some(merge_fed.clone()) } else { None },
            },
        );

        // ── 作用域树（2026-11 S10：对齐 dsh/Cordis 的 `ctx.fork()`）──
        //
        // 根作用域 = 进程级：上面注册的那张服务表就是它的 `Ctx`。人格作用域
        // 从它 `child()` 派生（父链继承、同名键遮蔽），各自挂"按人格才说得通"
        // 的插件（`kind ∈ {tool, skill, provider}`）。
        //
        // 目录**工厂**而不是目录：插件实例内部持有 `ServiceSlot`（mount 时绑定
        // 宿主适配器）。同一个实例挂进两层会让两层的槽互相覆盖——第一个人格的
        // 适配器被第二个人格顶掉，且无任何报错。所以每层重新构造一批。
        plugin_host.attach_root_ctx(ctx.clone());
        {
            let version = version.to_string();
            plugin_host.set_persona_catalog(std::sync::Arc::new(move || {
                match crate::builtin_plugins::catalog(&version) {
                    Ok(catalog) => catalog,
                    Err(error) => {
                        // 目录构造失败（命令域重名 / 抢了保留命令）= 装配 bug，
                        // 启动期已经因此失败过；走到这里只可能是运行期重建，
                        // 记 error 并返回空目录（该人格没有插件作用域，命令
                        // 回落进程级，不会静默用错适配器）。
                        tracing::error!(%error, "persona 插件目录构造失败");
                        Vec::new()
                    }
                }
            }));
        }

        // ── 内置插件目录：零参数（纯静态：这个二进制里有什么）──
        let mut catalog = crate::builtin_plugins::catalog(version)?;
        // 配置驱动的两项在目录之后追加：平台实例（id 由 `[instances.<平台>]`
        // 派生）与外部条目（`plugins.toml` 的非 builtin 行）——它们的"存在"
        // 取决于这份部署的配置，不该混进静态目录的签名。
        if let Some(platform) = (!platform.is_empty()).then_some(platform.as_str()) {
            catalog.push(crate::builtin_plugins::instance_plugin(version, platform));
        }
        for entry in plugin_runtime
            .as_ref()
            .map(|runtime| {
                runtime
                    .entries
                    .iter()
                    .filter(|entry| entry.kind != "builtin")
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
        {
            let entry = crate::builtin_plugins::ExternalEntry {
                id: entry.id.clone(),
                name: entry.name.clone(),
                kind: entry.kind.clone(),
            };
            catalog.push(crate::builtin_plugins::external_plugin(
                version, &entry,
            ));
        }

        // ── 按计划挂载（配置化组合 P4 的落点）──
        // 计划来自 plugins.toml 里 kind = "builtin" 的行：出现的按行序挂载、
        // 可 `enabled = false` 关掉；**没提到的一律照旧挂载**（现网
        // plugins.toml 大多只登记子进程插件，绝不能因此关掉内置功能）。
        // 无 plugins.toml / 无 builtin 行 → 计划为空 = 目录顺序全启用，
        // 与改造前逐字一致。
        let plan = plugins_runtime::builtin_mount_plan(&args.config_path());
        let catalog_ids: Vec<String> = catalog
            .iter()
            .map(|plugin| plugin.manifest().id.clone())
            .collect();
        let (order, diagnostics) = echo_agent::plugins::resolve_mount_order(&catalog_ids, &plan);
        for diagnostic in &diagnostics {
            warn!(%diagnostic, "builtin mount plan");
        }
        // 就绪等待（plugin-framework v0.6）：每个插件挂载前先等它的
        // `inject` 就绪，最多 2s。现网正常启动时依赖早已注册，第一次检查
        // 就命中（开销 = 一次 contains）；只有在组合根装配顺序将来被打乱、
        // 或依赖由异步任务稍后注册时，这 2s 才是真的等待而不是立刻报错。
        // 等不到的不算失败——进 pending，依赖到了由变更订阅（见下）追平。
        let (registered, mounted, waiting, mount_errors) = plugin_host
            .mount_plan_when_ready(catalog, &order, std::time::Duration::from_secs(2))
            .await;
        // 等满超时仍然缺依赖 = 装配 bug（服务键改名、注册被挪到挂载之后、
        // 或压根忘了注册）。**启动照样失败**——与 v0.5 的"缺失即启动失败"
        // 同款守卫；区别只是先给 2s 让"稍后才注册"这种情况自愈。
        // 运行期的短暂缺失不在这里判：那个由 `reconcile` 事后追平。
        let mut mount_errors = mount_errors;
        for (id, keys) in &waiting {
            mount_errors.push(format!(
                "plugin {id} still waiting for injected services {keys:?} after {:?}",
                std::time::Duration::from_secs(2)
            ));
        }
        if !mount_errors.is_empty() {
            // 与改造前逐插件 `?` 同语义：装配失败就启动失败——带着缺失的
            // 管理面/工具集继续跑只会让人以为"服务起来了但一片空白"。
            return Err(anyhow::anyhow!(mount_errors.join("; ")).context("mount builtin plugins"));
        }
        // 声明式注入的**响应端**（plugin-framework v0.6）：服务表的增删
        // 会沿父链广播 `Change`，这里把变化交给 `reconcile`——依赖到位的
        // pending 插件自动挂上、依赖消失的自动卸下。没有这条订阅，`inject`
        // 就只是"启动期检查一次"，而不是声明式。
        //
        // 重入由 `PluginHost::reconcile` 内部的重入位挡住（挂载会再注册服务）；
        // 订阅本身常驻进程生命周期，故意不保存 disposer（= 不撤销）。
        {
            let host = plugin_host.clone();
            let sub = ctx.on_change(move |change| {
                let report = host.reconcile();
                if !report.is_noop() {
                    info!(
                        service = %describe_change(&change),
                        mounted = ?report.mounted,
                        unmounted = ?report.unmounted,
                        "plugin tree reconciled after service change"
                    );
                }
            });
            std::mem::forget(sub);
        }

        // **`loop` 服务的唯一消费者**：驱动由插件 provide（见上方注释），宿主
        // 在挂载完成后才解析它。解析失败 = 插件没装/挂载失败——**启动失败**
        // （没有驱动的 agent 只会收消息不干活，静默比崩溃难查）。
        loop_runner = ctx
            .resolve::<Arc<echo_loop::TurnRunner>>("loop")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "`loop` 服务未提供：echo-plugin-loop 未挂载或挂载失败（驱动由该插件 provide）"
                )
            })?;

        // 子进程插件生命周期控制面：面板的启停开关据此真正停/起子进程，
        // 重启按钮据此 stop + start（注册表本身不持有进程句柄）。
        if let Some(runtime) = &plugin_runtime {
            plugin_host.set_subprocess_control(std::sync::Arc::new(
                plugins_runtime::SubprocessControl::new(runtime),
            ));
        }
        // 定义来源（面板技术信息）：外部条目报「plugins.toml 路径 + 启动命令 +
        // 环境变量键名」——排查「为什么不起来」不必翻配置文件。
        if let Some(runtime) = &plugin_runtime {
            let plugins_toml = args
                .config_path()
                .with_file_name("plugins.toml")
                .display()
                .to_string();
            plugin_host.set_definitions(
                runtime
                    .entries
                    .iter()
                    .filter(|e| e.kind != "builtin")
                    .map(|entry| {
                        (
                            entry.id.clone(),
                            echo_plugin::PluginDefinition {
                                source_path: plugins_toml.clone(),
                                command: entry.command.clone(),
                                args: entry.args.clone(),
                                env_keys: entry.env_keys.clone(),
                            },
                        )
                    }),
            );
        }
        // 外部条目的启动失败原因 + 配置来源层（面板据此区分「配置关了」
        // 与「起不来」，并指出该去改哪一层）。
        if let Some(runtime) = &plugin_runtime {
            plugin_host.set_plugin_errors(
                runtime
                    .entries
                    .iter()
                    .filter_map(|entry| entry.error.clone().map(|error| (entry.id.clone(), error))),
            );
        }
        info!(
            registered,
            mounted = ?mounted,
            plugins = ?plugin_host.descriptors().iter().map(|d| d.id.clone()).collect::<Vec<_>>(),
            "plugins mounted"
        );
    }

    // ---- Background shell sessions（进程级，面板 + 工具共用）----
    {
        use echo_agent::shell::{ShellEvent, ShellManager};
        let mgr = std::sync::Arc::new(ShellManager::new());
        // 事件广播：接到默认 agent 的 handle，Panel 单连接即可实时可视化。
        let shell_emitter = core_agent.clone();
        let emit = std::sync::Arc::new(move |event: ShellEvent| {
            shell_emitter.emit(echo_agent::shell::shell_event_to_backend(event));
        });
        // 绑定到共享句柄：工具侧与命令侧读同一份（不再是进程级锚点）。
        shell_handle.bind(std::sync::Arc::clone(&mgr), emit);
        info!("background shell manager ready");
    }

    // （插件门控判定读的是各 persona 注入的宿主，不再需要进程级锚点——
    //   2026-10 技术债清理：`plugin_host_global` 已删除。）
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
        persona.agent.set_loop_runner(loop_runner.clone());
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
                let incoming = echo_plugin_adapter::message_to_incoming(message);
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
                // 回退默认 persona——未登记实例的插件实例不能丢状态。
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
    // 进程级命令（团队 CRUD / API 改名）：**不属于任何人格**，在路由到 agent
    // 之前先问这里。
    //
    // 为什么在循环里惰性解析 manager 而不是进循环前拿一次：泵在装配早期就
    // spawn 了，而 manager 更晚才就绪（同一个 `agent_manager_slot` 弱引用）。
    // 惰性解析的代价是每条命令一次 `Weak::upgrade` —— 可忽略。
    let pump_slot = agent_manager_slot.clone();
    let pump = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            while let Some(cmd) = pump_handle.try_recv_command() {
                // 进程级命令先处理：命中即 `continue`，不落到任何 agent 上。
                // `admin` / `registry` 是同一个 manager 的两个面向。
                if let Some(manager) = pump_slot.read().ok().and_then(|w| w.upgrade()) {
                    let host = echo_agent::process_commands::ProcessCommandHost {
                        admin: manager.clone(),
                        registry: manager,
                        emitter: pump_core.clone(),
                    };
                    if echo_agent::process_commands::try_handle(&cmd, &host).await {
                        continue;
                    }
                }
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
            let runner = loop_runner.clone();
            mgr_arc.set_factory(move |id: String, p: AgentProfile| {
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
            // 每个 agent 绑定所属 manager（弱引用；技债清理：不再有进程级锚点）。
            // `bind_all` 覆盖注册表内的全部人格；`core_agent` 不是人格、
            // 不在注册表里，必须单独绑。
            mgr_arc.bind_all(&mgr_arc);
            // `core_agent` 是管理面/TeamsList 的命令宿主，但不是人格，
            // 不包含在 `mgr_arc.all()` 中；漏绑会让 Panel 收到空 TeamsList。
            //
            // S4b：绑的是**两个收窄后的面向**（查询 / 管理），不是 manager 本身。
            {
                let registry: std::sync::Arc<dyn echo_agent::agent::registry::AgentRegistry> =
                    mgr_arc.clone();
                core_agent.set_registry(&registry);
            }
            // 联邦运行时也绑定同一 manager（迁移需要枚举 source persona）。
            if let Ok(mut slot) = agent_manager_slot.write() {
                *slot = std::sync::Arc::downgrade(&mgr_arc);
            }
        }
    }
    // ---- Auto-start instance instances if enabled ----
    // 接线完成，标记 wired：此后 instance.qq 插件的运行期 enable 才会真正
    // start 插件实例（启动期 mount 不抢跑）。启动受配置与插件状态双重门控。
    qq_wired.store(true, std::sync::atomic::Ordering::SeqCst);
    // 没有平台配置时**根本没有**实例插件条目（目录里不建幽灵插件），所以
    // 这里直接算作"未启用"，不去问一个不存在的 id。
    let qq_plugin_enabled = !platform.is_empty()
        && agent
            .plugin_host()
            .registry
            .is_enabled(&crate::instance_manifests::plugin_id_for(&platform));
    if qq_host.enabled && qq_plugin_enabled {
        // 多实例：逐个生成 compose（非 legacy）并启动；单个失败不影响其余。
        for (instance_id, persona_id, instance) in &qq_handles {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `--config` 是 global：前置后置等价。
    ///
    /// 锁住的是**实际踩过的坑**——`global` 未设时 clap 只接受
    /// `-c x dump-config`，而人（与文档）自然写成 `dump-config -c x`，
    /// 得到 "unexpected argument '--config' found"。三个子命令共享同一
    /// 选项，任一处退化都会让它们全部复发。
    #[test]
    fn config_flag_is_accepted_before_and_after_subcommand() {
        let before = Args::try_parse_from(["echo-agent-core", "-c", "a.toml", "dump-config"])
            .expect("flag before subcommand");
        let after = Args::try_parse_from(["echo-agent-core", "dump-config", "-c", "a.toml"])
            .expect("flag after subcommand");
        assert!(matches!(before.command, Some(SubCommand::DumpConfig)));
        assert!(matches!(after.command, Some(SubCommand::DumpConfig)));
        // 两种写法必须落到同一个路径——否则"后置能解析但被忽略"会静默读错文件。
        let expected = PathBuf::from("a.toml");
        assert_eq!(before.config_path(), expected);
        assert_eq!(after.config_path(), expected);
    }

    /// 子命令缺省（无 command）= 启动服务；`dump-config` 不吞掉 `token` /
    /// `port`（clap 子命令互斥，这里锁住"新增子命令未打乱既有两条"）。
    #[test]
    fn existing_subcommands_keep_parsing() {
        let token =
            Args::try_parse_from(["echo-agent-core", "token", "--rotate"]).expect("token --rotate");
        assert!(matches!(
            token.command,
            Some(SubCommand::Token { rotate: true })
        ));
        let service = Args::try_parse_from(["echo-agent-core"]).expect("bare invocation");
        assert!(service.command.is_none(), "no subcommand = run the service");
    }

    /// 缺省配置路径仍是 `config/echo-agent-core.toml`（服务与子命令同源）。
    #[test]
    fn default_config_path_is_unchanged() {
        let args = Args::try_parse_from(["echo-agent-core"]).expect("bare invocation");
        assert_eq!(
            args.config_path(),
            PathBuf::from("config/echo-agent-core.toml")
        );
    }
}
