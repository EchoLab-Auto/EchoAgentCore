//! 会话写命令域的**宿主能力实现**（解耦计划 S6 补口）。
//!
//! 与 `session_view_host` / `provider_host` 同构：**校验、文案、进度、时间线
//! 重发在模块**（`agent_module_session::commands`），宿主只提供"只有它做得了"
//! 的几件事：
//!
//! | 宿主提供 | 唯一性来源 |
//! |---|---|
//! | [`resolve_trunk`](SessionCommandHost::resolve_trunk) | 人格注册表在 `AgentManager`；模块不认识它 |
//! | [`compact`](SessionCommandHost::compact) | 压缩策略在 `agent-module-loop`、provider 在 `Agent.model`——模块不能依赖模块 |
//! | [`emit_context_snapshot`](SessionCommandHost::emit_context_snapshot) | 快照要算 token 预算与上下文块 |
//! | [`forget_session_provider`](SessionCommandHost::forget_session_provider) | 会话级 API 覆盖表在 `Agent.model` |
//! | [`quiesce_session`](SessionCommandHost::quiesce_session) | 在途 turn 登记表与每会话 `turn_queue` 在宿主 |
//!
//! `trunk` / `resolve_trunk` 返回 `Arc<TrunkStore>` 是**语义快照**而非借用：
//! 模块拿到的是可独立持有的句柄，不必认识宿主的存储布局（与 `session_view`
//! 的"接缝不返回引用"同一条纪律）。
//!
//! `quiesce_session` 是 PR3 的第五件事：清理/归档前必须让目标会话的在途
//! turn 退出（取消 + 限时拿串行闸门），否则旧 turn 可能在 clear 之后 commit
//! 一条回复、或与新 turn 读到清空前的上下文。它只覆盖走 `turn_queue` 的
//! 入站 turn，**不**声称排空所有写者——旁路清单见该方法文档。

use std::sync::Arc;
use std::time::Duration;

use agent_module_management::protocol::BackendEvent;
use agent_module_session::commands::SessionCommandHost;
use agent_module_session::TrunkStore;

use crate::agent::Agent;

/// clear / archive 前等待在途 turn 退出的上限（模块侧拿到 Err 会拒绝执行
/// 写操作并回一条失败回执——宁可不清理，也不能与旧 turn 并发写同一会话）。
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(30);
/// 无 Session 身份可拿闸门时的轮询步长（只在旧 turn 已登记的场景用）。
const QUIESCE_POLL: Duration = Duration::from_millis(10);

/// 宿主适配器：以"某个 persona"为缺省视角的会话写命令。
pub struct AgentSessionCommandHost {
    agent: Arc<Agent>,
    bound: bool,
}

impl AgentSessionCommandHost {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self {
            agent,
            bound: false,
        }
    }

    /// 解析目标 persona（`None` / 解析不到 = 本 agent）。
    fn target(&self, team_id: &str) -> Option<Arc<Agent>> {
        self.agent
            .registry()
            .and_then(|manager| manager.resolve(team_id))
    }

    /// 精确解析命令的目标 persona（与模块 `Target::resolve` 同一口径）：
    /// 先查注册表，注册表没有时**只允许**回退到本 agent（team id 相同）；
    /// 解析不到且不是自己 → 明确报错，绝不把 A 的 session 按 B 的意图清理。
    fn resolve_agent(&self, team_id: Option<&str>) -> Result<Arc<Agent>, String> {
        if self.bound {
            return if team_id.is_none() || team_id == self.agent.team_id().as_deref() {
                Ok(Arc::clone(&self.agent))
            } else {
                Err("绑定的命令目标与 team 不一致".into())
            };
        }
        match team_id {
            None => Ok(Arc::clone(&self.agent)),
            Some(id) => {
                if let Some(target) = self.target(id) {
                    return Ok(target);
                }
                if self.agent.team_id().as_deref() == Some(id) {
                    Ok(Arc::clone(&self.agent))
                } else {
                    Err(format!("team {id} 不存在"))
                }
            }
        }
    }

    /// 等某个 session 的**已登记** turn 全部退表。
    ///
    /// 只在"Session 身份已不在 trunk 里"时用：没有 `Session` 就借不到它的
    /// `turn_queue`，能观察到的最强信号是 `active_inbound_turns` 里不再有
    /// 该会话（`finish_inbound_turn` 在 turn 收尾时出表）。注意这**不是**
    /// 完整的写者排空：spawn 后尚未登记的入站任务不在表里（见模块文档的
    /// 旁路清单），这里只做 best-effort 等待。
    async fn wait_registered_turns_gone(agent: &Agent, session_id: &str) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + QUIESCE_TIMEOUT;
        loop {
            if !agent
                .active_turn_session_ids()
                .iter()
                .any(|id| id == session_id)
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "会话 {session_id} 的旧 turn 在 {}s 内未退出（已发送取消），已拒绝执行",
                    QUIESCE_TIMEOUT.as_secs()
                ));
            }
            tokio::time::sleep(QUIESCE_POLL).await;
        }
    }
}

