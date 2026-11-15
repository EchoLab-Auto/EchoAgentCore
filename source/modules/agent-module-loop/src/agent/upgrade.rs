//! 每个 Agent 的**可逆升级准备**（upgrade guard）原语。
//!
//! 这不是退场（retire），也不是 shutdown。它是"升级协调器（global gate）
//! 决定换二进制/换宿主之前，先把**这一个** Agent 带到一个可检查、可回滚的
//! 维护态"：
//!
//! 1. **拒新 turn**，但**不 retire**：`retired` 是单向的（置位后这一代永不
//!    再接活），升级失败就没法回滚；所以这里不碰它。
//! 2. **不取消任何 token**：`CancellationToken` 是一次性的——一旦
//!    `cancel()`，同一代里所有等待方永远看到 cancelled，后台任务也再不能
//!    重启。升级回滚必须是"原样继续跑"，因此本模块**零 `cancel()` 调用**。
//! 3. **等在飞 turn 归零**：用既有的 `wait_inbound_turns_idle`（watch 计数，
//!    不是 sleep 轮询），等它们自己跑完 commit/cleanup。
//! 4. **checked flush**：用 `TrunkStore::save_now_checked()` 拿"本次尝试"
//!    的真实结论（内存态 / 加载失败 / 陈旧快照 / IO 失败都会明确区分），
//!    而不是读 `last_save_error` 的历史值。
//! 5. **后台写者不谎报**：本代后台任务只有周期保存与模块热重载，两者都只
//!    暴露一次性 `CancellationToken`，没有可逆的 pause/resume 钩子。取消
//!    它们 = 这一代不可恢复，直接违背"可逆升级"。所以结果里明确给
//!    [`WriterPauseStatus::Unsupported`]，`writers_stopped()` 恒为 false；
//!    **绝不**用一个假 ACK 让上层以为磁盘写者已经停了。
//!
//! ## 闸门为什么能不改既有方法就挡住新 turn
//!
//! `turns.draining` 是 2026-11 C 就存在的"拒新工作"闸：
//! `process_message`、`process_inbound_branch`（spawn 前 + spawn 内各一次）
//! 和 `try_register_inbound_turn`（入口 + 入表后复查）都检查它。把它置真
//! 即"挡新 turn"；把它复位即"完全恢复"。这正好是可逆升级需要的语义，
//! 不需要在任一既有方法里加分支（本模块也不改它们的签名/行为）。
//!
//! 唯一的外部耦合：`shutdown()` / `begin_retire()` 也会置 `draining`。因此：
//! - 进入 prepare 时若发现 `draining` 已被别人置真（且没有本闸的活跃准备），
//!   拒绝并把原因说清楚（[`UpgradeError::AlreadyDraining`]），**不接管**；
//! - 回滚（[`Agent::cancel_upgrade`]）时若已 `retired` 或两类 token 已取消，
//!   拒绝重开闸门（[`UpgradeCancelError::Retiring`]）——不能让一次迟到的
//!   cancel 把正在退场的一代"复活"。
//!
//! 已知残余竞态：`begin_retire()` / `shutdown()` 不经过本模块的锁，若与
//! `cancel_upgrade` 在"探测 → 写 draining"的纳秒级窗口内并发，可能把
//! 它们刚置真的 `draining` 又清掉。`retired` 仍会在 turn 登记处兜底（不会
//! 真的接新 turn），但进程级升级协调应当保证"退场与回滚互斥"（global gate
//! 的职责），本原语只负责单 Agent 的状态一致。
//!
//! ## 失败语义（fail preserves maintenance）
//!
//! 一旦本闸把 `draining` 置真（进入 prepare），**任何失败都不自动开闸**：
//! - `TurnsNotDrained` / `FlushFailed` / `RetiredDuringPrepare` 都保留
//!   `draining = true` 与活跃代数；
//! - 只有调用方拿着错误里的 `generation` 显式调用
//!   [`Agent::cancel_upgrade`]（且这一代未退场/未取消）才恢复接单。
//!
//! 原因：prepare 已经可能停掉/等待了 turn、写过一次快照。失败后"悄悄恢复"
//! 会让上层以为没发生过什么，而实际状态已经不是进入前的那一份。维护态是
//! 唯一安全的默认值。
//!
//! 这张闸没有 `Drop` 自动释放：持有者 panic / 忘记回滚时，Agent 会保持
//! "拒新 turn"的 fail-closed 状态（安全侧），而不是悄悄恢复接单。要重试
//! 一次失败的 prepare：先 `cancel_upgrade(err.generation())`，再重新 prepare。
//!
//! ## `tx` 是什么
//!
//! `prepare_upgrade(tx, timeout)` 里的 `tx` 是**建议性**的阶段通知通道
//! （[`UpgradeProgressSender`]）：`try_send`，接收端没建/已丢弃/缓冲满都
//! 不影响准备结论。真正的结论只由返回的 checked result 表达。对端不需要
//! 进度时可以调 [`Agent::prepare_upgrade_quiet`]（内部丢弃接收端）。

use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::mpsc;

use super::Agent;

