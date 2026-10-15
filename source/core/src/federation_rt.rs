//! 联邦运行时（组合根侧）：路由泵、联邦命令 / 查询处理、自更新、对端地址
//! 学习与邀请占位提升、远程代理工具注册。
//!
//! 2026-10 架构评审：自 `main.rs`（3800 行）拆出，纯搬移、行为不变。

#[allow(unused_imports)]
use crate::*;
#[allow(unused_imports)]
use anyhow::Context;

/// 联邦路由上下文（Phase 2）：路由泵与代理工具注册共用的句柄束。
pub(crate) struct FederationRuntime {
    pub(crate) federation: Arc<echo_federation::Federation>,
    pub(crate) router: Arc<echo_agent::federation::InvokeRouter>,
    /// peer 配置名 → node_id（Link Up 时登记；代理工具命名用配置名）。
    pub(crate) peer_names:
        std::sync::Arc<tokio::sync::RwLock<std::collections::HashMap<String, String>>>,
    /// 反向映射：node_id → peer 配置名（remove_peer 断链用）。
    pub(crate) node_to_peer:
        std::sync::Arc<tokio::sync::RwLock<std::collections::HashMap<String, String>>>,
    /// 联邦管理命令所需的配置写回与本机信息。
    pub(crate) config_store: echo_adapter::ConfigStore,
    pub(crate) listen: String,
    /// 邀请串对外宣告地址（`[federation] advertise`；空 = 从 listen 推导）。
    pub(crate) advertise: String,
    pub(crate) node_name: Option<String>,
    /// per-peer 执行策略（allow_tools/queries 等）——SaveFederationPeer
    /// 运行时更新即生效（此前启动期冻结会让白名单静默失效到重启）。
    pub(crate) peer_policies: std::sync::Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<String, echo_agent::federation::ExecutorPolicy>,
        >,
    >,
    /// 远程代理工具的注册保活句柄：node_id → 该 peer 的全部 Disposer
    /// （drop 即按名注销——必须持有到进程结束；Down 整包禁用不注销）。
    ///
    /// **按 peer 分桶**（2026-10 修复）：此前为单一全局 Vec、每次注册
    /// 整体 clear——多 peer 场景下任一对端注册/重连都会把其他对端的工具
    /// 一并注销；且「先插后删」时旧句柄按名移除会把同名的**新**注册条目
    /// 顶掉（连续重连后所有对端的远程工具全部消失，spawn_subagent node=
    /// 报「无可用代理工具」——活证据见当日日志）。
    pub(crate) remote_tool_keepers:
        std::sync::Mutex<std::collections::HashMap<String, Vec<echo_context::Disposer>>>,
    /// 端口合并（2026-10）：Some(management_access_token) = 合并模式——
    /// `SaveFederationPeer` 拒绝与之一致的 peer token（升级回调无法区分
    /// 管理/联邦连接）；None = 独立模式（无此校验）。
    pub(crate) merged_management_token: Option<String>,
    /// 邀请占位登记表（占位名 → 创建时刻，运行时-only；2026-10 审计）：
    /// 显式记账替代「名字前缀 invite-」猜测——用户配置名含该前缀时不再被
    /// 误提升/误覆盖权限；超时（1h）未配对的占位在生成新邀请时顺带清扫
    /// （此前只增不减，面板出现无法取消的幽灵 peer）。
    pub(crate) invite_placeholders:
        std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

impl FederationRuntime {
    /// 按名字解析 node_id（2026-10 审计 P2-③）：模型/标注回传的 peer 名
    /// 可能是**工具名清洗形**（空格/点/中文 → `_`，如 `my_node`），而
    /// `peer_names` 键是原始配置名（`my node`）——直接 get 会失配、通知
    /// 静默丢弃。先精确匹配，再按清洗规则反查原始配置名。
    pub(crate) async fn resolve_peer_node(&self, peer: &str) -> Option<String> {
        let map = self.peer_names.read().await;
        if let Some(id) = map.get(peer) {
            return Some(id.clone());
        }
        map.iter()
            .find(|(name, _)| echo_agent::federation::sanitize_peer_component(name) == peer)
            .map(|(_, id)| id.clone())
    }