impl SessionCommandHost for AgentSessionCommandHost {
    fn bind_target(
        self: Arc<Self>,
        team_id: Option<&str>,
    ) -> Result<Arc<dyn SessionCommandHost>, String> {
        let agent = self.resolve_agent(team_id)?;
        Ok(Arc::new(Self { agent, bound: true }))
    }

    fn self_team_id(&self) -> Option<String> {
        self.agent.team_id()
    }

    fn resolve_trunk(&self, team_id: &str) -> Option<Arc<TrunkStore>> {
        self.resolve_agent(Some(team_id))
            .ok()
            .map(|agent| Arc::new(agent.trunk.clone()))
    }

    fn trunk(&self) -> Arc<TrunkStore> {
        Arc::new(self.agent.trunk.clone())
    }

    fn forget_session_provider(&self, session_id: &str) {
        self.agent.model.session_providers.remove(session_id);
        // 同时清掉持久化意图，避免下一次 resolve/重启惰性恢复旧覆盖。
        self.agent.model.session_api.clear(session_id);
    }

    /// 在 clear / archive **动手之前**把目标会话的在途 turn 停下来。
    ///
    /// 顺序与理由：
    /// 1. **先按 team 精确路由**到目标 persona（不跨人格误清）；
    /// 2. **取消**该会话所有已登记 turn（排队中的也要取消——它们还没写事件，
    ///    取消后不会再 append）；
    /// 3. **限时 30s 拿 `Session::turn_queue`**：旧 turn 把闸门持有到本轮
    ///    结束（commit / cleanup 都跑完），所以拿到 guard 就保证旧工具/推理
    ///    已退出本轮任务；guard 交给模块持有到写操作与回执结束。新 turn
    ///    在闸门上**阻塞**，不会与 clear/archive 并发写同一会话。
    ///
    /// 无 `Session` 身份时（`evict_idle` 等场景）没有闸门可借：若还有已登记
    /// 旧 turn，只等它们退表（best-effort），否则直接返回空 guard。
    ///
    /// **未覆盖的旁路**（不能声称"所有写者都已排空"）：
    /// - `process_inbound_branch` 在 `tokio::spawn` 与 `try_register` 之间
    ///   的窗口：尚未登记，取消看不到；它可能在本方法返回后拿到**同一把**
    ///   闸门（阻塞等待），也可能在无 Session 身份时用新闸门开跑；
    /// - 不经过 `turn_queue` 的写者：`record_control_reply` /
    ///   `record_assistant_reply` 只取 `turn_lock`，模块压缩（PR2 用
    ///   epoch/前缀校验而非闸门）、后台周期保存，以及平台取消回执；
    /// - 同一 session id 跑在**另一代/另一个** Agent 实例上的 turn（各自
    ///   一把闸门；本方法只清目标 agent 的登记表）；
    /// - 旧 turn 内部自行 spawn、已脱离本轮 future 的后台任务（取消只能
    ///   停止被 await 的那一支，停不掉已 detach 的外部副作用）。
    fn quiesce_session<'a>(
        &'a self,
        team_id: Option<&'a str>,
        session_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn Send>, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            let target = self.resolve_agent(team_id)?;
            // 先取消：已登记的排队 turn 在 `acquire_turn_slot` 的取消分支
            // 返回 None，不会拿到闸门后再 append。
            target.cancel_inbound_turns(session_id, true);
            let Some(session) = target.trunk.get(session_id) else {
                // 没有 Session 身份 = 借不到 turn_queue。有旧 turn 就等它
                // 退表；没有就直接返回空 guard（无可保护的对象）。
                Self::wait_registered_turns_gone(&target, session_id).await?;
                return Ok(Box::new(()) as Box<dyn Send>);
            };
            match tokio::time::timeout(
                QUIESCE_TIMEOUT,
                Arc::clone(&session.turn_queue).lock_owned(),
            )
            .await
            {
                Ok(guard) => Ok(Box::new(guard) as Box<dyn Send>),
                Err(_) => Err(format!(
                    "会话 {session_id} 的在途 turn 在 {}s 内未退出（已发送取消），已拒绝执行",
                    QUIESCE_TIMEOUT.as_secs()
                )),
            }
        })
    }

    fn emit(&self, event: BackendEvent) {
        self.agent.emit(event);
    }

    fn emit_context_snapshot<'a>(
        &'a self,
        session_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.agent.emit_context_snapshot(Some(session_id)).await;
        })
    }

    fn compact<'a>(
        &'a self,
        team_id: Option<String>,
        session_id: &'a str,
        keep_recent: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        // 在创建 future 前绑定一次；等待/执行期间不再查注册表。
        let target = self.resolve_agent(team_id.as_deref());
        Box::pin(async move {
            target?
                .compact_session_history(session_id, keep_recent)
                .await
        })
    }
}

#[cfg(test)]
#[path = "session_commands_host_tests.rs"]
mod tests;
