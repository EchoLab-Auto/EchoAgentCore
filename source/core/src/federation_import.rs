//! 联邦会话迁移（P3-2b 分块搬运）与跨机子代理聚合摘要投递。
//!
//! 从 `main.rs` 拆出（框架优化）：组合根本职只留装配，迁移机制（分块/
//! 窗口/ack/重发/导入）与聚合摘要投递独立成模块。行为零变化的纯移动。
//!
//! ## 会话迁移语义（P3-2）
//!
//! 本机 timeline 导出 → 经联邦 Query(SessionSnapshot) 推送会话到目标
//! peer（目标侧经 Invoke 调 workspace 的保存路径导入）。v1 语义：
//! **复制式迁移**——目标侧得到带全部历史的新会话；源会话保留但标注
//! migrated（提示后续到目标侧继续）。
//!
//! ## 聚合摘要投递（P3-3）
//!
//! 向父会话 timeline 追加一条 assistant 系统消息（各 peer 状态 +
//! 结果摘要 + 成败统计）。

use std::sync::Arc;

use crate::{agent_supervisor, FederationRuntime};

pub(crate) async fn deliver_aggregate_summary(
    supervisor: &agent_supervisor::AgentSupervisor,
    summary: echo_agent::federation::aggregator::AggregateSummary,
) {
    for persona in supervisor.personas() {
        if persona.agent.trunk.get(&summary.session_id).is_some() {
            persona
                .agent
                .trunk
                .push_timeline(echo_protocol::TimelineMessage {
                    seq: 0, // push_timeline 内部分配
                    kind: "system".into(),
                    content: summary.text.clone(),
                    session_id: summary.session_id.clone(),
                    time: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0),
                    source: None,
                    reasoning: None,
                    tool: None,
                    images: None,
                });
            return;
        }
    }
    tracing::warn!(session = %summary.session_id, "aggregate summary: parent session not found");
}

/// 单次迁移分块上限（字节，UTF-8 边界对齐）。
const SESSION_IMPORT_CHUNK_BYTES: usize = 192 * 1024;
/// 迁移分块数上限（防御：192KB × 4096 ≈ 768MB，超限拒绝）。
const SESSION_IMPORT_MAX_CHUNKS: usize = 4096;
/// 发送窗口：目标每块回 ack，源最多领先窗口块（联邦出站队列 256 帧上限的
/// 安全余量，避免 try_send 满载失败）。
const SESSION_IMPORT_WINDOW: u32 = 32;
/// 未收到 ack 推进时的重发等待。
const SESSION_IMPORT_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// 整次迁移总时限（防御挂死）。
const SESSION_IMPORT_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

/// 按 UTF-8 字符边界把文本切成长度 ≤ `max_bytes` 的块（末块可为空串）。
pub(crate) fn chunk_utf8(text: &str, max_bytes: usize) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        let mut end = (start + max_bytes).min(bytes.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = bytes.len(); // 防御：单字符超限时整段送出
        }
        out.push(text[start..end].to_string());
        start = end;
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// 组装迁移回执帧。
pub(crate) fn session_import_result(
    transfer_id: &str,
    session_id: &str,
    target_peer: &str,
    success: bool,
    imported_events: usize,
    message: String,
) -> echo_federation::FedFrame {
    echo_federation::FedFrame::SessionImportResult(echo_federation::SessionImportResultFrame {
        transfer_id: transfer_id.to_string(),
        session_id: session_id.to_string(),
        target_peer: target_peer.to_string(),
        success,
        imported_events,
        message,
    })
}

/// 目标侧会话导入缓冲：transfer_id → 有序分块 + 回执标签。
pub(crate) struct ImportBuffer {
    chunks: Vec<Option<String>>,
    session_id: String,
    team_id: String,
}

/// 凑齐后的导入任务：由泵 spawn 到阻塞线程执行（大日志投影不阻塞联邦泵）。
pub(crate) struct ImportReady {
    trunk: echo_agent::TrunkStore,
    session_id: String,
    transfer_id: String,
    target_peer: String,
    team_id: String,
    events: Vec<echo_session::SessionEvent>,
}

/// 一个分块的处理结果。
pub(crate) enum ImportOutcome {
    /// 还没凑齐：继续等待后续分块。
    Pending,
    /// 终态失败（非法头 / 解析失败 / 人格不存在）。
    Failed(String),
    /// 全部到齐：交给调用方 spawn 导入并回终态（Box 收敛 enum 大小差异）。
    Ready(Box<ImportReady>),
}

/// 连续收到的块数（前缀）；重发块幂等覆盖，不影响该值。
pub(crate) fn contiguous_acked(chunks: &[Option<String>]) -> u32 {
    let mut acked = 0u32;
    for chunk in chunks {
        if chunk.is_some() {
            acked += 1;
        } else {
            break;
        }
    }
    acked
}