/// 后台写者无法安全暂停的固定原因（对外可展示、可审计）。
///
/// 这不是"暂时没实现"的占位：本代后台写者（周期保存 + 模块热重载）**只有**
/// 一次性取消令牌。取消 = 这一代永久不可恢复，与可逆升级的前提直接冲突。
/// 在新增可逆 pause/resume 钩子之前，任何 `writers_stopped=true` 都是假 ACK。
pub const WRITERS_PAUSE_UNSUPPORTED_REASON: &str =
    "background writers (periodic session save + module hot reload) only stop via a one-shot \
     CancellationToken; cancelling it is irreversible (the token cannot be reused), so a \
     reversible upgrade cannot pause them. No pause/resume hook exists yet.";

/// 升级准备的阶段通知通道（建议性，`try_send`，不阻塞准备）。
pub type UpgradeProgressSender = mpsc::Sender<UpgradePhase>;

/// 升级准备的阶段事件（供 global gate / 日志观察进度）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradePhase {
    /// 维护闸已关闭：新 turn 从此刻起被拒绝（在飞 turn 不受影响）。
    GateClosed,
    /// 在飞 turn 已归零（commit / cleanup 均已结束）。
    TurnsDrained { remaining: usize },
    /// checked flush 已确认成功。
    Flushed,
    /// 后台写者无法安全暂停（明确 Unsupported，绝不假 ACK）。
    WritersUnsupported {
        registered_tasks: usize,
        periodic_save_started: bool,
    },
    /// 准备成功，维护态建立（仍等待显式 cancel）。
    Prepared { generation: u64 },
    /// 准备失败；维护态保留到显式 cancel（见模块文档的失败语义）。
    Failed {
        generation: Option<u64>,
        reason: String,
    },
}

/// 后台写者暂停状态。
///
/// 当前实现**只会**产出 [`Self::Unsupported`]：见
/// [`WRITERS_PAUSE_UNSUPPORTED_REASON`]。保留 `Stopped` 变体是为了给未来
/// 真正实现 pause/resume 的写者一个诚实的成功态，而不是现在就用 bool 混过去。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriterPauseStatus {
    /// 已确认所有后台写者停止（当前实现**不会**返回）。
    Stopped,
    /// 无法安全暂停：写明原因与现场（在册任务数 / 周期保存是否已起过）。
    Unsupported {
        reason: &'static str,
        registered_tasks: usize,
        periodic_save_started: bool,
    },
}

impl WriterPauseStatus {
    /// 是否真的确认"后台写者已停止"。当前恒为 `false`。
    pub fn writers_stopped(&self) -> bool {
        matches!(self, Self::Stopped)
    }

    /// `Unsupported` 时的原因（`Stopped` 时为 `None`）。
    pub fn unsupported_reason(&self) -> Option<&'static str> {
        match self {
            Self::Stopped => None,
            Self::Unsupported { reason, .. } => Some(reason),
        }
    }
}

/// checked flush 的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeFlushOutcome {
    /// `TrunkStore::save_now_checked()` 返回 `Ok`。
    ///
    /// `memory_only = true` = 纯内存 store（没有 persist path）：没有磁盘
    /// 目标可写，不把它误报成"永久未落盘"。
    CheckedOk { memory_only: bool },
}

impl UpgradeFlushOutcome {
    pub fn is_ok(self) -> bool {
        matches!(self, Self::CheckedOk { .. })
    }
}

/// 一次**成功**的升级准备（维护态已建立、checked flush 已确认）。
///
/// 注意：`Ok` 不等于"可以无条件换二进制"。它只说明：
/// - 新 turn 已被挡住；
/// - 在飞 turn 已归零；
/// - 本次 checked flush 成功；
/// - **后台写者没有被停**（见 [`WriterPauseStatus`]）。
///
/// 上层必须用 [`Self::fully_prepared`] 判断"是否连写者都停了"；当前实现
/// 恒为 `false`，global gate 必须显式接受这个保守结论（或未来实现真正的
/// 可逆暂停）。
#[derive(Debug, Clone)]
pub struct UpgradePrepared {
    /// 人格 id（`None` = 进程级核心服务代理 / 未绑定）。
    pub team_id: Option<String>,
    /// 本次准备的代数：回滚时必须原样传回 [`Agent::cancel_upgrade`]。
    pub generation: u64,
    /// 从进入到维护态就绪的耗时。
    pub elapsed: Duration,
    /// 排空确认时的在飞 turn 数（恒 0 = 已确认归零）。
    pub in_flight_turns: usize,
    /// 本次 checked flush 结论（成功才会返回 `Ok`，所以恒为 `CheckedOk`）。
    pub flush: UpgradeFlushOutcome,
    /// 后台写者状态：当前恒 `Unsupported`（绝不谎报 stopped）。
    pub writers: WriterPauseStatus,
    /// 维护态仍由本次准备持有：必须显式 cancel 才会恢复接单。
    pub maintenance_held: bool,
}

