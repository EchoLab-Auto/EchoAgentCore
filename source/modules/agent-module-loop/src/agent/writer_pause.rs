//! 可逆升级：后台写者的**暂停 / 恢复**原语。
//!
//! ## 这是什么
//!
//! [`Agent::pause_upgrade_writers`] 拿住 `TurnState::upgrade_writers` 的
//! **写**锁并返回 RAII guard；在 guard 存活期间，所有按协议取**读**锁的后台
//! 写者都会停在写操作之前。drop guard 即恢复——没有取消任何
//! `CancellationToken`，锁本身也不是一次性令牌（可反复 pause / release）。
//!
//! ## 写者协议（本锁不覆盖的范围也写在这里）
//!
//! 目前只有两类后台写者按本协议取读锁，并持有到**整轮操作完成**：
//!
//! - 周期会话保存（[`Agent::start_session_save_task`]）：从 `maybe_save`
//!   开始，到本轮时间线探针 / 事件发射结束；
//! - 模块热重载 watcher（`Agent::run_module_reload_task`）：从本轮目录扫描
//!   开始，到 unmount / register / mount / emit 全部结束；初次读配置也在读锁内。
//!
//! **不覆盖**：turn commit、管理面写盘、退场最终保存等由其它全局门控负责。
//! 因此拿到本锁只能声称"这两类后台写者不在写"，**不能**声称"进程内所有
//! 写者都已停止"。
//!
//! 本锁是 **per-Agent** 的：模块宿主可能进程共享，多 persona 的 watcher 仍需
//! 全局升级协调逐个 pause；本原语不宣称跨 Agent / 全进程停止。
//!
//! 启动与 pause 的竞态：`start_background_tasks` 可能迟到拉起任务，本原语
//! 不靠启动 flag 判定。任务即使已在册，第一次实际写也会阻塞在读锁上；
//! 反过来，"任务在册"也不能证明它在写。
//!
//! ## 为什么用读写锁而不是取消令牌
//!
//! `CancellationToken` 是一次性的：取消后这一代后台任务永远退出，升级失败
//! 就无法原样回滚。读写锁的语义正好相反——写者拿读锁、升级拿写锁；写锁
//! drop 后新读者继续进入，任务本身从未被取消。超时（[`Option::None`]）只是
//! "这次没停下来"，不是"把写者杀了"，调用方可以稍后重试，或直接报告
//! writers 未停。
//!
//! ## 纪律
//!
//! - 写者只在**实际写操作**期间持读锁，绝不跨空闲 / 下一轮 tick 持有；
//!   否则 pause 永远等不到写锁。
//! - pause 的 guard 必须由需要暂停的调用方持有到升级结束；提前 drop 等于
//!   立刻恢复写者（`#[must_use]` 防误丢）。
//! - 等锁的写者用 `select!` 同时等取消令牌：退场 / shutdown 时能退出，
//!   但已经开始的写操作会跑完（本锁不中途取消写操作）。
//!

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::OwnedRwLockWriteGuard;

use super::Agent;

impl Agent {
    /// 暂停本代的两类后台写者（周期保存 + 模块热重载），返回写锁 guard。
    ///
    /// 成功 = 当前没有写者持有读锁；此后新写者会阻塞在写操作入口，直到
    /// guard drop。失败（`None`）= 在 `timeout` 内仍有写者在飞；**没有**
    /// 取消任何任务或令牌，写者继续正常工作。
    ///
    /// 语义边界见模块文档：本锁不覆盖 turn commit / 管理面写盘 / 最终保存。
    /// 调用方不得把 `Some(_)` 解释为"全进程写者已停"。
    ///
    /// # 参数
    ///
    /// - `timeout`：等写锁的上限。超时返回 `None`；不重试、不排队残留
    ///   （被 drop 的 `write_owned()` future 会离开等待队列）。
    ///
    /// # 返回值
    ///
    /// `Some(guard)` 必须被持有到"允许写者恢复"的那一刻；drop 即恢复。
    #[must_use = "返回的 guard 就是暂停本身；立即 drop 等于没有暂停，写者会马上恢复"]
    pub async fn pause_upgrade_writers(
        &self,
        timeout: Duration,
    ) -> Option<OwnedRwLockWriteGuard<()>> {
        // clone 出 Arc 再 `write_owned()`：guard 不借用 Agent，调用方可以把
        // 它放进升级状态里跨 await 持有。
        let writers = Arc::clone(&self.turns.upgrade_writers);
        match tokio::time::timeout(timeout, writers.write_owned()).await {
            Ok(guard) => Some(guard),
            Err(_) => {
                tracing::debug!(
                    timeout_ms = timeout.as_millis(),
                    "upgrade writer pause timed out; background writers remain active"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! 只测锁语义，不启动真实后台任务、不碰磁盘/服务：
    //! - 有读者持锁时，pause 必须超时（不能假 ACK）；
    //! - drop pause guard 后写者恢复；
    //! - 锁可重复使用，且全程不取消任何一次性 token。

    use super::*;
    use crate::config::AgentConfig;
    use crate::skill::SkillRegistry;
    use crate::tool::ToolRegistry;
    use echo_defs::module_instance::InstanceRegistry;
    use std::sync::atomic::AtomicUsize;

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

    /// 模拟"一轮保存仍在飞"：读者持锁时，写锁必须在超时后返回 None，
    /// 而不是假装暂停成功；读者释放后同一把锁仍可正常 pause。
    #[tokio::test]
    async fn pause_times_out_while_a_reader_is_held_then_succeeds_after_release() {
        let agent = stub_agent();
        let held_reader = agent.turns.upgrade_writers.read().await;

        let started = std::time::Instant::now();
        let paused = agent.pause_upgrade_writers(Duration::from_millis(60)).await;
        assert!(
            paused.is_none(),
            "仍有读者持锁时必须超时返回 None，不能假 ACK"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(30),
            "pause 应真的等满超时窗口，而不是立刻失败"
        );

        drop(held_reader);
        let paused = agent.pause_upgrade_writers(Duration::from_secs(1)).await;
        assert!(
            paused.is_some(),
            "一次超时不能污染/消耗锁：读者释放后应能正常暂停"
        );
    }

    /// drop pause guard = 恢复；随后写者能重新拿读锁，且同一把锁能再次
    /// pause（可逆、可重用，不是一次性令牌）。
    #[tokio::test]
    async fn dropping_pause_guard_resumes_writers_and_lock_is_reusable() {
        let agent = stub_agent();
        let paused = agent
            .pause_upgrade_writers(Duration::from_secs(1))
            .await
            .expect("空闲锁应立刻拿到写锁");

        assert!(
            tokio::time::timeout(
                Duration::from_millis(60),
                agent.turns.upgrade_writers.read()
            )
            .await
            .is_err(),
            "pause guard 持有时，新写者必须被挡在读锁之外"
        );

        drop(paused);

        let resumed =
            tokio::time::timeout(Duration::from_secs(1), agent.turns.upgrade_writers.read())
                .await
                .expect("drop pause guard 后写者应立即恢复");
        drop(resumed);

        let paused_again = agent.pause_upgrade_writers(Duration::from_secs(1)).await;
        assert!(
            paused_again.is_some(),
            "锁必须可重复使用：第二次 pause 仍应成功"
        );
        drop(paused_again);

        // 全程不碰一次性取消令牌 / 退场位——可逆暂停不能把这一代变成不可恢复。
        assert!(!agent.turns.cancel.is_cancelled());
        assert!(!agent.background.cancel.is_cancelled());
        assert!(!agent.is_retired());
    }
}