/// 目标侧处理一个 `SessionImport` 分块。返回 `(acked_upto, outcome)`：
/// `acked_upto` 是连续收到的块数（回 ack 推进源窗口 / 触发重发）。
pub(crate) fn handle_session_import(
    personas: &agent_supervisor::AgentSupervisor,
    frame: &echo_federation::SessionImportFrame,
    imports: &mut std::collections::HashMap<String, ImportBuffer>,
) -> (u32, ImportOutcome) {
    let total = frame.chunk_total as usize;
    if total == 0 || frame.chunk_index as usize >= total || total > SESSION_IMPORT_MAX_CHUNKS {
        return (0, ImportOutcome::Failed("非法分块头".into()));
    }
    let entry = imports
        .entry(frame.transfer_id.clone())
        .or_insert_with(|| ImportBuffer {
            chunks: vec![None; total],
            session_id: frame.session_id.clone(),
            team_id: frame.team_id.clone(),
        });
    if entry.chunks.len() != total {
        // 同一 transfer_id 分块总数变化 → 协议错误，按新头重置。
        entry.chunks = vec![None; total];
    }
    entry.chunks[frame.chunk_index as usize] = Some(frame.data.clone());
    let acked = contiguous_acked(&entry.chunks);
    if acked as usize != total {
        return (acked, ImportOutcome::Pending);
    }
    let entry = imports.remove(&frame.transfer_id).expect("entry present");
    let json: String = entry
        .chunks
        .into_iter()
        .map(|c| c.unwrap_or_default())
        .collect();
    let events: Vec<echo_session::SessionEvent> = match serde_json::from_str(&json) {
        Ok(events) => events,
        Err(error) => {
            return (
                total as u32,
                ImportOutcome::Failed(format!("解析事件日志失败：{error}")),
            );
        }
    };
    let Some(persona) = personas.get_exact(&entry.team_id) else {
        return (
            total as u32,
            ImportOutcome::Failed(format!("人格 {} 不存在或未运行", entry.team_id)),
        );
    };
    (
        total as u32,
        ImportOutcome::Ready(Box::new(ImportReady {
            trunk: persona.agent.trunk.clone(),
            session_id: entry.session_id,
            transfer_id: frame.transfer_id.clone(),
            target_peer: frame.target_peer.clone(),
            team_id: entry.team_id,
            events,
        })),
    )
}

/// 在阻塞线程执行导入并回终态（避免大日志投影卡住联邦泵）。
pub(crate) async fn run_session_import(
    rt: Arc<FederationRuntime>,
    from: String,
    ready: ImportReady,
) {
    let ImportReady {
        trunk,
        session_id,
        transfer_id,
        target_peer,
        team_id,
        events,
    } = ready;
    let index = events.len();
    let sid = session_id.clone();
    let imported =
        tokio::task::spawn_blocking(move || trunk.import_session_events(&sid, events)).await;
    let (success, imported_events, message) = match imported {
        Ok(()) => (true, index, format!("已导入至人格 {team_id}")),
        Err(error) => (false, 0, format!("导入任务失败：{error}")),
    };
    let frame = session_import_result(
        &transfer_id,
        &session_id,
        &target_peer,
        success,
        imported_events,
        message,
    );
    let _ = rt.federation.send_to(&from, frame).await;
}

/// 进行中的迁移 ack 通道：transfer_id → 已确认块数。
/// 源侧 `handle_migrate_session` 注册，泵收到 `SessionImportAck` 时推进。
/// 存储收敛于 [`echo_context::kernel`]（P1 唯一引导单元）：cell 内保存
/// 惰性泄漏的 `&'static Mutex<…>` 句柄（init 只执行一次，句柄生命周期
/// 与旧的进程级存储一致），对外签名与行为不变。
pub(crate) fn session_import_acks(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, tokio::sync::watch::Sender<u32>>> {
    echo_context::kernel::get_or_init(|| {
        let registry: &'static std::sync::Mutex<
            std::collections::HashMap<String, tokio::sync::watch::Sender<u32>>,
        > = Box::leak(Box::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )));
        registry
    })
}

/// 泵收到目标 ack：推进对应迁移的已确认游标（无接收者时忽略）。
pub(crate) fn note_session_import_ack(transfer_id: &str, acked_upto: u32) {
    if let Ok(map) = session_import_acks().lock() {
        if let Some(tx) = map.get(transfer_id) {
            let _ = tx.send(acked_upto);
        }
    }
}