impl UpgradePrepared {
    /// 是否达到"完整升级准备"（turn 排空 + flush 成功 + 后台写者真的停了）。
    ///
    /// 当前实现恒为 `false`：写者暂停是 `Unsupported`。这个方法是给
    /// global gate 的**防假 ACK 闸**——只有它为 true 才能宣传 "writers stopped"。
    pub fn fully_prepared(&self) -> bool {
        self.in_flight_turns == 0
            && self.flush.is_ok()
            && self.writers.writers_stopped()
            && self.maintenance_held
    }

    /// 写者是否真的停了（当前恒 `false`）。
    pub fn writers_stopped(&self) -> bool {
        self.writers.writers_stopped()
    }
}

/// 升级准备失败的原因。
///
/// 带 `generation` 的失败表示**维护态已经建立并保留**：调用方必须用这个
/// 代数显式 [`Agent::cancel_upgrade`] 才能恢复接单。
#[derive(Debug, Clone, thiserror::Error)]
pub enum UpgradeError {
    /// 人格已退场 / 正在 shutdown：升级准备没有意义（这一代不会再接活）。
    #[error("persona already retired or shutting down: upgrade preparation refused")]
    Retired,
    /// `draining` 已被别人（shutdown / retire）置真：不接管别人的排空闸。
    #[error("agent is already draining for shutdown/retirement: upgrade preparation refused")]
    AlreadyDraining,
    /// 本 agent 已有一次未回滚的升级准备（含失败后保留的维护态）。
    #[error("another upgrade preparation is still active (generation {active})")]
    AlreadyPrepared { active: u64 },
    /// 等待 prepare/cancel 临界区超时。
    #[error("timed out acquiring the upgrade gate")]
    GateLockTimeout,
    /// 在飞 turn 在窗口内没有归零（**未取消它们**）。
    #[error(
        "in-flight turns did not drain within {timeout:?}: {remaining} still active \
         (generation {generation})"
    )]
    TurnsNotDrained {
        generation: u64,
        remaining: usize,
        timeout: Duration,
    },
    /// checked flush 失败（加载失败 / 陈旧快照 / IO / 超时）。
    #[error("checked session flush failed (generation {generation}): {error}")]
    FlushFailed { generation: u64, error: String },
    /// 准备期间人格退场/停机：维护态保留，但准备不能算成功。
    #[error("persona retired while preparing upgrade (generation {generation})")]
    RetiredDuringPrepare { generation: u64 },
}

impl UpgradeError {
    /// 失败对应的活跃代数（需要显式 cancel 才能开闸）；`None` = 未建立维护态。
    pub fn generation(&self) -> Option<u64> {
        match self {
            Self::Retired | Self::AlreadyDraining | Self::AlreadyPrepared { .. } => None,
            Self::GateLockTimeout => None,
            Self::TurnsNotDrained { generation, .. }
            | Self::FlushFailed { generation, .. }
            | Self::RetiredDuringPrepare { generation } => Some(*generation),
        }
    }
}

/// 显式回滚（cancel）的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeRelease {
    /// 代数匹配，维护闸已重新打开（新 turn 可以登记）。
    Released,
    /// 代数不匹配 / 没有活跃准备：幂等空操作，**没有**开闸。
    NoMatch,
}

/// 显式回滚失败：这一代正在退场/停机，绝不能重开闸门（否则就是复活）。
#[derive(Debug, Clone, thiserror::Error)]
pub enum UpgradeCancelError {
    #[error(
        "persona retired or shutdown tokens already cancelled: refusing to reopen the \
         maintenance gate (would resurrect a retiring generation)"
    )]
    Retiring,
}

// ── 模块内状态：挂在 groups::TurnState 上 ──────────────────────────────
//
// 为什么需要它（而不是几个原子变量）：
// - prepare 要跨 await（等 turn 排空 / 写盘），cancel 必须和它**互斥**，
//   否则会出现"cancel 清掉状态 → 旧 prepare 又把它写成 Prepared"的幽灵；
// - 每次准备有代数：迟到的 cancel（上一代）绝不能打开新一代的闸；
// - 状态只有在本锁内才能从 None → Some → None，保证 prepare/cancel 的
//   临界区是同一个。

/// 每个 Agent 一份的可逆升级闸（由 [`super::groups::TurnState`] 持有）。
pub(crate) struct UpgradeGate {
    /// 串行化 prepare / cancel 的临界区（异步：prepare 跨 await）。
    critical: tokio::sync::Mutex<()>,
    /// 活跃准备 + 代数（同步锁，只在极短临界区内持有，绝不跨 await）。
    state: std::sync::Mutex<GateState>,
}

#[derive(Debug, Default)]
struct GateState {
    /// 代数分配器（0 = 从未准备过）。
    generation: u64,
    /// 当前活跃准备；`None` = 没有维护态。
    active: Option<ActivePrep>,
}