    /// 实时拉取对端负载（Query(NodeStatus) 3s 短超时；失败 None 退快照）。
    pub(crate) async fn fetch_live_load(&self, peer_name: &str) -> Option<u32> {
        let node_id = self.peer_names.read().await.get(peer_name).cloned()?;
        let (request, pending) = self.router.query(
            &node_id,
            echo_federation::QueryKind::NodeStatus,
            String::new(),
            String::new(),
            0,
            0,
        );
        self.federation
            .send_to(&node_id, echo_federation::FedFrame::Query(request))
            .await
            .ok()?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), pending.wait())
            .await
            .ok()?
            .ok()?;
        serde_json::from_str::<serde_json::Value>(&result.output)
            .ok()?
            .get("active_turns")?
            .as_u64()
            .map(|n| n as u32)
    }

    /// 读某 peer 配置名的执行策略（`peer_policies` 按配置名存；
    /// 启动装配与 `SaveFederationPeer` 运行时同步同键）。异步上下文里
    /// 这只是对 RwLock 的一次 read——独立成方法是为让调用点（如
    /// SubagentSpawn 门控）意图清晰、便于单测挂载点。
    pub(crate) async fn peer_policy_for(
        &self,
        peer_name: &str,
    ) -> Option<echo_agent::federation::ExecutorPolicy> {
        self.peer_policies.read().await.get(peer_name).cloned()
    }

    /// 组装 `FederationStatus` 事件（token 脱敏为 token_set 标记）。
    pub(crate) async fn status_event(&self) -> echo_protocol::BackendEvent {
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
                //
                // 2026-10 联邦组建向导：同一遍历顺带取出对端 node_id
                // （在线时映射必然存在——node_to_peer 在链路 Up 登记、
                // Down 移除）——回传 Panel 做「两节点是否已互联」的
                // 精确判定与拓扑渲染。
                let matched_node: Option<String> = self
                    .node_to_peer
                    .try_read()
                    .ok()
                    .and_then(|m| online_node_for_peer(&m, &active, &p.name));
                let online =
                    matched_node.is_some() || self.federation.is_link_token_online(&p.token).await;
                // 策略字段回传运行时值（此前恒空/false——前端基于状态
                // 回写保存会抹掉已配置的白名单）。
                let policy = self.peer_policies.read().await.get(&p.name).cloned();
                // 负载回传（P2 调度器输入）：在线 peer 经联邦
                // Query(NodeStatus) 实时拉取（3s 短超时，失败/离线退
                // 握手快照）——链路存活期负载不再失真到重连。
                let snapshot = self
                    .federation
                    .peer_caps(&p.name)
                    .await
                    .map(|c| c.active_turns)
                    .unwrap_or(0);
                let active_turns = if online {
                    match self.fetch_live_load(&p.name).await {
                        Some(live) => live,
                        None => snapshot,
                    }
                } else {
                    snapshot
                };
                // 连出健康诊断（2026-10 地址漂移事故）：失败计数/最后错误/
                // 最后在线时刻 + 最近学到的对端宣告地址——Panel 把"持续
                // 连不上"显性化并给出"改用对端宣告地址"的修复路径。
                let diag = self.federation.peer_diagnostics(&p.name).await;
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
                    active_turns,
                    node_id: matched_node,
                    consecutive_failures: diag.consecutive_failures,
                    last_error: diag.last_error,
                    last_online_at: diag.last_online_at,
                    last_advertise: diag.last_advertise,
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
pub(crate) async fn federation_router_pump(
    mut rx: tokio::sync::mpsc::Receiver<echo_federation::LinkEvent>,
    rt: Arc<FederationRuntime>,
    personas: Arc<agent_supervisor::AgentSupervisor>,
    core_agent: Arc<echo_agent::Agent>,
    peer_policies: std::sync::Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<String, echo_agent::federation::ExecutorPolicy>,
        >,
    >,
) {
    // 执行端结果回传通道：handle_invoke spawn 的执行任务 → 本泵统一发送。
    let (outbound_tx, mut outbound_rx) =
        tokio::sync::mpsc::channel::<(String, echo_federation::FedFrame)>(64);
    // 目标侧会话导入缓冲（P3-2b）：transfer_id → 有序分块。
    let mut imports: std::collections::HashMap<String, federation_import::ImportBuffer> =
        std::collections::HashMap::new();
    // 导入缓冲过期清扫（2026-10 巡检 P1）：对端断链/中途放弃的残片定期回收
    // （此前仅在凑齐时移除，残片永久驻留）。
    let mut import_sweep = tokio::time::interval(std::time::Duration::from_secs(60));
    import_sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = import_sweep.tick() => {
                let dropped = federation_import::sweep_stale_imports(&mut imports);
                if dropped > 0 {
                    tracing::warn!(
                        target: "federation",
                        dropped,
                        "stale session imports swept (peer disconnected or abandoned)"
                    );
                }
            }
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
                        // 邀请占位提升（2026-10）：本链路若来自占位 token，
                        // 把占位**提升为配置条目**（满权限默认 + 落配置 +
                        // 可在 Panel 编辑）——此前配对即删除占位，导致
                        // 连入方向既无配置也无策略（全部静默按默认全拒）、
                        // 对端重启后还因 token 消失无法重连。
                        promote_paired_invite_placeholders(
                            &rt,
                            &mut *peer_policies.write().await,
                        )
                        .await;
                        // 策略按**解析后的配置名**取（提升后为新配置名；
                        // 普通配对为对端在本机配置里的名字——接受侧
                        // peer_name 已由 link.rs 按 token 解析为配置名）。
                        let resolved_name = rt
                            .node_to_peer
                            .read()
                            .await
                            .get(&info.node_id)
                            .cloned()
                            .unwrap_or_else(|| info.peer_name.clone());
                        let policy = peer_policies
                            .read()
                            .await
                            .get(&resolved_name)
                            .cloned()
                            .unwrap_or_default();
                        rt.router.set_policy(&info.node_id, policy);
                        // 传**解析后的配置名**（非 info.peer_name）：占位提升后
                        // 两者不同——此前按旧占位名注册工具/包，而 Down 按新名
                        // 禁用（set_package_enabled 命中 0），对端离线后 LLM 仍见
                        // invite-xxxx__* 且包未禁（2026-10 审计 P0-1）。
                        register_remote_tools(&rt, &personas, &info, &resolved_name).await;
                        // 学习对端宣告的拨号地址（配对无方向性，2026-10）：
                        // 对端 Hello 携带其对外地址 → 本机若无该 peer 地址
                        // 则补上（运行时 + 落配置）→ 双方都会主动重连。
                        learn_peer_dial_address(&rt, &info).await;
                        broadcast_federation_status(&rt, &personas, &core_agent).await;
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
                        broadcast_federation_status(&rt, &personas, &core_agent).await;
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
                            // 执行端门控（Phase 3 消费）：peer 未授
                            // allow_subagent 时拒绝受理——此前只记审计就放行，
                            // 配置的 allow_subagent = false 形同虚设。拒绝以
                            // SubagentEvent(Failed) 回执，调用方聚合器按失败
                            // 终态销账（不会悬挂等待）。
                            let peer_name = rt
                                .node_to_peer
                                .read()
                                .await
                                .get(&from)
                                .cloned();
                            let allowed = match peer_name.as_ref() {
                                Some(n) => rt
                                    .peer_policy_for(n)
                                    .await
                                    .map(|p| p.allow_subagent)
                                    .unwrap_or(false),
                                None => false, // 未知 peer：fail-closed
                            };
                            if !allowed {
                                tracing::warn!(
                                    target: "federation",
                                    from = %from, call_id = %req.call_id,
                                    "remote subagent rejected (allow_subagent not granted)"
                                );
                                let reply = echo_federation::FedFrame::SubagentEvent(
                                    echo_federation::SubagentEventFrame {
                                        call_id: req.call_id.clone(),
                                        status: echo_federation::SubagentStatus::Failed,
                                        result: Some(format!(
                                            "peer {} 未授予远程委派（allow_subagent = false）",
                                            peer_name.as_deref().unwrap_or(&from)
                                        )),
                                    },
                                );
                                if rt.federation.send_to(&from, reply).await.is_err() {
                                    tracing::warn!(target: "federation", peer = %from, "subagent reject reply send failed");
                                }
                                continue;
                            }
                            // 执行端观测：对端大脑委派的远程子任务在本机
                            // 开始——记审计（Panel 后台任务列表的远程条目
                            // 展示留待后续迭代）。
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
                            // P3-3 聚合销账：组内全部终态时，聚合摘要
                            // 投递父会话 timeline（assistant 系统消息）。
                            if let Some(summary) =
                                echo_agent::federation::aggregator::settle_remote_subagent(
                                    &ev.call_id,
                                    ev.status,
                                    ev.result.clone(),
                                )
                            {
                                federation_import::deliver_aggregate_summary(&personas, summary).await;
                            }
                        } else if let echo_federation::FedFrame::SessionImport(import) = &frame {
                            // 目标侧（P3-2b）：聚合分块 + 每块 ack（流控/重发）；
                            // 凑齐后 spawn_blocking 导入，终态经 Result 回源。
                            let (acked, outcome) =
                                federation_import::handle_session_import(&personas, import, &mut imports);
                            let ack = echo_federation::FedFrame::SessionImportAck(
                                echo_federation::SessionImportAckFrame {
                                    transfer_id: import.transfer_id.clone(),
                                    acked_upto: acked,
                                },
                            );
                            if rt.federation.send_to(&from, ack).await.is_err() {
                                tracing::warn!(
                                    target: "federation", peer = %from,
                                    "session import ack send failed"
                                );
                            }
                            match outcome {
                                federation_import::ImportOutcome::Pending => {}
                                federation_import::ImportOutcome::Failed(reason) => {
                                    let reply = federation_import::session_import_result(
                                        &import.transfer_id,
                                        &import.session_id,
                                        &import.target_peer,
                                        false,
                                        0,
                                        reason,
                                    );
                                    if rt.federation.send_to(&from, reply).await.is_err() {
                                        tracing::warn!(
                                            target: "federation", peer = %from,
                                            "session import failure reply send failed"
                                        );
                                    }
                                }
                                federation_import::ImportOutcome::Ready(ready) => {
                                    let rt2 = rt.clone();
                                    let from2 = from.clone();
                                    tokio::spawn(async move {
                                        federation_import::run_session_import(rt2, from2, *ready).await;
                                    });
                                }
                            }
                        } else if let echo_federation::FedFrame::SessionImportAck(ack) = &frame {
                            // 源侧（P3-2b）：目标 ack → 推进本任务窗口。
                            federation_import::note_session_import_ack(&ack.transfer_id, ack.acked_upto);
                        } else if let echo_federation::FedFrame::SessionImportResult(result) = &frame
                        {
                            // 源侧（P3-2b）：目标终态回执 → 面板结果事件。
                            let message = if result.success {
                                format!(
                                    "迁移完成：导入 {} 条事件（{}）",
                                    result.imported_events, result.message
                                )
                            } else {
                                format!("迁移失败：{}", result.message)
                            };
                            let event = echo_protocol::BackendEvent::SessionMigrated {
                                session_id: result.session_id.clone(),
                                target_peer: result.target_peer.clone(),
                                new_session_id: result
                                    .success
                                    .then(|| result.session_id.clone()),
                                message,
                                success: result.success,
                            };
                            if let Some(persona) = personas.personas().into_iter().next() {
                                persona.agent.emit(event);
                            } else {
                                core_agent.emit(event);
                            }
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
pub(crate) async fn peer_package(rt: &FederationRuntime, peer_node: &str) -> String {
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
pub(crate) async fn broadcast_federation_status(
    rt: &FederationRuntime,
    personas: &agent_supervisor::AgentSupervisor,
    core_agent: &Arc<echo_agent::Agent>,
) {
    // 进程级事件汇聚点：任一人格都到 Panel。无人格（全部被禁用）时回退到
    // 进程级核心服务代理 `__core`——否则联邦状态变化静默丢失。
    let event = rt.status_event().await;
    if let Some(persona) = personas.personas().into_iter().next() {
        persona.agent.emit(event);
    } else {
        core_agent.emit(event);
    }
}

/// 联邦管理命令处理（SaveFederationPeer / DeleteFederationPeer /
/// RequestFederationStatus / RequestFederationInvite）。
/// 联邦 / 自更新命令处理器（联邦始终开启；自更新与联邦共用此入口，但彼此
/// 独立——曾因"联邦关闭时整段不注册"导致自更新也被报"联邦未启用"）。
pub(crate) async fn handle_federation_command(
    emit: std::sync::Arc<dyn Fn(echo_protocol::BackendEvent) + Send + Sync>,
    cmd: echo_protocol::BackendCommand,
    rt: Arc<FederationRuntime>,
) {
    match cmd {
        echo_protocol::BackendCommand::RequestFederationStatus => {
            emit(rt.status_event().await);
        }
        echo_protocol::BackendCommand::RequestSelfUpdate => {
            handle_self_update(emit.clone()).await;
        }
        echo_protocol::BackendCommand::MigrateSession {
            session_id,
            team_id,
            target_peer,
        } => {
            federation_import::handle_migrate_session(
                emit.clone(),
                &rt,
                session_id,
                team_id,
                target_peer,
            )
            .await;
        }
        echo_protocol::BackendCommand::RequestSelfUpdateStatus => {
            emit(read_self_update_status().await);
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
            // 端口合并：peer token 不得与 management_access_token 相同
            // （合并监听端升级回调无法区分两类连接；启动期已校存量，
            // 此处拦运行时新增/更新）。
            if let Some(mgmt_token) = rt.merged_management_token.as_deref() {
                if !mgmt_token.is_empty() && token == mgmt_token {
                    emit(echo_protocol::BackendEvent::Error {
                        session_id: None,
                        message: "该 token 与 management_access_token 相同——端口合并下无法区分管理/联邦连接，请使用不同令牌（改 peer token 或 `echo-agent-core token --rotate`）"
                            .into(),
                    });
                    return;
                }
            }
            // URL 形状校验（2026-10 审计 P1-4）：坏地址保存成功但 dial 反复
            // 失败（仅 debug 日志），面板看不出配置错误——保存时直接拒绝。
            // 空 = 仅接受连入（合法）。
            if let Err(reason) = validate_federation_peer_url(&peer.url) {
                emit(echo_protocol::BackendEvent::Error {
                    session_id: None,
                    message: reason,
                });
                return;
            }
            // 同 token 双 peer 拒绝（2026-10 审计 P2-④）：在线判定
            // （is_link_token_online）与连入侧名字解析按 token 匹配——
            // 两个 peer 共用 token 会让二者歧义（在线状态互相串、连入
            // 解析到错误配置名）。
            {
                let conflict = rt
                    .federation
                    .peer_configs()
                    .await
                    .into_iter()
                    .find(|p| p.name != name && !p.token.is_empty() && p.token == token);
                if let Some(other) = conflict {
                    emit(echo_protocol::BackendEvent::Error {
                        session_id: None,
                        message: format!(
                            "该 token 已被 peer「{}」使用——同一 token 只能属于一个 peer（在线判定与连入名解析按 token 匹配）",
                            other.name
                        ),
                    });
                    return;
                }
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
                // Down 等价清理（2026-10 审计 P0-2）：drop_link 移除链路句柄
                // 后，链路自身退出时 still_mine=false **不再发 Down**——此前
                // 这里仅 drop_link，导致 router 在飞调用、工具包禁用、映射
                // 与保活桶全部跳过（删除在线 peer 后工具残留可调用、
                // spawn/node:// 反查文案错、在飞调用悬挂至超时）。
                rt.router.drop_peer(&node);
                // 保活桶移除 = 注销该 peer 全部远程工具（Disposer drop 按名
                // 移除；删除语义强于 Down 的"整包禁用"——peer 已从配置消失，
                // 不设重新启用路径）。
                {
                    let mut slot = rt
                        .remote_tool_keepers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    slot.remove(&node);
                }
                rt.node_to_peer.write().await.remove(&node);
                rt.peer_names.write().await.remove(&name);
                rt.federation.drop_link(&node).await;
                tracing::info!(
                    target: "federation",
                    peer = %node, name = %name,
                    "federation peer deleted (router pending failed; remote tools unregistered; mappings cleaned)"
                );
            }
            // 邀请占位登记表同步移除（运行时-only；2026-10）。
            {
                let mut reg = rt
                    .invite_placeholders
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                reg.remove(&name);
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
            let host = invite_host(&rt.listen, &rt.advertise);
            let Some(host_port) = host else {
                emit(echo_protocol::BackendEvent::Error {
                    session_id: None,
                    message: "联邦未监听（listen 为空）或无法推导对外地址；请在配置 [federation] 写 listen / advertise（对端可达地址）".into(),
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
            // 占位登记 + 存量清扫（2026-10 审计 P1-2）：未配对占位此前
            // 只增不减（面板幽灵 peer、无取消入口）——生成新邀请时顺带
            // 清理超过 1 小时且未配对的旧占位。
            let stale: Vec<String> = {
                let mut reg = rt
                    .invite_placeholders
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let stale: Vec<String> = reg
                    .iter()
                    .filter(|(_, created)| created.elapsed() > std::time::Duration::from_secs(3600))
                    .map(|(name, _)| name.clone())
                    .collect();
                for name in &stale {
                    reg.remove(name);
                }
                stale
            }; // 锁在 await 前释放（MutexGuard 非 Send）
            for name in stale {
                // 同步从运行时 peer 表移除（若仍在）。
                rt.federation.remove_peer(&name).await;
            }
            rt.invite_placeholders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(placeholder.clone(), std::time::Instant::now());
            emit(echo_protocol::BackendEvent::FederationInvite { invite });
        }
        _ => {}
    }
}

/// 联邦 peer URL 形状校验（`SaveFederationPeer` 保存前判；2026-10 审计）。
/// 空 = 仅接受连入（合法）；否则必须 `ws://host[:port]`（wss 同）。
pub(crate) fn validate_federation_peer_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(());
    }
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
        .ok_or_else(|| {
            format!("联邦 peer 地址必须以 ws:// 或 wss:// 开头（空 = 仅接受连入）：{url}")
        })?;
    let hostport = rest.split('/').next().unwrap_or("");
    let (host, port) = hostport
        .rsplit_once(':')
        .ok_or_else(|| format!("联邦 peer 地址缺少端口（应为 ws://host:port）：{url}"))?;
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(format!("联邦 peer 地址的主机或端口无效：{url}"));
    }
    Ok(())
}

/// 只读查询执行端（Phase 5）。
///
/// - `NodeStatus`：恒允许（无害遥测）
/// - `WorkspaceFiles`：`subject` = 绝对路径；复用 workspace 插件的
///   canonical 前缀校验（**限本机各 persona 工作区目录的并集**）后
///   `collect_dir_files` 采集
/// - `SessionSnapshot`：`subject` = 会话 id（可带 `node://` 前缀——剥离
///   按本机处理）；复用 trunk 时间线快照 + since_seq/limit 分页
pub(crate) async fn handle_federation_query(
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
            // 本机全部 persona 工作区目录并集内的 canonical 校验；
            // **完全开放（allow_queries = ["*"]）豁免沙箱**（2026-10
            // 设计演进）：互信对端可浏览本机任意可读目录（「SSH 免密」
            // 同语义），只需路径存在。
            let dir = req.subject.trim().to_string();
            let full_access = rt
                .router
                .policy_of(from)
                .map(|p| p.is_full_access())
                .unwrap_or(false);
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
                let resolved = if full_access {
                    let p = std::path::Path::new(&dir)
                        .canonicalize()
                        .map_err(|e| format!("路径不存在或不可访问: {e}"))?;
                    if !p.is_dir() {
                        return Err("路径不是目录".to_string());
                    }
                    Ok(p)
                } else {
                    echo_agent::workspace::resolve_within_directories(&all_dirs, &dir)
                }?;
                echo_agent::workspace::collect_dir_files(&resolved)
            })
            .await;
            match collected {
                Ok(Ok(entries)) => ok(serde_json::json!({"entries": entries})),
                Ok(Err(message)) => err(&message),
                Err(e) => err(&format!("采集失败: {e}")),
            }
        }
        QueryKind::WorkspaceGitStatus => {
            // 远程 git 状态（跨机工作区）：subject = 目录绝对路径；与
            // WorkspaceFiles 同一沙箱（限本机各 persona 工作区目录并集），
            // 同样在 allow_queries = ["*"] 完全开放时豁免（2026-10）。
            let dir = req.subject.trim().to_string();
            let full_access = rt
                .router
                .policy_of(from)
                .map(|p| p.is_full_access())
                .unwrap_or(false);
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
                let resolved = if full_access {
                    let p = std::path::Path::new(&dir)
                        .canonicalize()
                        .map_err(|e| format!("路径不存在或不可访问: {e}"))?;
                    if !p.is_dir() {
                        return Err("路径不是目录".to_string());
                    }
                    p
                } else {
                    echo_agent::workspace::resolve_within_directories(&all_dirs, &dir)?
                };
                Ok::<_, String>(echo_agent::workspace::collect_dir_git(
                    &resolved.to_string_lossy(),
                ))
            })
            .await;
            match collected {
                Ok(Ok(info)) => ok(serde_json::json!({"git": info})),
                Ok(Err(message)) => err(&message),
                Err(e) => err(&format!("采集失败: {e}")),
            }
        }
        QueryKind::ApiProfiles => {
            // 供应商池**脱敏**返回（api_key 明文永不过线）：分布式
            // 供应商发现/导入（2026-10）。含全局默认配置（active_api
            // 视角）与各 profile；key_set 布尔指示该条目是否已配 key
            // ——「哪个节点有什么供应商、哪个没配 key」开箱可见。
            let agents = personas.personas();
            let core_agent = agents.iter().find(|p| p.id == "__core").or(agents.first());
            let Some(core_agent) = core_agent else {
                return err("无可用 agent 读取供应商配置");
            };
            let cfg = core_agent.agent.api_config().await;
            let mut resolved = cfg.clone();
            resolved.apply_active_profile();
            let active = serde_json::json!({
                "name": cfg.active_api,
                "provider": resolved.provider,
                "model": resolved.model,
                "base_url": resolved.base_url,
                "key_set": !resolved.effective_api_key().is_empty(),
                "thinking": resolved.thinking,
                "reasoning_effort": resolved.reasoning_effort,
            });
            let profiles: Vec<serde_json::Value> = cfg
                .api_profiles
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "name": p.name,
                        "provider": p.provider,
                        "model": p.model,
                        "base_url": p.base_url,
                        "key_set": !p.api_key.is_empty(),
                        "thinking": p.thinking,
                        "reasoning_effort": p.reasoning_effort,
                    })
                })
                .collect();
            ok(serde_json::json!({
                "active": active,
                "profiles": profiles,
            }))
        }
        QueryKind::BrowseDirectories => {
            // 目录选择器浏览（2026-10）：与 ApiProfiles 同级的只读结构查询。
            // `subject` 空 = 根列表请求；否则列出该目录子目录（限浏览根）。
            let requested = req.subject.trim().to_string();
            // 浏览根 = 全部 persona 工作区目录并集 ∪ HOME（与大脑侧本机
            // 浏览同一逻辑——browse_roots helper）。
            let mut all_dirs: Vec<echo_protocol::WorkspaceDirectory> = Vec::new();
            let mut labels: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for persona in personas.personas() {
                if let Some(store) = persona.agent.workspace_store() {
                    let (sessions, _) = store.snapshot();
                    for s in sessions {
                        for dir in &s.directories {
                            labels
                                .entry(dir.path().to_string())
                                .or_insert_with(|| s.name.clone());
                            all_dirs.push(dir.clone());
                        }
                    }
                }
            }
            let roots = echo_agent::workspace::browse_roots(&all_dirs, &labels);
            if requested.is_empty() {
                return ok(serde_json::json!({
                    "roots": roots,
                    "entries": [],
                }));
            }
            let collected = tokio::task::spawn_blocking(move || {
                let resolved =
                    echo_agent::workspace::resolve_within_browse_roots(&roots, &requested)?;
                echo_agent::workspace::collect_child_dirs(&resolved)
            })
            .await;
            match collected {
                Ok(Ok(entries)) => ok(serde_json::json!({"roots": [], "entries": entries})),
                Ok(Err(message)) => err(&message),
                Err(e) => err(&format!("采集失败: {e}")),
            }
        }
        QueryKind::SessionSnapshot => {
            // 会话归属解析：`node://` 前缀剥离按本机处理。
            // team_id 必填：同名会话如 `local:tui::local_user` 每个 persona
            // 都有，不限定人格的查询有歧义。
            let (_, key) = echo_defs::NodeId::split_ref(&req.subject);
            let session_id = key;
            if req.team_id.is_empty() {
                return err("team_id 必填（会话快照按人格限定）");
            }
            let candidates: Vec<_> = personas
                .personas()
                .into_iter()
                .filter(|p| p.id == req.team_id)
                .collect();
            if candidates.is_empty() {
                return err(&format!("人格 {} 不存在", req.team_id));
            }
            for persona in candidates {
                let agent = &persona.agent;
                if let Some(session) = agent.trunk.get(session_id) {
                    let (messages, seq) = if req.since_seq > 0 {
                        match agent.trunk.timeline_snapshot_since(req.since_seq) {
                            Some(snap) => snap,
                            None => (agent.trunk.timeline_snapshot(), agent.trunk.timeline_seq()),
                        }
                    } else {
                        (agent.trunk.timeline_snapshot(), agent.trunk.timeline_seq())
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

/// 自更新状态文件路径（与 install.sh/update.sh 布局一致）。
pub(crate) fn self_update_status_path() -> std::path::PathBuf {
    // 状态路径不依赖联邦——从 XDG/默认布局解析
    let state_home = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state")
        });
    state_home.join("echo-agent-core/update-status")
}

/// 读自更新状态文件 → `SelfUpdateStatus` 事件（无文件 = idle）。
pub(crate) async fn read_self_update_status() -> echo_protocol::BackendEvent {
    let path = self_update_status_path();
    let mut fields = std::collections::HashMap::new();
    if let Ok(text) = tokio::fs::read_to_string(&path).await {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                fields.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    echo_protocol::BackendEvent::SelfUpdateStatus {
        state: fields
            .get("state")
            .cloned()
            .unwrap_or_else(|| "idle".into()),
        phase: fields.get("phase").cloned(),
        revision: fields.get("revision").cloned(),
        message: fields.get("message").cloned(),
        updated_at: fields.get("updated_at").cloned(),
    }
}

/// 触发自更新：启动 `echo-agent-core-update.service`（oneshot，脱离本
/// 进程执行 构建→替换→重启——agent 自杀不了它）。Graceful Drain 由
/// core 的 SIGTERM 处理保证（当前回复完整生成后再退）。
pub(crate) async fn handle_self_update(
    emit: std::sync::Arc<dyn Fn(echo_protocol::BackendEvent) + Send + Sync>,
) {
    // 防重入：已有 update service 在跑就不重复启动。
    let check = tokio::process::Command::new("systemctl")
        .args([
            "--user",
            "is-active",
            "--quiet",
            "echo-agent-core-update.service",
        ])
        .status()
        .await;
    if matches!(check, Ok(s) if s.success()) {
        emit(echo_protocol::BackendEvent::Error {
            session_id: None,
            message: "自更新已在进行中（echo-agent-core-update.service active）".into(),
        });
        return;
    }
    let result = tokio::process::Command::new("systemctl")
        .args(["--user", "start", "echo-agent-core-update.service"])
        .status()
        .await;
    match result {
        Ok(s) if s.success() => {
            // 受理回执：状态文件很快会被 updater 写 running——前端
            // 轮询 SelfUpdateStatus 观察进度；看到 restarting 即停止
            // 轮询（服务即将重启，后续验证属新 turn）。
            emit(read_self_update_status().await);
        }
        Ok(s) => {
            emit(echo_protocol::BackendEvent::Error {
                session_id: None,
                message: format!(
                    "自更新启动失败（systemctl exit {s}）——请检查 echo-agent-core-update.service"
                ),
            });
        }
        Err(e) => {
            emit(echo_protocol::BackendEvent::Error {
                session_id: None,
                message: format!("自更新启动失败: {e}"),
            });
        }
    }
}

/// 配对成功的邀请占位**提升为配置条目**（2026-10）：
///
/// 此前占位（`invite-*`，运行时-only）配对即被删除——但删除后本机对已
/// 配对链路既无配置（Panel 不可见/不可编辑、无策略 → 连入方向静默全拒），
/// 对端重启后还因 token 消失（认证要求 token 匹配）**无法重连**。
///
/// 现在改为三步（幂等；仅对"token 已有活跃链路的占位"生效）：
/// 1. 写配置 `[federation.peers.<name>]`：url 空（仅接受连入）、保留
///    token、**默认满权限**（`ExecutorPolicy::full()`）——邀请串即凭证，
///    接收方按互信对待（与「SSH 免密」同语义）；之后可在 Panel 收紧。
/// 2. 运行时登记：`add_peer`（接受重连）+ 策略表写入满权限 +
///    刷新该 node 的路由策略（本函数返回后由调用方复核）。
/// 3. 映射更新（peer_names / node_to_peer）为新配置名 + 移除占位。
///
/// 名字选取：对端 Hello 的 `node_name` 优先（人类可读，如 `gpu-box`），
/// 冲突或缺失时退 `node-<node_id 前 8 位>`，仍冲突追加序号。
/// 学习对端 Hello 宣告的**可拨地址**（2026-10，配对无方向性）：
///
/// 邀请配对之初只有一根方向（接入方 → 邀请方）被系统配置；链路建立后，
/// 对端 Hello 携带其 `[federation] listen/advertise` 推导的对外地址。本机
/// 据此把自己这边的 peer 条目补全（运行时 + 落配置）——此后**双方都会
/// 主动重连**（谁先恢复链路都行），配对不再有方向语义。仅填空，不覆盖
/// 已配置/已学习的地址；对端呼叫本机地址（同机双节点等）时跳过。
pub(crate) async fn learn_peer_dial_address(
    rt: &FederationRuntime,
    info: &echo_federation::PeerInfo,
) {
    // 解析后的配置名（邀请占位已提升为最终名）。
    let name = rt
        .node_to_peer
        .read()
        .await
        .get(&info.node_id)
        .cloned()
        .unwrap_or_else(|| info.peer_name.clone());
    let configs = rt.federation.peer_configs().await;
    let Some(cfg) = configs.into_iter().find(|p| p.name == name) else {
        return;
    };
    let Some(url) = learnable_peer_url(&cfg.url, info.advertise.as_deref()) else {
        return;
    };
    // 自指防护：对端宣告的竟是本机地址 → 学习只会导致自拨回环。
    let own = invite_host(&rt.listen, &rt.advertise).map(|host_port| format!("ws://{host_port}"));
    if own.as_deref() == Some(url.as_str()) {
        return;
    }
    let write_name = name.clone();
    let write_url = url.clone();
    if let Err(e) = rt.config_store.patch(move |root| {
        let federation = echo_adapter::ensure_table(root, "federation");
        let peers = echo_adapter::ensure_table(federation, "peers");
        let entry = echo_adapter::ensure_table(peers, &write_name);
        entry.insert("url".into(), toml::Value::String(write_url));
        Ok(())
    }) {
        tracing::warn!(target: "federation", %name, error = %e, "learn peer url: config write failed");
    }
    rt.federation
        .add_peer(echo_federation::PeerConfig {
            name: name.clone(),
            url: url.clone(),
            token: cfg.token.clone(),
        })
        .await;
    tracing::info!(
        target: "federation",
        peer = %name, url = %url,
        "learned peer dial address (bidirectional reconnect)"
    );
}

/// 学习决策（纯函数）：仅当本机尚无该 peer 地址、且对端宣告了非空地址。
pub(crate) fn learnable_peer_url(existing_url: &str, advertised: Option<&str>) -> Option<String> {
    if !existing_url.trim().is_empty() {
        return None;
    }
    advertised
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
}

pub(crate) async fn promote_paired_invite_placeholders(
    rt: &FederationRuntime,
    policies: &mut std::collections::HashMap<String, echo_agent::federation::ExecutorPolicy>,
) {
    let configs = rt.federation.peer_configs().await;
    // 占位判定走**登记表**（2026-10 审计 P1-1）：此前按「名字前缀 invite-」
    // 猜测——用户配置名含该前缀的 peer 一旦 Up 即被当占位提升（改名、
    // 写配置、权限强制覆盖为 full 且原配置段残留）。登记表为显式记账。
    let placeholders: std::collections::HashSet<String> = {
        let reg = rt
            .invite_placeholders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reg.keys().cloned().collect()
    };
    if placeholders.is_empty() {
        return;
    }
    let mut paired: Vec<(String, String)> = Vec::new(); // (placeholder 名, token)
    for p in &configs {
        if !placeholders.contains(&p.name) {
            continue;
        }
        if rt.federation.is_link_token_online(&p.token).await {
            paired.push((p.name.clone(), p.token.clone()));
        }
    }
    if paired.is_empty() {
        return;
    }
    let existing: Vec<String> = configs.iter().map(|p| p.name.clone()).collect();
    for (placeholder, token) in paired {
        // 每个占位从**自己的链路信息**取对端 node_id/node_name（接受侧
        // peer_name 已由 link.rs 按 token 解析为占位名，peer_info 可查）——
        // 多占位并发配对时不能都用"当前链路"的身份。
        let Some(info) = rt.federation.peer_info(&placeholder).await else {
            continue;
        };
        let node_id = info.node_id.as_str();
        let node_name = info.node_name.as_deref();
        let promoted = pick_promoted_peer_name(&existing, node_name, node_id);
        // 1. 落配置（满权限默认）。
        let write_name = promoted.clone();
        let write_token = token.clone();
        if let Err(e) = rt.config_store.patch(move |root| {
            let federation = echo_adapter::ensure_table(root, "federation");
            let peers = echo_adapter::ensure_table(federation, "peers");
            let entry = echo_adapter::ensure_table(peers, &write_name);
            entry.insert("url".into(), toml::Value::String(String::new()));
            entry.insert("token".into(), toml::Value::String(write_token));
            entry.insert(
                "allow_tools".into(),
                toml::Value::Array(vec![toml::Value::String("*".into())]),
            );
            entry.insert(
                "allow_queries".into(),
                toml::Value::Array(vec![toml::Value::String("*".into())]),
            );
            entry.insert("allow_subagent".into(), toml::Value::Boolean(true));
            entry.insert("require_confirm".into(), toml::Value::Array(Vec::new()));
            Ok(())
        }) {
            tracing::warn!(target: "federation", %placeholder, error = %e, "promote placeholder: config write failed");
        }
        // 2. 运行时：登记 peer（url 空 = 仅接受连入）+ 满权限策略。
        rt.federation
            .add_peer(echo_federation::PeerConfig {
                name: promoted.clone(),
                url: String::new(),
                token: token.clone(),
            })
            .await;
        policies.insert(
            promoted.clone(),
            echo_agent::federation::ExecutorPolicy::full(),
        );
        rt.router
            .set_policy(node_id, echo_agent::federation::ExecutorPolicy::full());
        // 3. 映射替换 + 移除占位。
        rt.peer_names.write().await.remove(&placeholder);
        rt.peer_names
            .write()
            .await
            .insert(promoted.clone(), node_id.to_string());
        rt.node_to_peer
            .write()
            .await
            .insert(node_id.to_string(), promoted.clone());
        // 最近链路信息同步重键（invite-* → 配置名）：dial 停车判定与
        // 远程反查继续按新名命中（否则学习地址后的连出循环会无视当前
        // 活跃链路、立刻对拆）。
        rt.federation
            .rename_peer_info(&placeholder, &promoted)
            .await;
        rt.federation.remove_peer(&placeholder).await;
        // 登记表移除（已提升为正式配置条目）。
        {
            let mut reg = rt
                .invite_placeholders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reg.remove(&placeholder);
        }
        tracing::info!(
            target: "federation",
            %placeholder, promoted = %promoted, node = %node_id,
            "invite placeholder promoted to full-permission config entry"
        );
    }
}

/// 反查某 peer 配置名对应的**在线**对端 node_id（联邦状态快照的
/// `node_id` 字段来源，2026-10 组建向导）。
///
/// 数据源：`node_to_peer`（链路 Up 时登记 node_id → 配置名、Down 移除）
/// + 活跃链路 node_id 集合——双条件防陈旧映射（表在、链路已死不选）。
pub(crate) fn online_node_for_peer(
    node_to_peer: &std::collections::HashMap<String, String>,
    active: &std::collections::HashSet<String>,
    peer_name: &str,
) -> Option<String> {
    node_to_peer
        .iter()
        .find(|(node, name)| name.as_str() == peer_name && active.contains(node.as_str()))
        .map(|(node, _)| node.clone())
}

/// 提升后的配置名选取（纯函数，便于单测）：
/// node_name 优先（非空且未占用）；否则 `node-<id 前 8>`；仍冲突追加 `-2`…
pub(crate) fn pick_promoted_peer_name(
    existing: &[String],
    node_name: Option<&str>,
    node_id: &str,
) -> String {
    let base = match node_name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => name.to_string(),
        None => {
            // node_id 形如 `node-<ulid>`：**剥前缀**再取前 8 位——直接截取
            // 会产出 `node-node-…` 双前缀（2026-10 修复；原测试用无前缀
            // 输入掩盖了这一点）。
            let short: String = node_id
                .strip_prefix("node-")
                .unwrap_or(node_id)
                .chars()
                .take(8)
                .collect();
            format!("node-{short}")
        }
    };
    if !existing.iter().any(|n| n == &base) {
        return base;
    }
    for i in 2..100 {
        let candidate = format!("{base}-{i}");
        if !existing.iter().any(|n| n == &candidate) {
            return candidate;
        }
    }
    base
}

/// 邀请用的对外地址（优先显式 `advertise` 配置）：
///
/// - `listen` 为空（纯连出）→ None（无监听，邀请无意义）
/// - `advertise` 非空 → 含端口（末段可解析为端口）直接用；否则补 listen
///   端口。多网卡 / VPN 场景下指定"对端可达的那个地址"（如 tun0 地址）。
/// - `advertise` 为空 → 沿用推导（listen 非 0.0.0.0 直接用；否则取本机
///   首个非回环 IPv4——见 [`advertise_host`]）。
pub(crate) fn invite_host(listen: &str, advertise: &str) -> Option<String> {
    if listen.trim().is_empty() {
        return None;
    }
    let adv = advertise.trim();
    if !adv.is_empty() {
        let has_port = adv
            .rsplit_once(':')
            .map(|(_, p)| p.parse::<u16>().is_ok())
            .unwrap_or(false);
        if has_port {
            return Some(adv.to_string());
        }
        let port = listen
            .rsplit_once(':')
            .map(|(_, p)| p)
            .filter(|p| !p.is_empty())
            .unwrap_or("3133");
        return Some(format!("{adv}:{port}"));
    }
    advertise_host(listen)
}

/// 邀请用的对外地址：listen 为具体 IP/主机名时直接用；`0.0.0.0` / `[::]`
/// 时退化为"首个非回环 IPv4"（无则 None，提示手工配置 advertise）。
pub(crate) fn advertise_host(listen: &str) -> Option<String> {
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
pub(crate) async fn register_remote_tools(
    rt: &FederationRuntime,
    personas: &agent_supervisor::AgentSupervisor,
    info: &echo_federation::PeerInfo,
    peer_name: &str,
) {
    let candidates = echo_agent::federation::remote_tool_candidates(&info.caps);
    if candidates.is_empty() {
        return;
    }
    // `peer_name` 为解析后的配置名（占位提升后 = 新名）；工具注册名与包
    // 标签都以它为准，保证与 Down 的 peer_package（node_to_peer 反查）一致。
    let package = format!("echo-agent.federation.{peer_name}");
    // 先清本 peer 旧句柄（重注册/重连场景）：**先删后插**——Disposer
    // 按名移除，若先插后删，旧句柄会把同名的**新**注册条目一并删掉
    // （2026-10 修复；旧实现为全局 clear 后 extend，多 peer 下互杀）。
    {
        let mut slot = rt
            .remote_tool_keepers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.remove(&info.node_id);
    }
    let mut registered = 0usize;
    let mut keepers: Vec<echo_context::Disposer> = Vec::new();
    for persona in personas.personas() {
        let definitions = persona.agent.tools.definitions().await;
        for remote_name in &candidates {
            let (description, parameters) =
                echo_agent::federation::proxy_schema(&definitions, remote_name, peer_name);
            let tool = std::sync::Arc::new(echo_agent::federation::RemoteTool::new(
                peer_name,
                &info.node_id,
                remote_name,
                description,
                parameters,
                rt.federation.clone(),
                rt.router.clone(),
            ));
            let disposer = persona.agent.tools.register_reversible(tool);
            keepers.push(disposer); // 保活：drop 即注销，绝不能当临时值
            persona.agent.tools.set_package(
                &echo_agent::federation::remote_tool_name(peer_name, remote_name),
                &package,
            );
        }
        registered = candidates.len();
        // 重连语义：Down 时整包禁用，Up 必须整包重新启用（首次注册时
        // 包不在禁用集，本调用无害幂等）。
        persona.agent.tools.set_package_enabled(&package, true);
    }
    // 保活句柄入库（本 peer 分桶；旧桶已在上方清空）。
    {
        let mut slot = rt
            .remote_tool_keepers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.insert(info.node_id.clone(), keepers);
    }
    tracing::info!(
        target: "federation",
        peer = %info.node_id, name = %peer_name, package = %package, tools = registered,
        "remote proxy tools registered"
    );
}

#[cfg(test)]
mod core_util_tests {
    use super::online_node_for_peer;
    use super::validate_federation_peer_url;
    use crate::agent_supervisor::AgentSupervisor;
    use crate::federation_import::{chunk_utf8, handle_session_import, ImportOutcome};
    use crate::pick_promoted_peer_name;
    use crate::{advertise_host, invite_host, learnable_peer_url};
    use echo_agent::{AgentConfig, TeamMember};

    /// 对端地址学习决策（配对无方向性）：仅填空，不覆盖；空宣告不学习。
    #[test]
    pub(crate) fn learnable_peer_url_fills_only_empty_slots() {
        // 空 url + 对端宣告 → 学习。
        assert_eq!(
            learnable_peer_url("", Some("ws://10.0.0.9:3133")),
            Some("ws://10.0.0.9:3133".to_string())
        );
        // 已配置 url → 不覆盖（操作员配置 / 历史学习优先）。
        assert_eq!(
            learnable_peer_url("ws://old:3133", Some("ws://new:3133")),
            None
        );
        // 对端未宣告 / 空白宣告 → 不学习。
        assert_eq!(learnable_peer_url("", None), None);
        assert_eq!(learnable_peer_url("", Some("  ")), None);
        // 首尾空白清理。
        assert_eq!(
            learnable_peer_url("", Some(" ws://a:1 ")),
            Some("ws://a:1".to_string())
        );
    }

    /// 在线对端 node_id 反查（联邦状态快照 `node_id` 字段来源）：
    /// 双条件（名字匹配 + node 在活跃集合），防陈旧映射；空表/不匹配 None。
    #[test]
    pub(crate) fn online_node_for_peer_matches_name_and_active() {
        use std::collections::{HashMap, HashSet};
        let map: HashMap<String, String> = [
            ("node-abc".to_string(), "gpu-box".to_string()),
            ("node-def".to_string(), "invite-deadbeef".to_string()),
        ]
        .into_iter()
        .collect();
        let active: HashSet<String> = ["node-abc".to_string(), "node-def".to_string()]
            .into_iter()
            .collect();

        // 名字匹配 + 活跃 → Some(node_id)。
        assert_eq!(
            online_node_for_peer(&map, &active, "gpu-box").as_deref(),
            Some("node-abc")
        );
        // 名字不存在 → None。
        assert_eq!(online_node_for_peer(&map, &active, "other"), None);
        // 映射在但链路已死（不在活跃集合）→ None（防陈旧）。
        let empty: HashSet<String> = HashSet::new();
        assert_eq!(online_node_for_peer(&map, &empty, "gpu-box"), None);
        // 空表 → None。
        let none: HashMap<String, String> = HashMap::new();
        assert_eq!(online_node_for_peer(&none, &active, "gpu-box"), None);
    }
    /// 邀请对外地址（2026-10）：显式 `[federation] advertise` 优先——多网卡/
    /// VPN 叠加场景指定"对端可达地址"（如 tun0），不再赌 hostname -I 顺序。
    /// peer URL 形状校验（SaveFederationPeer 前置；2026-10 审计）：
    /// 空 = 仅接受连入（合法）；ws/wss + host:port 合法；其余拒绝。
    #[test]
    pub(crate) fn federation_peer_url_validation() {
        // 合法：空（被动）、ws、wss、路径尾缀、IPv6。
        assert!(validate_federation_peer_url("").is_ok());
        assert!(validate_federation_peer_url("  ").is_ok());
        assert!(validate_federation_peer_url("ws://10.10.2.250:3133").is_ok());
        assert!(validate_federation_peer_url("wss://example.com:443").is_ok());
        assert!(validate_federation_peer_url("ws://example.com:3133/ws").is_ok());
        assert!(validate_federation_peer_url("ws://[::1]:3133").is_ok());

        // 非法：缺 scheme、缺端口、端口非数字、空主机。
        assert!(validate_federation_peer_url("10.10.2.250:3133").is_err());
        assert!(validate_federation_peer_url("http://x:1").is_err());
        assert!(validate_federation_peer_url("ws://10.10.2.250").is_err());
        assert!(validate_federation_peer_url("ws://host:notaport").is_err());
        assert!(validate_federation_peer_url("ws://:3133").is_err());
    }

    #[test]
    pub(crate) fn invite_host_prefers_explicit_advertise() {
        // 显式 host:port：原样使用
        assert_eq!(
            invite_host("0.0.0.0:3133", "10.10.10.100:3133"),
            Some("10.10.10.100:3133".to_string())
        );
        // 仅 host：补 listen 端口（含自定义端口与首尾空白）
        assert_eq!(
            invite_host("0.0.0.0:4000", "10.10.10.100"),
            Some("10.10.10.100:4000".to_string())
        );
        assert_eq!(
            invite_host("0.0.0.0:3133", " vpn.example.net "),
            Some("vpn.example.net:3133".to_string())
        );
        // IPv6 全写（[addr]:port）：直接使用
        assert_eq!(
            invite_host("0.0.0.0:3133", "[fd00::1]:3133"),
            Some("[fd00::1]:3133".to_string())
        );
    }

    /// advertise 为空：沿用旧推导；listen 为空（纯连出）：邀请无意义 → None。
    #[test]
    pub(crate) fn invite_host_fallback_and_empty_listen() {
        assert_eq!(
            invite_host("192.168.1.5:3133", ""),
            Some("192.168.1.5:3133".to_string())
        );
        assert_eq!(
            invite_host("192.168.1.5:3134", "   "),
            Some("192.168.1.5:3134".to_string())
        );
        // 纯连出（无监听）：即便配了 advertise 也不生成邀请地址
        assert_eq!(invite_host("", "10.10.10.100"), None);
        assert_eq!(invite_host("   ", "10.10.10.100:3133"), None);
        // listen 为具体地址时 advertise_host 本身直接回显（确定性分支）
        assert_eq!(
            advertise_host("10.0.0.7:3133"),
            Some("10.0.0.7:3133".to_string())
        );
    }

    /// 邀请占位提升的命名规则（2026-10）：node_name 优先、缺失退
    /// node-<id 前 8>、冲突追加序号；node_id 带 `node-` 前缀时剥前缀
    /// 再截（防 `node-node-…` 双前缀，2026-10 修复）。
    #[test]
    pub(crate) fn pick_promoted_peer_name_prefers_node_name_and_dedupes() {
        assert_eq!(
            pick_promoted_peer_name(&[], Some("gpu-box"), "abcdef012345"),
            "gpu-box"
        );
        assert_eq!(
            pick_promoted_peer_name(&[], Some("  "), "abcdef012345"),
            "node-abcdef01"
        );
        assert_eq!(
            pick_promoted_peer_name(&[], None, "abcdef012345"),
            "node-abcdef01"
        );
        // 真实格式 `node-<ulid>`：剥前缀后截取，不得出现双前缀。
        assert_eq!(
            pick_promoted_peer_name(&[], None, "node-01m3yevthc03jf599zj7avx4m8"),
            "node-01m3yevt"
        );
        assert_eq!(
            pick_promoted_peer_name(&[], Some(" "), "node-abcdef012345"),
            "node-abcdef01"
        );
        let existing = vec!["gpu-box".to_string()];
        assert_eq!(
            pick_promoted_peer_name(&existing, Some("gpu-box"), "abcdef012345"),
            "gpu-box-2"
        );
        let existing2 = vec!["gpu-box".to_string(), "gpu-box-2".to_string()];
        assert_eq!(
            pick_promoted_peer_name(&existing2, Some("gpu-box"), "abcdef012345"),
            "gpu-box-3"
        );
    }

    /// P3-2b：分块必须落在 UTF-8 字符边界，拼接后与原文一致。
    #[test]
    pub(crate) fn chunk_utf8_respects_char_boundaries() {
        let text = "你好世界abcdef";
        let chunks = chunk_utf8(text, 4);
        assert_eq!(chunks.concat(), text);
        for chunk in &chunks {
            assert!(chunk.len() <= 4, "chunk over budget: {chunk:?}");
            assert!(text.contains(chunk.as_str()));
        }
    }

    #[test]
    pub(crate) fn chunk_utf8_empty_and_small_inputs() {
        assert_eq!(chunk_utf8("", 8), vec![String::new()]);
        assert_eq!(chunk_utf8("abc", 8), vec!["abc".to_string()]);
    }

    pub(crate) fn disabled_supervisor() -> AgentSupervisor {
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "alix".into(),
            TeamMember {
                enabled: false,
                ..Default::default()
            },
        );
        AgentSupervisor::build(&raw, |_id, _profile| {
            panic!("disabled personas must not be instantiated")
        })
    }

    pub(crate) fn import_frame(
        index: u32,
        total: u32,
        data: &str,
    ) -> echo_federation::SessionImportFrame {
        echo_federation::SessionImportFrame {
            transfer_id: "mig-1".into(),
            session_id: "local:tui::local_user".into(),
            team_id: "alix".into(),
            target_peer: "gpu-box".into(),
            chunk_index: index,
            chunk_total: total,
            data: data.into(),
        }
    }

    /// P3-2b：非终块 ack 推进但不产出终态；终块 ack=total 且目标无该人格 → 失败。
    #[test]
    pub(crate) fn session_import_buffers_chunks_then_reports_missing_persona() {
        let supervisor = disabled_supervisor();
        let mut imports = std::collections::HashMap::new();
        let (acked, outcome) =
            handle_session_import(&supervisor, &import_frame(0, 2, "[]"), &mut imports);
        assert_eq!(acked, 1);
        assert!(matches!(outcome, ImportOutcome::Pending));
        let (acked, outcome) =
            handle_session_import(&supervisor, &import_frame(1, 2, ""), &mut imports);
        assert_eq!(acked, 2);
        match outcome {
            ImportOutcome::Failed(message) => assert!(message.contains("不存在")),
            _ => panic!("expected terminal failure"),
        }
    }

    /// 重复块（重发）幂等：已确认前缀不因重复而倒退。
    #[test]
    pub(crate) fn session_import_duplicate_chunks_are_idempotent() {
        let supervisor = disabled_supervisor();
        let mut imports = std::collections::HashMap::new();
        let (acked, _) =
            handle_session_import(&supervisor, &import_frame(0, 3, "[]"), &mut imports);
        assert_eq!(acked, 1);
        // 重发第 0 块：ack 仍为 1，不倒退。
        let (acked, _) =
            handle_session_import(&supervisor, &import_frame(0, 3, "[]"), &mut imports);
        assert_eq!(acked, 1);
        // 乱序：先到第 2 块不推进前缀（ack 仍 1）。
        let (acked, _) = handle_session_import(&supervisor, &import_frame(2, 3, ""), &mut imports);
        assert_eq!(acked, 1);
    }

    /// 防御：超限单块（>192KB）拒收，不进入缓冲（接收侧尺寸校验）。
    #[test]
    pub(crate) fn session_import_rejects_oversized_chunk() {
        let supervisor = disabled_supervisor();
        let mut imports = std::collections::HashMap::new();
        let oversized = "x".repeat(192 * 1024 + 1);
        let (acked, outcome) =
            handle_session_import(&supervisor, &import_frame(0, 1, &oversized), &mut imports);
        assert_eq!(acked, 0);
        match outcome {
            ImportOutcome::Failed(message) => assert!(message.contains("超限"), "消息: {message}"),
            _ => panic!("expected oversized chunk rejection"),
        }
        assert!(imports.is_empty(), "超限块不应进入缓冲");
    }

    /// 防御：非法分块头（total=0）立即回失败，不污染缓冲。
    #[test]
    pub(crate) fn session_import_rejects_bad_header() {
        let supervisor = disabled_supervisor();
        let mut imports = std::collections::HashMap::new();
        let (acked, outcome) =
            handle_session_import(&supervisor, &import_frame(0, 0, "[]"), &mut imports);
        assert_eq!(acked, 0);
        assert!(matches!(outcome, ImportOutcome::Failed(_)));
        assert!(imports.is_empty());
    }
}
