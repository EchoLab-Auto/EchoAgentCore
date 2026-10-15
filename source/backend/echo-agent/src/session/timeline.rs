//! 展示时间线：入列 / 快照 / 增量快照 / 就地更新 / 恢复 / 一致性探针。
//!
//! 2026-10 架构评审：自 `session.rs`（4300 行）拆出，纯搬移、行为不变。

use super::*;

impl TrunkStore {
    /// Append one entry to the display timeline, dropping the oldest entries
    /// beyond the cap. Marks the store dirty so the entry is persisted.
    ///
    /// 条目 seq 在锁内赋值并与计数器同步推进（仅在条目真正入列时递增），
    /// 保证 `timeline_snapshot_since` 的按 seq 过滤语义成立。
    pub fn push_timeline(&self, message: crate::event::TimelineMessage) {
        let mut timeline = self.timeline.lock();
        self.push_timeline_locked(&mut timeline, message);
        drop(timeline);
        self.mark_dirty();
    }

    /// 同 [`Self::push_timeline`]（锁已改为同步短临界区后两者行为一致；
    /// 保留 async 签名以兼容既有调用方）——供异步控制路径
    /// （归档 / 压缩的 system 提示行等）使用：这些行必须入列，且要经
    /// 统一路径赋 seq / 计数 / 执行上限裁剪。
    ///
    /// 2026-10 审计修复：此前归档 / 压缩直接 `timeline.lock().await.push(…)`，
    /// 绕过 seq 与 pushed 计数、也不裁剪——(a) 提示行 seq=0，增量同步
    /// （since_seq）永远看不到它；(b) 探针对账恒等式在下一采样点误报
    /// 失衡（差一条未计数的插入）。
    pub async fn push_timeline_blocking(&self, message: crate::event::TimelineMessage) {
        let mut timeline = self.timeline.lock();
        self.push_timeline_locked(&mut timeline, message);
        drop(timeline);
        self.mark_dirty();
    }

    /// 入列共享实现（调用方持锁）：赋 seq、推进 pushed、执行 cap 裁剪。
    pub(super) fn push_timeline_locked(
        &self,
        timeline: &mut Vec<crate::event::TimelineMessage>,
        mut message: crate::event::TimelineMessage,
    ) {
        message.seq = self
            .timeline_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        timeline.push(message);
        self.timeline_pushed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if timeline.len() > TRUNK_TIMELINE_MAX {
            let excess = timeline.len() - TRUNK_TIMELINE_MAX;
            timeline.drain(..excess);
            self.timeline_drained
                .fetch_add(excess as u64, std::sync::atomic::Ordering::Relaxed);
            if excess > 1 {
                // 正常路径单次 push 只滚出一条；一次滚出多条意味着异常批量
                // 插入（探针：定位"时间线头部无声前移"事故，2026-10）。
                tracing::warn!(
                    excess,
                    "timeline drained more than one entry in a single push"
                );
            }
        }
    }