#[derive(Debug, Clone, Copy)]
struct ActivePrep {
    generation: u64,
    phase: PrepPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrepPhase {
    /// 闸已关，正在等在飞 turn 排空。
    Gating,
    /// turn 已归零，正在/已经写 checked flush。
    Drained,
    /// flush 已确认，写者状态已判定。
    Flushed,
    /// 准备成功，维护态生效。
    Prepared,
    /// 准备失败/被退场打断，维护态保留到显式 cancel。
    Failed,
}

impl UpgradeGate {
    pub(crate) fn new() -> Self {
        Self {
            critical: tokio::sync::Mutex::new(()),
            state: std::sync::Mutex::new(GateState::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Agent {
    /// **每个 Agent 的可逆升级准备**（见模块文档）。
    ///
    /// 语义（成功时）：
    /// 1. 关闭维护闸（`turns.draining = true`）：新 turn 被拒绝；
    /// 2. **不** retire、**不**取消任何 token（在飞 turn 自己跑完）；
    /// 3. 真等在飞 turn 归零（watch 计数）；
    /// 4. `save_now_checked()` 做一次带结论的落盘；
    /// 5. 判定后台写者：当前只有一次性取消令牌 → 明确
    ///    [`WriterPauseStatus::Unsupported`]（`writers_stopped = false`）。
    ///
    /// 失败时（`Err`）：
    /// - 未建立维护态的错误（`Retired` / `AlreadyDraining` /
    ///   `AlreadyPrepared` / `GateLockTimeout`）不改变任何状态；
    /// - 已建立维护态的错误（`generation()` 为 `Some`）**保留维护态**，
    ///   必须显式 [`Self::cancel_upgrade`] 才恢复接单。
    ///
    /// 失败后要重试：先 `cancel_upgrade(err.generation().unwrap())`，再重新
    /// 调用本方法——`AlreadyPrepared` 表示上一代维护态还在，不会静默覆盖。
    ///
    /// `timeout` 约束整个准备（抢锁 + 排空 + flush）。`tx` 是建议性阶段
    /// 通道：接收端丢弃/缓冲满都不会让准备失败。
    pub async fn prepare_upgrade(
        &self,
        tx: UpgradeProgressSender,
        timeout: Duration,
    ) -> Result<UpgradePrepared, UpgradeError> {
        let started = std::time::Instant::now();
        // 快速失败（不碰任何状态）。
        if self.upgrade_externally_stopping() {
            return Err(UpgradeError::Retired);
        }
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));

        // 1) 进入 prepare/cancel 临界区（超时也明确报告，不静默排队）。
        let _critical =
            match tokio::time::timeout_at(deadline, self.turns.upgrade.critical.lock()).await {
                Ok(guard) => guard,
                Err(_) => return Err(UpgradeError::GateLockTimeout),
            };

        // 锁内复查：排队期间可能已经退场/停机。
        if self.upgrade_externally_stopping() {
            return Err(UpgradeError::Retired);
        }
        // 已有活跃准备（含失败后保留的维护态）：不覆盖、不并发。
        let generation = {
            let mut state = self.turns.upgrade.state();
            if let Some(active) = state.active {
                return Err(UpgradeError::AlreadyPrepared {
                    active: active.generation,
                });
            }
            state.generation = state.generation.wrapping_add(1).max(1);
            state.generation
        };

        // 2) 关闸：复用既有的 `draining`（所有新 turn 入口都检查它）。
        //    swap 返回 true = 别人（shutdown/retire）已经在排空：不接管。
        if self.turns.draining.swap(true, Ordering::AcqRel) {
            return Err(UpgradeError::AlreadyDraining);
        }
        {
            let mut state = self.turns.upgrade.state();
            state.active = Some(ActivePrep {
                generation,
                phase: PrepPhase::Gating,
            });
        }
        notify_phase(&tx, UpgradePhase::GateClosed);

        // 3) 等在飞 turn 归零。绝不 cancel 任何 turn 令牌——它们自己跑完。
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match self.wait_inbound_turns_idle(remaining).await {
            Ok(()) => {}
            Err(left) => {
                self.mark_upgrade_phase(generation, PrepPhase::Failed);
                let error = UpgradeError::TurnsNotDrained {
                    generation,
                    remaining: left,
                    timeout,
                };
                notify_phase(
                    &tx,
                    UpgradePhase::Failed {
                        generation: Some(generation),
                        reason: error.to_string(),
                    },
                );
                tracing::warn!(
                    team = ?self.team_id(),
                    generation,
                    remaining = left,
                    "upgrade preparation: in-flight turns did not drain; maintenance kept"
                );
                return Err(error);
            }
        }
        // 排空期间退场/停机：维护态保留，但准备不能算成功。
        if self.upgrade_externally_stopping() {
            self.mark_upgrade_phase(generation, PrepPhase::Failed);
            let error = UpgradeError::RetiredDuringPrepare { generation };
            notify_phase(
                &tx,
                UpgradePhase::Failed {
                    generation: Some(generation),
                    reason: error.to_string(),
                },
            );
            return Err(error);
        }
        self.mark_upgrade_phase(generation, PrepPhase::Drained);
        notify_phase(&tx, UpgradePhase::TurnsDrained { remaining: 0 });

        // 4) checked flush：只认**本次尝试**的结论。
        let memory_only = self.trunk.persist_path().is_none();
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let flush_result = if remaining.is_zero() {
            Err("timeout budget exhausted before checked flush".to_string())
        } else {
            match tokio::time::timeout(remaining, self.trunk.save_now_checked()).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err("checked flush timed out".to_string()),
            }
        };
        if let Err(error) = flush_result {
            self.mark_upgrade_phase(generation, PrepPhase::Failed);
            let error = UpgradeError::FlushFailed { generation, error };
            notify_phase(
                &tx,
                UpgradePhase::Failed {
                    generation: Some(generation),
                    reason: error.to_string(),
                },
            );
            tracing::warn!(
                team = ?self.team_id(),
                generation,
                %error,
                "upgrade preparation: checked flush failed; maintenance kept"
            );
            return Err(error);
        }
        // flush 期间退场：写已经发生，但协调器必须看到失败（不能报 prepared）。
        if self.upgrade_externally_stopping() {
            self.mark_upgrade_phase(generation, PrepPhase::Failed);
            let error = UpgradeError::RetiredDuringPrepare { generation };
            notify_phase(
                &tx,
                UpgradePhase::Failed {
                    generation: Some(generation),
                    reason: error.to_string(),
                },
            );
            return Err(error);
        }
        self.mark_upgrade_phase(generation, PrepPhase::Flushed);
        notify_phase(&tx, UpgradePhase::Flushed);

        // 5) 后台写者：只有一次性取消令牌 → 明确 Unsupported，绝不假 ACK。
        let writers = self.upgrade_background_writers();
        if let WriterPauseStatus::Unsupported {
            registered_tasks,
            periodic_save_started,
            ..
        } = &writers
        {
            notify_phase(
                &tx,
                UpgradePhase::WritersUnsupported {
                    registered_tasks: *registered_tasks,
                    periodic_save_started: *periodic_save_started,
                },
            );
        }
        self.mark_upgrade_phase(generation, PrepPhase::Prepared);
        notify_phase(&tx, UpgradePhase::Prepared { generation });

        tracing::info!(
            team = ?self.team_id(),
            generation,
            elapsed_ms = started.elapsed().as_millis() as u64,
            writers_stopped = writers.writers_stopped(),
            memory_only,
            "upgrade preparation ready: new turns gated, in-flight turns drained, checked flush done"
        );

        Ok(UpgradePrepared {
            team_id: self.team_id(),
            generation,
            elapsed: started.elapsed(),
            in_flight_turns: 0,
            flush: UpgradeFlushOutcome::CheckedOk { memory_only },
            writers,
            maintenance_held: true,
        })
    }

    /// [`Self::prepare_upgrade`] 的无进度版本（丢弃接收端）。
    pub async fn prepare_upgrade_quiet(
        &self,
        timeout: Duration,
    ) -> Result<UpgradePrepared, UpgradeError> {
        let (tx, _rx) = mpsc::channel(1);
        self.prepare_upgrade(tx, timeout).await
    }

    /// **显式回滚**一次升级准备：只有代数匹配且这一代未退场/未停机时，才
    /// 重新打开维护闸（`draining = false`）。
    ///
    /// - `Released`：确实开闸了（新 turn 可以登记）。
    /// - `NoMatch`：代数不匹配 / 没有活跃准备（幂等空操作，不开闸）。
    /// - `Err(Retiring)`：这一代正在退场/停机（`retired` 或 token 已取消）：
    ///   **拒绝开闸**，避免把 retiring 的一代复活。
    ///
    /// 与 `prepare_upgrade` 共用同一把临界区锁：迟到的 cancel 不可能清掉
    /// 新一代的闸，退场后的 cancel 也不会把状态改回去。
    pub async fn cancel_upgrade(
        &self,
        generation: u64,
    ) -> Result<UpgradeRelease, UpgradeCancelError> {
        // 快路径：退场/停机中绝不重开。
        if self.upgrade_externally_stopping() {
            return Err(UpgradeCancelError::Retiring);
        }
        let _critical = self.turns.upgrade.critical.lock().await;
        if self.upgrade_externally_stopping() {
            return Err(UpgradeCancelError::Retiring);
        }
        let active = {
            let mut state = self.turns.upgrade.state();
            match state.active {
                Some(active) if active.generation == generation => {
                    state.active = None;
                    Some(active)
                }
                _ => None,
            }
        };
        let Some(active) = active else {
            return Ok(UpgradeRelease::NoMatch);
        };
        // 最终探测：退场/停机在锁内也可能刚好发生——恢复状态并拒绝开闸。
        if self.upgrade_externally_stopping() {
            self.turns.upgrade.state().active = Some(active);
            return Err(UpgradeCancelError::Retiring);
        }
        self.turns.draining.store(false, Ordering::Release);
        // 开闸后再探一次：若退场恰好在"探测 → 写 false"之间开始（该窗口
        // 无法靠本模块的锁关闭，见模块文档），立刻把闸恢复。
        if self.upgrade_externally_stopping() {
            self.turns.draining.store(true, Ordering::Release);
            return Err(UpgradeCancelError::Retiring);
        }
        tracing::info!(
            team = ?self.team_id(),
            generation,
            "upgrade preparation rolled back: maintenance gate reopened"
        );
        Ok(UpgradeRelease::Released)
    }

    /// 当前活跃准备的代数（`None` = 没有维护态）。
    pub fn upgrade_maintenance_generation(&self) -> Option<u64> {
        self.turns
            .upgrade
            .state()
            .active
            .map(|active| active.generation)
    }

    /// 当前是否处于升级维护态（含失败后保留的维护态）。
    pub fn upgrade_maintenance_held(&self) -> bool {
        self.turns.upgrade.state().active.is_some()
    }

    /// 维护态是否已完成 prepare（`false` = 还在准备、已失败但保留维护，
    /// 或这一代已进入退场/停机——后者即使代数还在也不再算"可继续升级"）。
    pub fn upgrade_maintenance_ready(&self) -> bool {
        if self.upgrade_externally_stopping() {
            return false;
        }
        self.turns
            .upgrade
            .state()
            .active
            .is_some_and(|active| active.phase == PrepPhase::Prepared)
    }

    /// 外部停机/退场的判定：`retired` 或任一"这一代"的取消令牌已取消。
    ///
    /// 两者都不可逆——本模块只用它来**拒绝**重开闸门，从不去 `cancel()`。
    fn upgrade_externally_stopping(&self) -> bool {
        self.is_retired()
            || self.turns.cancel.is_cancelled()
            || self.background.cancel.is_cancelled()
    }

    /// 判定后台写者状态：当前只有"在册任务数 + 周期保存是否已启动"这两条
    /// 现场信息；暂停能力明确为 Unsupported。
    fn upgrade_background_writers(&self) -> WriterPauseStatus {
        let registered_tasks = self
            .background
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let periodic_save_started = self.background.save_started.load(Ordering::Acquire);
        WriterPauseStatus::Unsupported {
            reason: WRITERS_PAUSE_UNSUPPORTED_REASON,
            registered_tasks,
            periodic_save_started,
        }
    }

    /// 只改本代代数对应的阶段（迟到的旧准备不会改新一代的状态）。
    fn mark_upgrade_phase(&self, generation: u64, phase: PrepPhase) {
        let mut state = self.turns.upgrade.state();
        if let Some(active) = state.active.as_mut() {
            if active.generation == generation {
                active.phase = phase;
            }
        }
    }
}

/// 建议性阶段通知：`try_send`，接收端缺失/已满/已关闭都不影响准备结论。
fn notify_phase(tx: &UpgradeProgressSender, phase: UpgradePhase) {
    let _ = tx.try_send(phase);
}

#[cfg(test)]
mod tests {
    //! 复用 `agent::tests` 的 `MockProvider`（同一 crate 的测试桩），不碰真实
    //! 数据/服务：所有 Agent 都是进程内 stub，flush 目标是 memory-only 或
    //! `std::env::temp_dir()` 下的临时坏档（用完即删）。

