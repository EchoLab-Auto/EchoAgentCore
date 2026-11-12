//! 主对话流式增量转发（`AgentContentDelta` / `AgentReasoningDelta`）。
//!
//! 2026-10 架构评审：自 `agent/mod.rs` 拆出（纯搬移，行为不变）。

use super::*;

/// 流式增量转发任务（每 step 一个；`AgentContentDelta` / `AgentReasoningDelta`）。
///
/// provider 的 `chat_stream` 把 token 级增量送入 `rx`；本任务按 ~40ms 窗口
/// （或累计 ≥256 字符）合并成批再发射——token 级直发会产生每步几十上百个
/// WS 帧。`rx` 关闭（provider 结束、或调用方 future 被取消丢弃）后做最终
/// flush 并退出；调用方在成功路径上先 `await` 本任务再返回，保证增量先于
/// 本 step 的后续事件（工具卡 / 终值 `AgentOutput`/`AgentReasoning`）送达。
///
/// 事件构造不带 `team_id`（由 emit handle 内的 `annotate_team_for` 统一注入）。
pub(super) fn spawn_delta_forwarder(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ChatChunk>,
    emit: std::sync::Arc<dyn Fn(BackendEvent) + Send + Sync>,
    session_id: String,
    branch_id: String,
) -> tokio::task::JoinHandle<()> {
    /// 合并窗口：每次至多一个批（前一批未满窗口也送出，保实时观感）。
    const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(40);
    /// 单批字符上限触发条件：大段输出（长句/代码块）免受窗口节流。
    const FLUSH_CHARS: usize = 256;
    tokio::spawn(async move {
        let mut content = String::new();
        let mut reasoning = String::new();
        let flush = |content: &mut String, reasoning: &mut String| {
            // 推理先于正文（上游时序：thinking → text）。
            if !reasoning.is_empty() {
                emit(BackendEvent::AgentReasoningDelta {
                    session_id: session_id.clone(),
                    team_id: None,
                    branch_id: branch_id.clone(),
                    delta: std::mem::take(reasoning),
                });
            }
            if !content.is_empty() {
                emit(BackendEvent::AgentContentDelta {
                    session_id: session_id.clone(),
                    team_id: None,
                    branch_id: branch_id.clone(),
                    delta: std::mem::take(content),
                });
            }
        };
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // 跳过立即触发的第一拍
        loop {
            tokio::select! {
                chunk = rx.recv() => match chunk {
                    Some(chunk) => {
                        if let Some(delta) = chunk.content_delta {
                            content.push_str(&delta);
                        }
                        if let Some(delta) = chunk.reasoning_delta {
                            reasoning.push_str(&delta);
                        }
                        if content.len() + reasoning.len() >= FLUSH_CHARS {
                            flush(&mut content, &mut reasoning);
                        }
                    }
                    None => {
                        flush(&mut content, &mut reasoning);
                        break;
                    }
                },
                _ = ticker.tick() => {
                    if !content.is_empty() || !reasoning.is_empty() {
                        flush(&mut content, &mut reasoning);
                    }
                }
            }
        }
    })
}
