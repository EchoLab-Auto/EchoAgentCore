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
    pub(crate) config_store: echo_config::ConfigStore,
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
    /// **两层**（2026-11 S13）：`node_id → persona id → 该人格的 Disposer`。
    ///
    /// 为什么按人格分层：注册是**按人格**发生的（每人格有自己的工具注册表），
    /// 而"新人格在链路已 Up 之后才建"要求能够**单独**给一个 agent 注册而不
    /// 重复给别人注册（此前只有 node_id 一层，只能整 node 重注册）。
    pub(crate) remote_tool_keepers: std::sync::Mutex<
        std::collections::HashMap<
            String,
            std::collections::HashMap<String, Vec<echo_context::Disposer>>,
        >,
    >,
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
    /// 进行中的会话迁移 ack 通道：`transfer_id → 已确认块数的 watch sender`。
    /// 源侧 `handle_migrate_session` 注册，泵收到 `SessionImportAck` 时推进。
    ///
    /// 2026-10 技术债清理：此前存在 `echo_bootstrap` 惰性槽（`Box::leak` 出
    /// `&'static Mutex`）。它确实是**宿主运行期状态**，但塞进进程级表只是
    /// "借用现成设施"——挪到本运行时后，两套联邦运行时也互不干扰。
    pub(crate) session_import_acks:
        std::sync::Mutex<std::collections::HashMap<String, tokio::sync::watch::Sender<u32>>>,
    /// 本节点当前活跃的人格：`id → Weak<Agent>`（2026-11 S14）。
    ///
    /// ## 为什么不用 `AgentSupervisor`
    ///
    /// 此前联邦侧要"全部人格"时一律调 `personas.personas()`——那是
    /// `AgentSupervisor` 的句柄，于是 `federation_rt` 与 `agent_supervisor`
    /// 绑死（泵因此搬不进插件），每次调用还要 clone 一整份 `Vec<Persona>`
    /// （链路抖动时每条事件两次）。
    ///
    /// 现在改为**人格自己登记**：构造时调 [`Self::register_persona`]，
    /// 存的是 `Weak`——人格被删除/禁用后自动从列表里消失（`upgrade()` 失败），
    /// 不需要任何"清扫已删人格"的代码路径。
    ///
    /// 语义与 `supervisor.personas()` 等价：都是"当前活着的、启用的人格"。
    pub(crate) personas: tokio::sync::RwLock<Vec<(String, std::sync::Weak<echo_agent::Agent>)>>,
    /// 进程级事件兜底出口（`core_agent`）：**没有**任何人格时联邦状态事件
    /// 仍要送到 Panel（否则链路变化静默丢失）。弱引用，理由同上。
    pub(crate) event_fallback: std::sync::RwLock<Option<std::sync::Weak<echo_agent::Agent>>>,
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
    /// 登记一个人格（构造期调；同 id 覆盖）。
    ///
    /// 幂等：人格重建（禁用再启用 / 保存后重跑 factory）会用新实例覆盖旧条目，
    /// 于是 `live_personas()` 永远只看到**当前**那一个。
    pub(crate) async fn register_persona(&self, id: &str, agent: &std::sync::Arc<echo_agent::Agent>) {
        let mut list = self.personas.write().await;
        let weak = std::sync::Arc::downgrade(agent);
        if let Some(entry) = list.iter_mut().find(|(existing, _)| existing == id) {
            entry.1 = weak;
        } else {
            list.push((id.to_string(), weak));
            list.sort_by(|a, b| a.0.cmp(&b.0));
        }
    }

    /// 当前**活着**的人格（`Weak` 升级失败 = 已被删除/替换）。
    ///
    /// 顺带把死条目摘掉（这就是"清扫"的全部，不需要定时器）。
    pub(crate) async fn live_personas(&self) -> Vec<(String, std::sync::Arc<echo_agent::Agent>)> {
        let mut list = self.personas.write().await;
        let mut live = Vec::with_capacity(list.len());
        list.retain(|(id, weak)| match weak.upgrade() {
            Some(agent) => {
                live.push((id.clone(), agent));
                true
            }
            None => false,
        });
        live
    }

    /// 按 id 取一个人格（`None` = 不存在 / 已死）。
    pub(crate) async fn persona_by_id(
        &self,
        id: &str,
    ) -> Option<std::sync::Arc<echo_agent::Agent>> {
        let list = self.personas.read().await;
        list.iter()
            .find(|(existing, _)| existing == id)
            .and_then(|(_, weak)| weak.upgrade())
    }

    /// 设事件兜底出口（`core_agent`；组合根在它构造后调一次）。
    pub(crate) fn set_event_fallback(&self, agent: &std::sync::Arc<echo_agent::Agent>) {
        if let Ok(mut slot) = self.event_fallback.write() {
            *slot = Some(std::sync::Arc::downgrade(agent));
        }
    }

    /// 发一条联邦事件：**任一人格**都通到 Panel（进程级汇聚点），所以取第一个
    /// 活着的；无人格（全部被禁用）时回退到 `core_agent`——否则链路变化静默丢失。
    ///
    /// 取第一个而不是"找特定人格"：事件是进程级的，谁发都一样。
    pub(crate) async fn emit_federation_event(&self, event: echo_protocol::BackendEvent) {
        if let Some((_, agent)) = self.live_personas().await.into_iter().next() {
            agent.emit(event);
            return;
        }
        let fallback = self
            .event_fallback
            .read()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(std::sync::Weak::upgrade));
        match fallback {
            Some(agent) => agent.emit(event),
            None => tracing::warn!(
                target: "federation",
                event = event.kind_name(),
                "no persona and no fallback emitter; federation event dropped"
            ),
        }
    }

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
    peer_policies: std::sync::Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<String, echo_agent::federation::ExecutorPolicy>,
        >,
    >,
    // 联邦出口集合：泵侧销账要用与 Agent 侧**同一个**聚合器（P3 终态：
    // 参数下发，不再摸 kernel cell）。
    ports: Arc<echo_agent::federation::FederationPorts>,
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
                        register_remote_tools(&rt, &info, &resolved_name).await;
                        // 学习对端宣告的拨号地址（配对无方向性，2026-10）：
                        // 对端 Hello 携带其对外地址 → 本机若无该 peer 地址
                        // 则补上（运行时 + 落配置）→ 双方都会主动重连。
                        learn_peer_dial_address(&rt, &info).await;
                        broadcast_federation_status(&rt).await;
                    }
                    echo_federation::LinkEvent::Down { peer_node_id, reason } => {
                        tracing::info!(target: "federation", peer = %peer_node_id, %reason, "link down");
                        rt.router.drop_peer(&peer_node_id);
                        let package = peer_package(&rt, &peer_node_id).await;
                        for (_id, agent) in rt.live_personas().await {
                            agent.tools.set_package_enabled(&package, false);
                        }
                        if let Some(name) = rt.node_to_peer.write().await.remove(&peer_node_id) {
                            rt.peer_names.write().await.remove(&name);
                        }
                        // 链路状态变化 → 面板联邦页实时刷新。
                        broadcast_federation_status(&rt).await;
                    }
                    echo_federation::LinkEvent::Frame { from, frame } => {
                        if let echo_federation::FedFrame::Invoke(request) = frame {
                            let registry = rt
                                .live_personas()
                                .await
                                .into_iter()
                                .map(|(_id, agent)| agent.tools.clone())
                                .next();
                            let Some(registry) = registry else {
                                // 无 persona = 无工具注册表。此前的 `continue`
                                // 让调用方一直等到自身超时（实测 460s 空等，
                                // 且本端只看到「远程工具超时」，排查成本极高）。
                                // 改为显式 Error 回执：调用方立即失败并看到原因。
                                tracing::warn!(
                                    target: "federation", from = %from, call_id = %request.call_id,
                                    "invoke rejected: no persona registry on this node"
                                );
                                let reply = echo_federation::FedFrame::Error {
                                    call_id: Some(request.call_id.clone()),
                                    code: echo_federation::FedError::Forbidden,
                                    message: "执行端未加载任何 persona，联邦工具通道不可用".into(),
                                };
                                if rt.federation.send_to(&from, reply).await.is_err() {
                                    tracing::warn!(target: "federation", peer = %from, "invoke reject reply send failed");
                                }
                                continue;
                            };
                            // 沙箱根并集：全部 persona 工作区目录（本地
                            // 目录取 path；远程目录标注占位不参战）。
                            let mut roots: Vec<std::path::PathBuf> = Vec::new();
                            for (_id, agent) in rt.live_personas().await {
                                if let Some(store) = agent.workspace_store() {
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
                            let reply = handle_federation_query(&rt, &from, req).await;
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
                                    &ports.aggregator,
                                    &ev.call_id,
                                    ev.status,
                                    ev.result.clone(),
                                )
                            {
                                federation_import::deliver_aggregate_summary(&rt, summary).await;
                            }
                        } else if let echo_federation::FedFrame::SessionImport(import) = &frame {
                            // 目标侧（P3-2b）：聚合分块 + 每块 ack（流控/重发）；
                            // 凑齐后 spawn_blocking 导入，终态经 Result 回源。
                            let (acked, outcome) =
                                federation_import::handle_session_import(&rt, import, &mut imports);
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
                            federation_import::note_session_import_ack(
                                &rt,
                                &ack.transfer_id,
                                ack.acked_upto,
                            );
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
                            rt.emit_federation_event(event).await;
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
pub(crate) async fn broadcast_federation_status(rt: &FederationRuntime) {
    // 进程级事件汇聚点：任一人格都到 Panel。无人格（全部被禁用）时回退到
    // 进程级核心服务代理 `__core`——否则联邦状态变化静默丢失。
    let event = rt.status_event().await;
    rt.emit_federation_event(event).await;
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
            let agents = rt.live_personas().await;
            let active_turns: usize = agents
                .iter()
                .map(|(_id, agent)| agent.active_turn_session_ids().len())
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
            for (_id, agent) in rt.live_personas().await {
                if let Some(store) = agent.workspace_store() {
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
            for (_id, agent) in rt.live_personas().await {
                if let Some(store) = agent.workspace_store() {
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
            // 全局配置在 `__core` 上（它承接管理面命令）；无人格时回退兜底出口。
            let core_agent = match rt.persona_by_id("__core").await {
                Some(agent) => Some(agent),
                None => rt.live_personas().await.into_iter().next().map(|(_, a)| a),
            };
            let Some(core_agent) = core_agent else {
                return err("无可用 agent 读取供应商配置");
            };
            let cfg = core_agent.api_config().await;
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
            //
            // `"/"` 也按"根列表"处理（2026-11）：大脑侧的自然写法是
            // `node://<peer>/`，剥掉前缀后剩 `"/"`——此前它被当成"要浏览
            // 对端根目录"，于是**每次点开对端都回「路径不在浏览根内」**。
            // 语义上"对端的 /" 就等于"列出对端能做根的目录"，所以归一化到空。
            let requested = match req.subject.trim() {
                "/" => String::new(),
                other => other.to_string(),
            };
            // 浏览根 = 全部 persona 工作区目录并集 ∪ HOME（与大脑侧本机
            // 浏览同一逻辑——browse_roots helper）。
            let mut all_dirs: Vec<echo_protocol::WorkspaceDirectory> = Vec::new();
            let mut labels: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for (_id, agent) in rt.live_personas().await {
                if let Some(store) = agent.workspace_store() {
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
            let candidates: Vec<_> = rt
                .live_personas()
                .await
                .into_iter()
                .filter(|(id, _)| id == &req.team_id)
                .collect();
            if candidates.is_empty() {
                return err(&format!("人格 {} 不存在", req.team_id));
            }
            for (persona_id, agent) in candidates {
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
                        "team_id": persona_id,
                        "seq": seq,
                        "messages": messages,
                    }));
                }
            }
            err(&format!("会话 {session_id} 不存在"))
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
    let own = echo_plugin_federation::admin::invite_host(&rt.listen, &rt.advertise)
        .map(|host_port| format!("ws://{host_port}"));
    if own.as_deref() == Some(url.as_str()) {
        return;
    }
    let write_name = name.clone();
    let write_url = url.clone();
    if let Err(e) = rt.config_store.patch(move |root| {
        let federation = echo_config::ensure_table(root, "federation");
        let peers = echo_config::ensure_table(federation, "peers");
        let entry = echo_config::ensure_table(peers, &write_name);
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
            let federation = echo_config::ensure_table(root, "federation");
            let peers = echo_config::ensure_table(federation, "peers");
            let entry = echo_config::ensure_table(peers, &write_name);
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



/// 代理工具的包标签（Down 时按它整包禁用）。
///
/// `peer_name` 是**解析后的配置名**（占位提升后 = 新名）；工具注册名与包
/// 标签都以它为准，保证与 Down 的 `peer_package`（node_to_peer 反查）一致。
fn peer_tool_package(peer_name: &str) -> String {
    format!("echo-agent.federation.{peer_name}")
}

/// 给**一个** agent 注册某对端的全部代理工具（本机 schema 复制）。
///
/// 返回该 agent 的保活句柄（drop 即按名注销）。
///
/// ## 为什么是"一个 agent"而不是"全部 persona"（2026-11 S13）
///
/// 原来的形状只能整进程一次性做：`register_remote_tools(rt, personas, …)`
/// 遍历 supervisor。于是有一个**静默缺口**——链路 Up 之后才建的人格拿不到
/// 任何远程工具（`grep register_remote_tools` 只有一个调用点：Link Up），
/// 面板上看得见这个人格、模型却看不见 `peer__read_file`，且**没有任何报错**；
/// 要等到下次断线重连才补上。
///
/// 拆成"单人格"之后，两条路径复用同一段逻辑：
/// - 链路 Up → 遍历全部人格各调一次；
/// - 人格新建 → 只给自己调一次（`adopt_active_links`）。
pub(crate) async fn register_remote_tools_for_agent(
    rt: &FederationRuntime,
    agent: &std::sync::Arc<echo_agent::Agent>,
    info: &echo_federation::PeerInfo,
    peer_name: &str,
) -> Vec<echo_context::Disposer> {
    let candidates = echo_agent::federation::remote_tool_candidates(&info.caps);
    if candidates.is_empty() {
        return Vec::new();
    }
    let package = peer_tool_package(peer_name);
    let definitions = agent.tools.definitions().await;
    let mut keepers = Vec::with_capacity(candidates.len());
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
        // 保活：drop 即注销，绝不能当临时值。
        keepers.push(agent.tools.register_reversible(tool));
        agent.tools.set_package(
            &echo_agent::federation::remote_tool_name(peer_name, remote_name),
            &package,
        );
    }
    // 重连语义：Down 时整包禁用，Up 必须整包重新启用（首次注册时包不在
    // 禁用集，本调用无害幂等）。
    agent.tools.set_package_enabled(&package, true);
    keepers
}

/// 按对端 caps 给**全部** persona 注册代理工具（链路 Up 时调）。
pub(crate) async fn register_remote_tools(
    rt: &FederationRuntime,
    info: &echo_federation::PeerInfo,
    peer_name: &str,
) {
    let candidates = echo_agent::federation::remote_tool_candidates(&info.caps);
    if candidates.is_empty() {
        return;
    }
    let package = peer_tool_package(peer_name);
    prune_deleted_personas(rt);
    // 先清本 peer 旧桶（重注册/重连场景）：**先删后插**——Disposer 按名移除，
    // 若先插后删，旧句柄会把同名的**新**注册条目一并删掉（2026-10 修复；
    // 旧实现为全局 clear 后 extend，多 peer 下互杀）。
    {
        let mut slot = rt
            .remote_tool_keepers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.remove(&info.node_id);
    }
    let mut per_persona: std::collections::HashMap<String, Vec<echo_context::Disposer>> =
        std::collections::HashMap::new();
    for (id, agent) in rt.live_personas().await {
        let keepers = register_remote_tools_for_agent(rt, &agent, info, peer_name).await;
        if !keepers.is_empty() {
            per_persona.insert(id, keepers);
        }
    }
    let registered = candidates.len();
    // 保活句柄入库（本 peer 分桶；旧桶已在上方清空）。
    {
        let mut slot = rt
            .remote_tool_keepers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.insert(info.node_id.clone(), per_persona);
    }
    tracing::info!(
        target: "federation",
        peer = %info.node_id, name = %peer_name, package = %package, tools = registered,
        "remote proxy tools registered"
    );
}

/// 清掉**已经被删掉的人格**留下的保活句柄（2026-11 S13/S14）。
///
/// 为什么需要：每个 `Disposer` 内部持有 `Arc<ToolRegistry>`（RAII 注销要靠它）。
/// 人格被删除后，它的注册表本该随之消失——但 keepers 里的句柄仍在，于是
/// **整个注册表被一条不会再被读的记录**吊着。反复"新建 → 删除"会线性累积。
///
/// 判据来自 [`FederationRuntime::live_personas`]（`Weak` 升级失败的条目已被
/// 它摘掉）——S14 之前这里读 `agent_manager` 的强引用列表，那是一个多余的
/// 依赖：既然人格登记本身就是弱引用，"谁还活着"不需要再问第二个人。
fn prune_deleted_personas(rt: &FederationRuntime) {
    let live: std::collections::HashSet<String> = rt
        .personas
        .try_read()
        .map(|list| {
            list.iter()
                .filter(|(_, weak)| weak.strong_count() > 0)
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut slot = rt
        .remote_tool_keepers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut dropped = 0usize;
    for bucket in slot.values_mut() {
        let before = bucket.len();
        bucket.retain(|persona, _| live.contains(persona));
        dropped += before - bucket.len();
    }
    slot.retain(|_, bucket| !bucket.is_empty());
    if dropped > 0 {
        tracing::debug!(
            target: "federation",
            dropped,
            "清掉已删除人格的代理工具保活句柄"
        );
    }
}

/// **新建人格**：把当前已 Up 的全部对端的代理工具装进它（2026-11 S13）。
///
/// 补的是原来的静默缺口：`register_remote_tools` 只在 Link Up 时跑一次，
/// 之后新建的人格**拿不到任何远程工具**（面板看得见这个人格、模型看不见
/// `peer__read_file`，且不报错），要等下次断线重连才补上。
///
/// 幂等：已注册过的人格再调一次只会覆盖自己的桶（先删后插），不会重复注册。
pub(crate) async fn adopt_active_links(
    rt: &FederationRuntime,
    agent: &std::sync::Arc<echo_agent::Agent>,
    persona_id: &str,
) {
    // 先清扫：这个入口在每次人格新建时都会跑，是顺手的回收点。
    prune_deleted_personas(rt);
    // 当前 Up 的 peer：`peer_names` 是 (配置名 → node_id) 的登记表。
    let peers: Vec<(String, String)> = rt
        .peer_names
        .read()
        .await
        .iter()
        .map(|(name, node)| (name.clone(), node.clone()))
        .collect();
    if peers.is_empty() {
        return;
    }
    for (peer_name, node_id) in peers {
        let Some(info) = rt.federation.peer_info(&peer_name).await else {
            continue;
        };
        // 防御：登记表与 PeerInfo 可能短暂不一致（刚断链）——以 node_id 为准。
        if info.node_id != node_id {
            continue;
        }
        let keepers = register_remote_tools_for_agent(rt, agent, &info, &peer_name).await;
        if keepers.is_empty() {
            continue;
        }
        let package = peer_tool_package(&peer_name);
        let count = keepers.len();
        // 记下**工具名**而不只是数量：这条日志是排查"新人格为什么看不见
        // 远程工具"的唯一线索（此前连数量都没有）。
        let installed: Vec<String> = echo_agent::federation::remote_tool_candidates(&info.caps)
            .iter()
            .map(|tool| echo_agent::federation::remote_tool_name(&peer_name, tool))
            .collect();
        {
            let mut slot = rt
                .remote_tool_keepers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.entry(node_id.clone())
                .or_default()
                .insert(persona_id.to_string(), keepers);
        }
        tracing::info!(
            target: "federation",
            persona = %persona_id, peer = %node_id, name = %peer_name,
            package = %package, tools = count, installed = ?installed,
            "persona 新建：补装已在线的远程代理工具"
        );
    }
}

#[cfg(test)]
mod remote_tool_install_tests {
    //! 代理工具**按人格安装**（2026-11 S13）。
    //!
    //! 这一组测试守的是一个静默缺口：`register_remote_tools` 曾在**只有**
    //! 链路 Up 一个调用点，于是"链路已通之后新建的人格"拿不到任何 `peer__*`
    //! 工具——面板看得见它、模型看不见，且不报错。
    //!
    //! 拆分之后两条路径复用 [`super::register_remote_tools_for_agent`]，所以
    //! 这里直接测那一段（它才知道"给谁装、装成什么样"）。

    use std::sync::Arc;

    /// 最小 agent（不跑模型，只要能装工具）。
    struct NullProvider;

    #[async_trait::async_trait]
    impl echo_agent::llm::LlmProvider for NullProvider {
        fn name(&self) -> &str {
            "null"
        }
        fn default_model(&self) -> &str {
            "null"
        }
        async fn chat(
            &self,
            _request: &echo_agent::llm::ChatRequest,
        ) -> Result<echo_agent::llm::ChatResponse, echo_agent::llm::LlmError> {
            Err(echo_agent::llm::LlmError::Config("not used".into()))
        }
        async fn chat_stream(
            &self,
            request: &echo_agent::llm::ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<echo_agent::llm::ChatChunk>,
        ) -> Result<echo_agent::llm::ChatResponse, echo_agent::llm::LlmError> {
            self.chat(request).await
        }
    }

    fn new_agent(team: &str) -> Arc<echo_agent::Agent> {
        let agent = Arc::new(echo_agent::Agent::new(
            Arc::new(NullProvider),
            echo_agent::AgentConfig::default(),
            echo_agent::skill::SkillRegistry::new(),
            echo_agent::tool::ToolRegistry::new(),
            Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
        ));
        agent.set_team_id(Some(team.to_string()));
        agent
    }

    /// 造一个"够用"的联邦运行时（不监听、不连出：只要句柄）。
    pub(super) fn runtime(dir: &std::path::Path) -> Arc<super::FederationRuntime> {
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let federation = echo_federation::Federation::new(
            "node-local".into(),
            None,
            echo_federation::NodeCaps::default(),
            Vec::new(),
            None,
            event_tx,
            shutdown_tx,
        );
        Arc::new(super::FederationRuntime {
            federation,
            router: echo_agent::federation::InvokeRouter::new("node-local".into()),
            peer_names: Arc::new(tokio::sync::RwLock::new(Default::default())),
            node_to_peer: Arc::new(tokio::sync::RwLock::new(Default::default())),
            config_store: echo_config::ConfigStore::new(dir.join("core.toml")),
            listen: String::new(),
            advertise: String::new(),
            node_name: None,
            peer_policies: Arc::new(tokio::sync::RwLock::new(Default::default())),
            remote_tool_keepers: std::sync::Mutex::new(Default::default()),
            merged_management_token: None,
            invite_placeholders: std::sync::Mutex::new(Default::default()),
            session_import_acks: std::sync::Mutex::new(Default::default()),
            personas: tokio::sync::RwLock::new(Vec::new()),
            event_fallback: std::sync::RwLock::new(None),
        })
    }

    fn peer_info(caps: echo_federation::NodeCaps) -> echo_federation::PeerInfo {
        echo_federation::PeerInfo {
            node_id: "node-remote".into(),
            node_name: Some("gpu-box".into()),
            version: "0.0.0".into(),
            caps,
            advertise: None,
            peer_name: "gpu-box".into(),
        }
    }

    /// 核心行为：给**一个**人格装远程工具 = 它自己的注册表里出现 `peer__*`，
    /// 且打到正确的包标签（Down 时按包整包禁用，标错就禁不掉）。
    #[tokio::test]
    async fn installing_for_one_persona_fills_only_that_persona() {
        let dir = std::env::temp_dir().join(format!("echo-adopt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = runtime(&dir);
        let alice = new_agent("alice");
        let bob = new_agent("bob");

        let caps = echo_federation::NodeCaps {
            tools: vec!["read_file".into(), "bash".into()],
            ..Default::default()
        };
        // 本机 schema 复制要用到同名工具定义；没有定义时 proxy_schema 仍会
        // 产出可调用工具（描述退化为简版），这正是它设计上的降级路径。
        let keepers = super::register_remote_tools_for_agent(&rt, &alice, &peer_info(caps), "gpu-box")
            .await;
        assert_eq!(keepers.len(), 2, "两个候选工具都该注册");
        let names = alice.tools.names();
        assert!(
            names.iter().any(|n| n == "gpu-box__read_file"),
            "alice 必须有 gpu-box__read_file：{names:?}"
        );
        assert!(names.iter().any(|n| n == "gpu-box__bash"), "{names:?}");

        // 包标签：Down 时按它整包禁用（`set_package_enabled`）。标错就禁不掉，
        // 对端离线后模型仍会看到并调用 `gpu-box__*`（2026-10 审计 P0-1 的形状）。
        assert_eq!(
            alice.tools.package_of("gpu-box__read_file").as_deref(),
            Some("echo-agent.federation.gpu-box"),
            "包标签必须是 echo-agent.federation.<配置名>，且 Down 侧按同一套算"
        );
        assert_eq!(
            alice.tools.package_of("gpu-box__bash").as_deref(),
            Some("echo-agent.federation.gpu-box")
        );

        // bob 完全不受影响——这正是"按人格装"的意义。
        assert!(
            bob.tools.names().is_empty(),
            "给 alice 装工具不得污染 bob：{:?}",
            bob.tools.names()
        );

        // 保活句柄 drop 即注销（RAII 纪律：临时值会让工具当场消失）。
        drop(keepers);
        assert!(
            !alice
                .tools
                .names()
                .iter()
                .any(|n| n == "gpu-box__read_file"),
            "Disposer drop 后工具必须注销：{:?}",
            alice.tools.names()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 对端没宣告任何可远程工具 → 不装、返回空句柄（不是错误）。
    #[tokio::test]
    async fn peer_without_remote_tools_installs_nothing() {
        let dir = std::env::temp_dir().join(format!("echo-adopt-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = runtime(&dir);
        let alice = new_agent("alice");
        let keepers = super::register_remote_tools_for_agent(
            &rt,
            &alice,
            &peer_info(echo_federation::NodeCaps::default()),
            "gpu-box",
        )
        .await;
        assert!(keepers.is_empty());
        assert!(alice.tools.names().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 人格登记表（2026-11 S14）：登记 / 去重 / 弱引用自动淘汰。
    ///
    /// 这一组替掉的是 `AgentSupervisor` 依赖：「谁还活着」由人格自己报，
    /// 死后自动消失（`Weak` 升级失败），不需要任何"清扫已删人格"的路径。
    #[tokio::test]
    async fn persona_registry_tracks_liveness_without_a_supervisor() {
        let dir = std::env::temp_dir().join(format!("echo-pregistry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = runtime(&dir);

        let alice = new_agent("alice");
        rt.register_persona("alice", &alice).await;
        assert_eq!(rt.live_personas().await.len(), 1);
        assert!(rt.persona_by_id("alice").await.is_some());

        // 同 id 覆盖（人格重建）：不会留下两条记录。
        let alice2 = new_agent("alice");
        rt.register_persona("alice", &alice2).await;
        let live = rt.live_personas().await;
        assert_eq!(live.len(), 1, "同 id 必须覆盖而不是追加：{}", live.len());
        assert!(
            Arc::ptr_eq(&live[0].1, &alice2),
            "覆盖后必须是新实例"
        );

        // 旧实例被 drop（模拟人格被删除）→ 自动从表里消失。
        //
        // 注意 `drop(live)`：`live_personas()` 返回的是**强引用**，调用方拿着
        // 它期间人格就是活的——这是对的行为（发事件时不能让人格跑到一半消失），
        // 但测试里必须先放手才能断言"死了就消失"。
        drop(live);
        drop(alice);
        drop(alice2);
        assert!(
            rt.live_personas().await.is_empty(),
            "drop 掉的人格不该还活着"
        );
        assert!(rt.persona_by_id("alice").await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无人格时的事件出口回退到 `__core`，且没有兜底时**只 warn 不 panic**。
    ///
    /// 这条守的是"全部人格被禁用"的场景：联邦状态事件不能静默丢失，也不能
    /// 让泵直接崩掉。
    #[tokio::test]
    async fn federation_events_fall_back_when_no_persona_is_alive() {
        let dir = std::env::temp_dir().join(format!("echo-pevent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = runtime(&dir);

        // ① 什么都没有：不 panic（只记 warn）。
        rt.emit_federation_event(echo_protocol::BackendEvent::Error {
            session_id: None,
            message: "x".into(),
        })
        .await;

        // ② 设了兜底出口 → 事件经它发出（用 sink 断言）。
        let core = new_agent("__core");
        rt.set_event_fallback(&core);
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        core.attach_event_sink(Arc::new(move |event| {
            sink.lock().unwrap().push(event.kind_name().to_string());
        }));
        rt.emit_federation_event(echo_protocol::BackendEvent::Error {
            session_id: None,
            message: "y".into(),
        })
        .await;
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["Error"],
            "无人格时必须走兜底出口"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **新建人格**的入口：没有在线 peer 时是**静默 no-op**（不 panic、不阻塞）。
    ///
    /// 这条守的是组合根 factory 的那次 `tokio::spawn`：它在每个人格构造时都会
    /// 跑（包括启动期的全部人格，那时链路还没起），所以必须便宜且安全。
    #[tokio::test]
    async fn adopt_active_links_is_a_cheap_noop_without_peers() {
        let dir = std::env::temp_dir().join(format!("echo-adopt-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = runtime(&dir);
        let bob = new_agent("bob");
        super::adopt_active_links(&rt, &bob, "bob").await;
        assert!(bob.tools.names().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod core_util_tests {
    use super::online_node_for_peer;
    use crate::federation_import::{chunk_utf8, handle_session_import, ImportOutcome};
    use crate::pick_promoted_peer_name;
    use crate::learnable_peer_url;

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
        let rt = super::remote_tool_install_tests::runtime(std::path::Path::new("/tmp"));
        let mut imports = std::collections::HashMap::new();
        let (acked, outcome) =
            handle_session_import(&rt, &import_frame(0, 2, "[]"), &mut imports);
        assert_eq!(acked, 1);
        assert!(matches!(outcome, ImportOutcome::Pending));
        let (acked, outcome) =
            handle_session_import(&rt, &import_frame(1, 2, ""), &mut imports);
        assert_eq!(acked, 2);
        match outcome {
            ImportOutcome::Failed(message) => assert!(message.contains("不存在")),
            _ => panic!("expected terminal failure"),
        }
    }

    /// 重复块（重发）幂等：已确认前缀不因重复而倒退。
    #[test]
    pub(crate) fn session_import_duplicate_chunks_are_idempotent() {
        let rt = super::remote_tool_install_tests::runtime(std::path::Path::new("/tmp"));
        let mut imports = std::collections::HashMap::new();
        let (acked, _) =
            handle_session_import(&rt, &import_frame(0, 3, "[]"), &mut imports);
        assert_eq!(acked, 1);
        // 重发第 0 块：ack 仍为 1，不倒退。
        let (acked, _) =
            handle_session_import(&rt, &import_frame(0, 3, "[]"), &mut imports);
        assert_eq!(acked, 1);
        // 乱序：先到第 2 块不推进前缀（ack 仍 1）。
        let (acked, _) = handle_session_import(&rt, &import_frame(2, 3, ""), &mut imports);
        assert_eq!(acked, 1);
    }

    /// 防御：超限单块（>192KB）拒收，不进入缓冲（接收侧尺寸校验）。
    #[test]
    pub(crate) fn session_import_rejects_oversized_chunk() {
        let rt = super::remote_tool_install_tests::runtime(std::path::Path::new("/tmp"));
        let mut imports = std::collections::HashMap::new();
        let oversized = "x".repeat(192 * 1024 + 1);
        let (acked, outcome) =
            handle_session_import(&rt, &import_frame(0, 1, &oversized), &mut imports);
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
        let rt = super::remote_tool_install_tests::runtime(std::path::Path::new("/tmp"));
        let mut imports = std::collections::HashMap::new();
        let (acked, outcome) =
            handle_session_import(&rt, &import_frame(0, 0, "[]"), &mut imports);
        assert_eq!(acked, 0);
        assert!(matches!(outcome, ImportOutcome::Failed(_)));
        assert!(imports.is_empty());
    }
}