    use super::*;
    use crate::config::AgentConfig;
    use crate::skill::SkillRegistry;
    use crate::tool::ToolRegistry;
    use echo_defs::module_instance::InstanceRegistry;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    fn stub_agent() -> Arc<Agent> {
        let provider = Arc::new(crate::agent::tests::MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        Arc::new(Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            ToolRegistry::new(),
            Arc::new(InstanceRegistry::new()),
        ))
    }

    fn temp_json(name: &str) -> std::path::PathBuf {
        let unique = format!(
            "echo-upgrade-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
            name
        );
        std::env::temp_dir().join(format!("{unique}.json"))
    }

    /// 成功准备：挡新 turn、不 retire、不 cancel 任何 token、checked flush
    /// 成功、writers 明确 Unsupported（不谎报 stopped）。
    #[tokio::test]
    async fn prepare_blocks_new_turns_without_retiring_or_cancelling_tokens() {
        let agent = stub_agent();
        let (tx, mut rx) = mpsc::channel(16);
        let prepared = agent
            .prepare_upgrade(tx, Duration::from_secs(2))
            .await
            .expect("memory-only agent should prepare");

        assert_eq!(prepared.in_flight_turns, 0);
        assert!(prepared.maintenance_held);
        assert!(!prepared.fully_prepared(), "写者没停，不能算完整准备");
        assert_eq!(
            prepared.flush,
            UpgradeFlushOutcome::CheckedOk { memory_only: true }
        );
        assert!(
            !prepared.writers_stopped(),
            "当前实现绝不能谎报 writers_stopped"
        );
        match &prepared.writers {
            WriterPauseStatus::Unsupported { reason, .. } => {
                assert_eq!(*reason, WRITERS_PAUSE_UNSUPPORTED_REASON);
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }

        // 不 retire、不取消任何不可重用 token。
        assert!(!agent.is_retired());
        assert!(agent.turns.draining.load(Ordering::Acquire));
        assert!(!agent.turns.cancel.is_cancelled());
        assert!(!agent.background.cancel.is_cancelled());

        // 新 turn 被拒（既有入口检查 draining）。
        assert!(agent.try_register_inbound_turn("s1", 1).is_none());

        // 建议性阶段通知至少包含 GateClosed / Prepared（尽力而为）。
        let mut phases = Vec::new();
        while let Ok(phase) = rx.try_recv() {
            phases.push(phase);
        }
        assert!(phases
            .iter()
            .any(|phase| matches!(phase, UpgradePhase::GateClosed)));
        assert!(phases
            .iter()
            .any(|phase| matches!(phase, UpgradePhase::Prepared { .. })));

        // 显式回滚后恢复接单。
        assert_eq!(
            agent.cancel_upgrade(prepared.generation).await.unwrap(),
            UpgradeRelease::Released
        );
        assert!(!agent.turns.draining.load(Ordering::Acquire));
        assert!(!agent.is_retired());
        assert!(agent.try_register_inbound_turn("s2", 2).is_some());
    }

    /// prepare 真的等现有 turn 跑完（watch 计数），且不取消它的令牌。
    #[tokio::test]
    async fn prepare_waits_for_in_flight_turn_without_cancelling_it() {
        let agent = stub_agent();
        let registration = agent
            .try_register_inbound_turn("s1", 1)
            .expect("turn should register before maintenance");

        assert_eq!(
            *agent.turns.inbound_turn_count.borrow(),
            1,
            "fixture: one in-flight turn counted"
        );
        let turn_id = registration.id.clone();
        let turn_cancel = registration.cancel.clone();
        let worker_agent = Arc::clone(&agent);
        let worker = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let cancelled = worker_agent
                .turns
                .active_inbound_turns
                .get(&turn_id)
                .map(|turn| turn.cancel.is_cancelled())
                .unwrap_or(false);
            assert!(!cancelled, "prepare 不得取消在飞 turn 的令牌");
            worker_agent.finish_inbound_turn(&turn_id);
        });