    /// 推进 timeline 序号并返回新值（就地更新场景：条目内容变化但位置
    /// 不变，调用方把返回的 seq 写到被更新的条目上，增量同步即可重新
    /// 投递该条目）。
    pub fn bump_timeline_seq(&self) -> u64 {
        self.timeline_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    /// Point-in-time snapshot of the display timeline (oldest first).
    /// 全量快照：同步短锁内 clone，永不返回"空顶替"（历史上空快照曾被
    /// 当"空全量"发给前端而清空本地聊天缓存）。
    pub fn timeline_snapshot(&self) -> Vec<crate::event::TimelineMessage> {
        self.timeline.lock().clone()
    }

    /// 当前 timeline 单调序号（增量同步游标）。
    pub fn timeline_seq(&self) -> u64 {
        self.timeline_seq.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 增量快照：返回 seq > since_seq 的条目 + 最新 seq。
    /// `since_seq == 0` 时为全量快照。注意：timeline 本身有 1024 条目上限，
    /// 若增量窗口内的条目已被滚出（since_seq 落后太多），返回 None 表示
    /// 无法增量对齐，调用方应回退全量。`since_seq > current`（前端游标来自
    /// 本 core 重启之前）同样返回 None——序号在重启后重新从持久化条目
    /// 重建，无法与旧游标对齐。
    pub fn timeline_snapshot_since(
        &self,
        since_seq: u64,
    ) -> Option<(Vec<crate::event::TimelineMessage>, u64)> {
        let current = self.timeline_seq();
        if since_seq > current {
            // 游标超前于当前序号（如对端缓存自 core 重启前）：无法对齐。
            return None;
        }
        if since_seq == current {
            return Some((Vec::new(), current));
        }
        let timeline = self.timeline.lock();
        // 缺口检测：淘汰只发生在头部；若现存最老条目的 seq 已越过
        // since_seq + 1，说明窗口内有条目被滚出，增量不再完整。
        // （seq == 0 的旧版/重启前条目不参与缺口判断，直接按全量处理。）
        if since_seq > 0 {
            match timeline.first() {
                Some(oldest) if oldest.seq > 0 && oldest.seq > since_seq + 1 => return None,
                Some(oldest) if oldest.seq == 0 => return None,
                None => return None,
                _ => {}
            }
        }
        let messages: Vec<_> = timeline
            .iter()
            .filter(|m| m.seq > since_seq)
            .cloned()
            .collect();
        Some((messages, current))
    }

    /// Mutable access to the display timeline for in-place updates (e.g.
    /// attaching a tool result to its running entry).
    ///
    /// 同步短锁：调用方不得跨 `.await` 持有返回的 guard（编译期由
    /// parking_lot guard 的 `!Send` 保证不会被带进 spawn 的 future）。
    pub fn timeline_mut(&self) -> parking_lot::MutexGuard<'_, Vec<crate::event::TimelineMessage>> {
        self.timeline.lock()
    }

    /// Restore the persisted display timeline and rehydrate the seq counter
    /// from the highest entry seq. 旧版文件的条目没有 seq（反序列化为 0），
    /// 计数器保持为 0，新条目从 1 开始——此时增量同步的缺口检测会以
    /// 最老条目 seq == 0 为依据回退全量，语义仍然正确。
    ///
    /// 顺带清理僵尸 running 工具条目：进程被杀（如自更新重启）时
    /// run_tool 的 ToolResult 永远不会到达，条目 output 永远为 null，
    /// 前端会一直显示"运行中"。恢复时把这类条目标注为中断并分配新
    /// seq（增量同步会把完成态重新投递给已连接的前端）。
    pub(super) fn restore_timeline(&self, timeline: Vec<crate::event::TimelineMessage>) {
        let mut max_seq = timeline.iter().map(|entry| entry.seq).max().unwrap_or(0);
        let mut timeline = timeline;
        let mut interrupted = 0usize;
        for entry in timeline.iter_mut() {
            if entry.kind != "tool" {
                continue;
            }
            let Some(tool) = entry.tool.as_mut() else {
                continue;
            };
            if tool.output.is_none() {
                tool.output = Some("[已中断] Core 服务重启导致本次调用未返回，可重试".into());
                tool.failed = true;
                max_seq += 1;
                entry.seq = max_seq;
                interrupted += 1;
            }
        }
        if interrupted > 0 {
            tracing::info!(
                interrupted,
                "dangling running tool entries marked interrupted on restore"
            );
            self.mark_dirty();
        }
        let entries = timeline.len();
        let head_seq = timeline.first().map(|entry| entry.seq).unwrap_or(0);
        let tail_seq = timeline.last().map(|entry| entry.seq).unwrap_or(0);
        *self.timeline.lock() = timeline;
        self.timeline_seq
            .store(max_seq, std::sync::atomic::Ordering::Relaxed);
        // 整表替换：置位探针重基线标志（跨表差值对账对替换无意义）。
        self.timeline_rebaseline
            .store(true, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(
            entries,
            head_seq,
            tail_seq,
            max_seq,
            interrupted,
            "display timeline restored from persisted file"
        );
    }

    /// 时间线探针（2026-10："面板看不到历史"事故定位用）。
    ///
    /// 背景：显示时间线头部曾两次在**没有任何**清理/归档/压缩/重启日志的
    /// 情况下被整体前移（10-01、10-13 各一次，近千条无声消失），事后无法
    /// 指认元凶。本探针做**对账式监控**：周期采样 条目数 + 头/尾 seq 与
    /// 时间 + 各会话分布 + 三个单调写计数（push / cap-drain / 其余删除），
    /// 并对相邻采样验证恒等式
    ///
    /// ```text
    /// count(now) == count(last) + Δpushed − Δdrained − Δremoved
    /// ```
    ///
    /// 失衡即当场 warn（附全部差值）——把事故窗口压到 ≤ 采样周期；配合
    /// clear / restore / drain 各写路径自身的日志即可指认来源，任何绕过
    /// 这些写路径的整表替换（即使来自当前代码之外的机制）都会触发失衡。
    pub fn probe_timeline(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        // 计数器前后各读一次，一致才算样本（避免与并发写交错造成假失衡；
        // 不一致就跳过本轮，下轮再采）。
        let pushed = self.timeline_pushed.load(Relaxed);
        let drained = self.timeline_drained.load(Relaxed);
        let removed = self.timeline_removed.load(Relaxed);
        let sampled = {
            let timeline = self.timeline.lock();
            {
                let count = timeline.len();
                let head_seq = timeline.first().map(|m| m.seq).unwrap_or(0);
                let head_time = timeline.first().map(|m| m.time).unwrap_or(0);
                let tail_seq = timeline.last().map(|m| m.seq).unwrap_or(0);
                let sessions = timeline_sessions_summary(&timeline);
                (count, head_seq, head_time, tail_seq, sessions)
            }
        };
        let (count, head_seq, head_time, tail_seq, sessions) = sampled;
        if self.timeline_pushed.load(Relaxed) != pushed
            || self.timeline_drained.load(Relaxed) != drained
            || self.timeline_removed.load(Relaxed) != removed
        {
            return;
        }
        let rebaseline = self.timeline_rebaseline.swap(false, Relaxed);
        let now = TimelineProbeSample {
            count,
            pushed,
            drained,
            removed,
            head_seq,
        };
        let mut state = self
            .timeline_probe_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(last) = *state else {
            tracing::info!(
                count,
                head_seq,
                head_time,
                tail_seq,
                sessions = %sessions,
                "timeline probe: baseline established"
            );
            *state = Some(now);
            return;
        };
        if rebaseline {
            tracing::info!(
                count,
                head_seq,
                head_time,
                tail_seq,
                "timeline probe: baseline re-established after full replace"
            );
            *state = Some(now);
            return;
        }
        let expected = last.expected_count_after(&now);
        if expected != count as i64 {
            tracing::warn!(
                expected,
                actual = count,
                last_count = last.count,
                pushed_delta = now.pushed.saturating_sub(last.pushed),
                drained_delta = now.drained.saturating_sub(last.drained),
                removed_delta = now.removed.saturating_sub(last.removed),
                head_seq,
                head_time,
                sessions = %sessions,
                "timeline probe: entry count mismatch — timeline changed outside tracked write paths"
            );
        } else if count != last.count || head_seq != last.head_seq {
            tracing::info!(
                count,
                head_seq,
                head_time,
                tail_seq,
                sessions = %sessions,
                "timeline probe: sample"
            );
        }
        *state = Some(now);
    }
}