/// 会话迁移（P3-2b）：源侧导出会话事件日志 → 按块经联邦送到目标节点 →
/// 目标导入到同名 persona 的事实来源日志 → 回执驱动结果事件。
pub(crate) async fn handle_migrate_session(
    emit: std::sync::Arc<dyn Fn(echo_protocol::BackendEvent) + Send + Sync>,
    rt: &FederationRuntime,
    session_id: String,
    team_id: Option<String>,
    target_peer: String,
) {
    let emit_fail = emit.clone();
    let sid = session_id.clone();
    let tp = target_peer.clone();
    let fail = move |message: String| {
        emit_fail(echo_protocol::BackendEvent::SessionMigrated {
            session_id: sid.clone(),
            target_peer: tp.clone(),
            new_session_id: None,
            message,
            success: false,
        });
    };
    // 1) 目标 peer 在线 + node_id 反查。
    let Some(node_id) = rt.peer_names.read().await.get(&target_peer).cloned() else {
        fail(format!("目标 peer 「{target_peer}」不在线或不存在"));
        return;
    };
    // 2) 定位源 persona + 该会话的事件（事实来源日志按 session 过滤）。
    let Some(mgr) = echo_agent::agent_manager::global_manager() else {
        fail("agent manager unavailable".into());
        return;
    };
    let personas = mgr.all();
    let persona = match team_id.as_deref() {
        Some(team) => personas.iter().find(|p| p.id == team),
        None => personas
            .iter()
            .find(|p| p.agent.trunk.get(&session_id).is_some()),
    };
    let Some(persona) = persona else {
        fail(match team_id.as_deref() {
            Some(team) => format!("人格 {team} 不存在或未运行"),
            None => format!("会话 {session_id} 不存在"),
        });
        return;
    };
    if persona.agent.trunk.get(&session_id).is_none() {
        fail(format!("会话 {session_id} 在人格 {} 下不存在", persona.id));
        return;
    }
    let events: Vec<echo_session::SessionEvent> = persona
        .agent
        .trunk
        .event_log()
        .into_iter()
        .filter(|event| event.session() == Some(session_id.as_str()))
        .collect();
    if events.is_empty() {
        fail(format!("会话 {session_id} 无可迁移事件"));
        return;
    }
    let json = match serde_json::to_string(&events) {
        Ok(json) => json,
        Err(error) => {
            fail(format!("序列化会话失败：{error}"));
            return;
        }
    };
    let chunks = chunk_utf8(&json, SESSION_IMPORT_CHUNK_BYTES);
    if chunks.len() > SESSION_IMPORT_MAX_CHUNKS {
        fail(format!(
            "会话过大（{} 块 > 上限 {SESSION_IMPORT_MAX_CHUNKS}）",
            chunks.len()
        ));
        return;
    }
    let transfer_id = echo_federation::new_call_id(rt.federation.local_node_id());
    let total = chunks.len() as u32;
    let source_team = persona.id.clone();
    // 注册 ack 通道（泵收到目标 SessionImportAck 时推进）。
    let (ack_tx, mut ack_rx) = tokio::sync::watch::channel(0u32);
    {
        let mut map = session_import_acks().lock().expect("ack registry poisoned");
        map.insert(transfer_id.clone(), ack_tx);
    }
    // 窗口流控 + 超时重发：目标每收一块回 ack（acked_upto）。联邦出站队列
    // 只有 256 帧且 `try_send` 满载即错——无流控时大日志必然中途失败。
    let send_result: Result<(), String> = {
        let started = tokio::time::Instant::now();
        let mut acked = 0u32;
        let mut next = 0u32;
        loop {
            if acked >= total {
                break Ok(());
            }
            if next < total && next < acked.saturating_add(SESSION_IMPORT_WINDOW) {
                let frame =
                    echo_federation::FedFrame::SessionImport(echo_federation::SessionImportFrame {
                        transfer_id: transfer_id.clone(),
                        session_id: session_id.clone(),
                        team_id: source_team.clone(),
                        target_peer: target_peer.clone(),
                        chunk_index: next,
                        chunk_total: total,
                        data: chunks[next as usize].clone(),
                    });
                match rt.federation.send_to(&node_id, frame).await {
                    Ok(()) => next += 1,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                }
            } else {
                match tokio::time::timeout(SESSION_IMPORT_ACK_TIMEOUT, ack_rx.changed()).await {
                    Ok(Ok(())) => acked = *ack_rx.borrow(),
                    Ok(Err(_)) => break Err("迁移确认通道关闭".into()),
                    // 超时：从已确认处重发（目标对重复块幂等）。
                    Err(_) => next = acked,
                }
            }
            if started.elapsed() > SESSION_IMPORT_TOTAL_TIMEOUT {
                break Err(format!("迁移超时（已确认 {acked}/{total} 块）"));
            }
        }
    };
    session_import_acks()
        .lock()
        .expect("ack registry poisoned")
        .remove(&transfer_id);
    if let Err(error) = send_result {
        fail(error);
        return;
    }
    let message = format!(
        "已向「{target_peer}」发送 {total} 个分块（{} 条事件），等待目标确认",
        events.len()
    );
    emit(echo_protocol::BackendEvent::SessionMigrated {
        session_id,
        target_peer,
        new_session_id: None, // 待目标 SessionImportResult 回执确认
        message,
        success: true,
    });
}