        let started = std::time::Instant::now();
        let prepared = agent
            .prepare_upgrade_quiet(Duration::from_secs(2))
            .await
            .expect("prepare should wait for the turn and succeed");
        assert!(
            started.elapsed() >= Duration::from_millis(60),
            "prepare 必须在 turn 结束之后才返回（实际 {:?}）",
            started.elapsed()
        );
        assert_eq!(prepared.in_flight_turns, 0);
        assert!(!turn_cancel.is_cancelled());
        assert!(!agent.is_retired());
        worker.await.unwrap();
        assert_eq!(
            agent.cancel_upgrade(prepared.generation).await.unwrap(),
            UpgradeRelease::Released
        );
    }

    /// 排空超时：保留维护态（不静默恢复），只有显式 cancel 才开闸；
    /// 第二次 prepare 被明确拒绝（不会覆盖失败后的维护态）。
    #[tokio::test]
    async fn timed_out_prepare_keeps_maintenance_until_explicit_cancel() {
        let agent = stub_agent();
        let registration = agent
            .try_register_inbound_turn("s1", 1)
            .expect("turn should register");

        let error = agent
            .prepare_upgrade_quiet(Duration::from_millis(60))
            .await
            .expect_err("turn never finishes: prepare must time out");
        let generation = error.generation().expect("maintenance generation");
        assert!(matches!(
            error,
            UpgradeError::TurnsNotDrained { remaining: 1, .. }
        ));

        // fail preserves maintenance：闸仍关，token 未取消，未退场。
        assert!(agent.turns.draining.load(Ordering::Acquire));
        assert!(!registration.cancel.is_cancelled());
        assert!(!agent.is_retired());
        assert_eq!(
            agent.upgrade_maintenance_generation(),
            Some(generation),
            "失败后维护态必须保留并可见"
        );
        assert!(agent.try_register_inbound_turn("s2", 2).is_none());

        // 没显式回滚前，不允许新的 prepare 覆盖。
        let second = agent
            .prepare_upgrade_quiet(Duration::from_secs(1))
            .await
            .expect_err("must not silently replace the kept maintenance");
        assert!(matches!(
            second,
            UpgradeError::AlreadyPrepared { active } if active == generation
        ));

        // 显式回滚：开闸。
        assert_eq!(
            agent.cancel_upgrade(generation).await.unwrap(),
            UpgradeRelease::Released
        );
        assert!(!agent.turns.draining.load(Ordering::Acquire));
        agent.finish_inbound_turn(&registration.id);
    }

    /// 迟到的 cancel（上一代）不能打开新一代的闸。
    #[tokio::test]
    async fn stale_cancel_does_not_open_a_newer_preparation() {
        let agent = stub_agent();
        let first = agent
            .prepare_upgrade_quiet(Duration::from_secs(1))
            .await
            .expect("first prepare");
        assert_eq!(
            agent.cancel_upgrade(first.generation).await.unwrap(),
            UpgradeRelease::Released
        );

        let second = agent
            .prepare_upgrade_quiet(Duration::from_secs(1))
            .await
            .expect("second prepare");
        assert_ne!(first.generation, second.generation);

        assert_eq!(
            agent.cancel_upgrade(first.generation).await.unwrap(),
            UpgradeRelease::NoMatch,
            "旧代数必须是无操作"
        );
        assert!(
            agent.turns.draining.load(Ordering::Acquire),
            "旧 cancel 绝不能打开新一代的维护闸"
        );
        assert_eq!(
            agent.cancel_upgrade(second.generation).await.unwrap(),
            UpgradeRelease::Released
        );
    }

    /// 退场后 cancel 必须拒绝开闸（不能复活这一代）。
    #[tokio::test]
    async fn retire_wins_over_cancel() {
        let agent = stub_agent();
        let prepared = agent
            .prepare_upgrade_quiet(Duration::from_secs(1))
            .await
            .expect("prepare");
        agent.begin_retire();
        assert!(agent.is_retired());
        assert!(agent.turns.cancel.is_cancelled());
        assert!(agent.background.cancel.is_cancelled());

        assert!(matches!(
            agent.cancel_upgrade(prepared.generation).await,
            Err(UpgradeCancelError::Retiring)
        ));
        assert!(
            agent.turns.draining.load(Ordering::Acquire),
            "退场中绝不能被 cancel 开闸"
        );
        assert!(!agent.upgrade_maintenance_ready());
    }

    /// 别人（shutdown/retire）已经在排空时，prepare 不接管、不改状态。
    #[tokio::test]
    async fn prepare_refuses_to_hijack_existing_drain() {
        let agent = stub_agent();
        agent.turns.draining.store(true, Ordering::Release);
        let error = agent
            .prepare_upgrade_quiet(Duration::from_secs(1))
            .await
            .expect_err("must not take over an external drain");
        assert!(matches!(error, UpgradeError::AlreadyDraining));
        assert!(
            agent.turns.draining.load(Ordering::Acquire),
            "别人的排空闸必须保持"
        );
        assert!(!agent.upgrade_maintenance_held());
        assert!(!agent.is_retired());
        agent.turns.draining.store(false, Ordering::Release);
    }

    /// checked flush 失败（坏档 → LoadFailed）：保留维护态直到显式 cancel。
    #[tokio::test]
    async fn checked_flush_failure_keeps_maintenance_until_cancel() {
        let agent = stub_agent();
        let path = temp_json("flush-failure");
        std::fs::write(&path, b"{ this is not valid json").unwrap();
        agent.set_session_persist_path(&path);
        // 加载坏档 → LoadStage::Failed（负缓存）；之后 checked 落盘必须拒绝。
        assert_eq!(agent.load_sessions().await, 0);

        let error = agent
            .prepare_upgrade_quiet(Duration::from_secs(2))
            .await
            .expect_err("corrupt snapshot must fail the checked flush");
        let generation = error.generation().expect("maintenance generation");
        assert!(matches!(error, UpgradeError::FlushFailed { .. }));

        assert!(agent.turns.draining.load(Ordering::Acquire));
        assert!(!agent.is_retired());
        assert!(agent.try_register_inbound_turn("s1", 1).is_none());

        assert_eq!(
            agent.cancel_upgrade(generation).await.unwrap(),
            UpgradeRelease::Released
        );
        assert!(!agent.turns.draining.load(Ordering::Acquire));
        let _ = std::fs::remove_file(&path);
    }

    /// 并发 prepare 被临界区串行化：只有一个能建立维护态，另一个明确
    /// `AlreadyPrepared`（不会出现两代同时 active）。
    #[tokio::test]
    async fn concurrent_prepare_is_serialized() {
        let agent = stub_agent();
        let first = agent.prepare_upgrade_quiet(Duration::from_secs(1));
        let second = agent.prepare_upgrade_quiet(Duration::from_secs(1));
        let (first, second) = tokio::join!(first, second);

        let (prepared, refused) = match (first, second) {
            (Ok(prepared), Err(refused)) => (prepared, refused),
            (Err(refused), Ok(prepared)) => (prepared, refused),
            (first, second) => panic!("exactly one prepare must win: {first:?} / {second:?}"),
        };
        assert!(matches!(
            refused,
            UpgradeError::AlreadyPrepared { active } if active == prepared.generation
        ));
        assert_eq!(
            agent.upgrade_maintenance_generation(),
            Some(prepared.generation)
        );
        assert_eq!(
            agent.cancel_upgrade(prepared.generation).await.unwrap(),
            UpgradeRelease::Released
        );
    }

    /// 抢临界区超时：明确报 GateLockTimeout，且不改变任何闸门状态。
    #[tokio::test]
    async fn prepare_times_out_waiting_for_gate_lock() {
        let agent = stub_agent();
        let _held = agent.turns.upgrade.critical.lock().await;
        let error = agent
            .prepare_upgrade_quiet(Duration::from_millis(30))
            .await
            .expect_err("critical section is held: prepare must time out");
        assert!(matches!(error, UpgradeError::GateLockTimeout));
        assert!(!agent.turns.draining.load(Ordering::Acquire));
        assert!(!agent.upgrade_maintenance_held());
    }

    /// 幂等：没有活跃准备时 cancel 是 NoMatch，且不会凭空关/开闸。
    #[tokio::test]
    async fn cancel_without_active_preparation_is_noop() {
        let agent = stub_agent();
        assert_eq!(
            agent.cancel_upgrade(42).await.unwrap(),
            UpgradeRelease::NoMatch
        );
        assert!(!agent.turns.draining.load(Ordering::Acquire));
        assert!(!agent.upgrade_maintenance_held());
    }
}
