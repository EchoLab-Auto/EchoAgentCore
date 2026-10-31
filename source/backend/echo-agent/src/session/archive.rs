//! 清理 / 归档 / 压缩：会话级与全量清理、归档文件轮转、压缩预览与落地。
//!
//! 2026-10 架构评审：自 `session.rs`（4300 行）拆出，纯搬移、行为不变。

use super::*;

impl TrunkStore {
    /// Erase all conversation memory: the durable event log, the in-memory
    /// trunk projection and the display timeline, then persist immediately so
    /// a restart cannot resurrect the cleared history. Identity labels
    /// (provenance metadata) and the session header are kept — they carry no
    /// message content.
    pub async fn clear_history(&self) {
        // 压缩互斥（2026-10 巡检 P1）：compact_history 全程持有 compact_lock
        // （预览→归档→LLM 摘要→replace_all 落地，秒级窗口）。不取锁的话，
        // 清理落在压缩窗口内时，压缩的 replace_all 会把**清理前**的旧事件
        // 重新灌回日志——「清空」被静默复活（清空发生在压缩落地之后才正确）。
        // 与 compact_history 同锁互斥；无嵌套调用（compact 流程不调
        // clear_history），取锁无死锁风险。
        let _compact_guard = self.compact_lock().await;
        let events_cleared = self.event_log.len();
        self.event_log.clear();
        for entry in self.session_histories.iter() {
            entry.value().lock().clear();
        }
        let timeline_cleared = {
            let mut timeline = self.timeline.lock();
            let count = timeline.len();
            timeline.clear();
            count
        };
        self.timeline_removed.fetch_add(
            timeline_cleared as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.mark_dirty();
        self.save_now().await;
        tracing::info!(
            events_cleared,
            timeline_cleared,
            "clear_history: all sessions wiped (events + projections + timeline)"
        );
    }

    /// 清理**单个会话**的记忆：事件日志中该会话的事件、该会话的上下文
    /// 投影、时间线中该会话的条目。
    ///
    /// 2026-10 用户报告修复：面板「清理历史记忆」此前映射到
    /// [`Self::clear_history`]（全量）——同一人格下其他会话（QQ 群/私聊、
    /// 其他 workspace 会话）的记忆被一并抹掉。现在 UI 走本方法按
    /// `session_id` 精确清理；全量版本保留（显式全清的管理语义）。
    ///
    /// - `session == None` 的旧版事件不删（无归属信息，宁保守不误删）；
    /// - 压缩互斥同 [`Self::clear_history`]（清理与压缩落地竞争同样会
    ///   让旧事件被 replace_all 复活）。
    pub async fn clear_session(&self, session_id: &str) {
        let _compact_guard = self.compact_lock().await;
        let removed = self.event_log.retain_excluding_session(session_id);
        // 2026-10-15 用户报障修复（uuapi 端点 400 "field messages is
        // required"）：必须**保留映射条目并原地清空**，不能 `remove`。
        // 身份对象 `Session.history` 持有同一 Arc 的拷贝（`get_or_create`
        // 命中既有身份时原样返回，`register_incoming_branch`/`process_message`
        // 读的也是它）。一旦摘除映射，下一条事件的 `reproject` 会经
        // `history_or_create` 新建第二个 Arc——旧 Arc 与后续投影永久脱节：
        // 该会话此后每个请求只剩系统提示词（严格端点 400 "field messages
        // is required"；宽松端点回复退化成自我介绍式问候），直到重启才自愈。
        // 保留 + 原地 clear 与全量 `clear_history` 语义一致。
        if let Some(history) = self.history_for(session_id) {
            history.lock().clear();
        }
        let (timeline_removed, timeline_remaining) = {
            let mut timeline = self.timeline.lock();
            let before = timeline.len();
            timeline.retain(|entry| entry.session_id != session_id);
            (before - timeline.len(), timeline.len())
        };
        self.timeline_removed.fetch_add(
            timeline_removed as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.mark_dirty();
        self.save_now().await;
        tracing::info!(
            session = %session_id,
            removed_events = removed,
            timeline_removed,
            timeline_remaining,
            "session memory cleared (scoped)"
        );
    }

    /// 归档保留上限（archives/ 下本 stem 的归档文件数，最旧先删；
    /// 2026-10 议题：此前归档只增不减，一年可达数百 MB）。
    pub const DEFAULT_ARCHIVE_MAX_FILES: usize = 20;

    /// 归档文件名是否属于本 stem：`{stem}-{8位日期}-{6位时间}[-{reason}].json`。
    ///
    /// 2026-10 审计：前缀匹配必须**锚定时间戳段**——`starts_with("{stem}-")`
    /// 会让 stem `x` 误统计/删除 `x-y` 的归档（改名/新增 agent 后旧 stem
    /// 归档被连带清理）。
    pub(super) fn is_own_archive_name(name: &str, stem: &str) -> bool {
        let Some(rest) = name.strip_prefix(stem) else {
            return false;
        };
        let Some(rest) = rest.strip_prefix('-') else {
            return false;
        };
        let Some(stamp) = rest.strip_suffix(".json") else {
            return false;
        };
        // stamp: `20261013-074421` / `20261013-074421-123`（2026-10 起带毫秒）
        // 或追加 `-{reason}`（如 `-precompact`）。
        let bytes = stamp.as_bytes();
        if bytes.len() < 15 || bytes[8] != b'-' {
            return false;
        }
        if !bytes[..8].iter().all(u8::is_ascii_digit)
            || !bytes[9..15].iter().all(u8::is_ascii_digit)
        {
            return false;
        }
        // 第 15 位之后：直接结尾（旧格式）/ `-{reason}`（旧）/
        // `-{3位毫秒}`（新）/ `-{3位毫秒}-{reason}`（新）。
        match stamp.len() {
            15 => true,
            19 => bytes[15] == b'-' && bytes[16..19].iter().all(u8::is_ascii_digit),
            _ if bytes[15] == b'-' => {
                // 新格式带 reason：`…-{3位毫秒}-{reason}`
                if bytes[16..19].iter().all(u8::is_ascii_digit) {
                    bytes.len() > 19 && bytes[19] == b'-'
                } else {
                    // 旧格式带 reason：`…-{reason}`
                    true
                }
            }
            _ => false,
        }
    }

    /// 归档保留清理：`archives/` 下**本 stem**（时间戳锚定）的归档按
    /// 文件名内嵌时间戳（字典序 = 时间序）保留最新 `max` 份，多余的最旧
    /// 先删（删除失败仅记日志不阻塞归档）。
    pub fn prune_archives(archive_dir: &std::path::Path, stem: &str, max: usize) {
        if max == 0 || !archive_dir.is_dir() {
            return;
        }
        let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
        if let Ok(read) = std::fs::read_dir(archive_dir) {
            for entry in read.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if !Self::is_own_archive_name(name, stem) {
                    continue;
                }
                entries.push((name.to_string(), entry.path()));
            }
        }
        if entries.len() <= max {
            return;
        }
        // 2026-10 上下文审计 🔵 修复：按文件名排序而非 mtime——归档名内嵌
        // 时间戳（字典序 = 时间序），文件被复制/恢复后 mtime 失真会淘汰错
        // 目标。
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let doomed = entries.len() - max;
        for (_, path) in entries.into_iter().take(doomed) {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!(path = %path.display(), %e, "archive prune failed");
            } else {
                tracing::info!(path = %path.display(), "old archive pruned");
            }
        }
    }

    /// 归档会话（整档）：把**内存快照**（序列化当前态）复制到
    /// `archives/{stem}-{ts}.json`，再清空全部历史（归档后从头开始）。
    /// 返回归档文件路径。
    ///
    /// 2026-10 审计修复两处：
    /// - 归档源从"复制磁盘文件"改为内存快照——磁盘最多落后 30s 保存
    ///   周期，窗口内已接收的事件此前"未归档即被清"，归档与日志双失；
    /// - 提示行走统一 `push_timeline_blocking`（seq / 计数 / 上限），
    ///   不再绕过探针对账（见 [`Self::push_timeline_blocking`]）。
    pub async fn archive_history(&self) -> Result<String, String> {
        if !self.has_archivable_content() {
            return Err("没有可归档的历史（会话为空）".into());
        }
        let data = self
            .serialize()
            .ok_or_else(|| "会话快照序列化失败（时间线锁被占用）".to_string())?;
        let archive = self.write_archive_file(&data, None)?;
        self.clear_history().await;
        self.push_timeline_blocking(crate::event::TimelineMessage::system(
            format!("历史已归档：{archive}"),
            String::new(),
            chrono::Utc::now().timestamp(),
        ))
        .await;
        // 提示行落盘：clear_history 内部 save_now 先于 push 完成，
        // 不落盘则重启后提示行消失（2026-10 归档巡检发现的生产 bug）。
        self.mark_dirty();
        self.save_now().await;
        Ok(archive)
    }

    /// 归档**单个会话**：先写整档快照（与 [`Self::archive_history`] 同一
    /// 文件形态——恢复时可直接作为会话文件手工换回），再只清空该会话的
    /// 历史；其他会话不受影响。
    ///
    /// 面板「归档」按钮的会话级语义（2026-10 审计：此前该命令无 session
    /// 维度，点一次把同人格**所有**会话一并清空——与面板"归档当前会话"
    /// 的承诺不符，属「清理历史记忆」同类漏网的误清）。
    pub async fn archive_session(&self, session_id: &str) -> Result<String, String> {
        if !self.has_archivable_content() {
            return Err("没有可归档的历史（会话为空）".into());
        }
        let data = self
            .serialize()
            .ok_or_else(|| "会话快照序列化失败（时间线锁被占用）".to_string())?;
        let archive = self.write_archive_file(&data, None)?;
        self.clear_session(session_id).await;
        self.push_timeline_blocking(crate::event::TimelineMessage::system(
            format!("会话历史已归档：{archive}（会话 {session_id}）"),
            session_id.to_string(),
            chrono::Utc::now().timestamp(),
        ))
        .await;
        self.mark_dirty();
        self.save_now().await;
        Ok(archive)
    }

    /// 压缩计划（多会话，2026-09）：**按会话分别**计算该会话前
    /// `keep_recent` 条之外、将被 Compaction 摘要替换的事件——纯只读，
    /// 不改动日志。调用方据此先归档、再逐组生成摘要，最后
    /// [`Self::apply_compaction`] 落地。
    ///
    /// 未归因的旧事件单独成组（None 键，与日志同序）。
    pub fn compact_preview(&self, keep_recent: usize) -> Result<CompactPreview, String> {
        let events = self.event_log.log();
        if events.len() <= keep_recent + 1 {
            return Err(format!("历史不足（{} 条事件，无需压缩）", events.len()));
        }
        let total_events = events.len();
        let mut groups = Vec::new();
        let mut total_replaced = 0usize;
        for (key, group) in group_events_by_session(&events) {
            if group.len() <= keep_recent + 1 {
                continue;
            }
            let replaced_count = group.len() - keep_recent;
            let replaced = group[..replaced_count].to_vec();
            let tools = replaced
                .iter()
                .filter(|e| matches!(e, echo_session::SessionEvent::ToolCall(_)))
                .count();
            let users = replaced
                .iter()
                .filter(|e| matches!(e, echo_session::SessionEvent::UserMessage(_)))
                .count();
            total_replaced += replaced_count;
            groups.push(CompactGroup {
                session: key,
                replaced,
                replaced_count,
                users,
                tools,
            });
        }
        if total_replaced == 0 {
            return Err(format!("历史不足（{total_events} 条事件，无需压缩）"));
        }
        Ok(CompactPreview {
            groups,
            total_replaced,
            total_events,
        })
    }

    /// 归档前的空档防守：事件日志与显示时间线都为空时无可归档内容。
    pub(super) fn has_archivable_content(&self) -> bool {
        if !self.event_log.is_empty() {
            return true;
        }
        !self.timeline.lock().is_empty()
    }

    /// 把序列化数据写入 `archives/{stem}-{ts}[-{suffix}].json`，并按
    /// [`Self::DEFAULT_ARCHIVE_MAX_FILES`] 做保留清理。返回归档文件路径。
    ///
    /// 2026-10 审计：归档数据统一**来自内存快照**（调用方先 `serialize()`），
    /// 不再复制磁盘文件——磁盘最多落后 30s 保存周期，此前 ArchiveHistory
    /// 走磁盘会在"归档后立即清空"时让窗口内已接收事件"未归档即被清"，
    /// 归档与日志双失。
    pub(super) fn write_archive_file(
        &self,
        data: &str,
        suffix: Option<&str>,
    ) -> Result<String, String> {
        let path = self
            .persist_path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| "没有可归档的会话文件（未启用持久化）".to_string())?;
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let archive_dir = dir.join("archives");
        std::fs::create_dir_all(&archive_dir)
            .map_err(|e| format!("create archives dir failed: {e}"))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        // 2026-10 上下文审计 🟡 修复：时间戳补毫秒段——秒级精度下同秒两次
        // 归档（压缩前自动归档 + 手动归档连点）会同名覆盖、静默丢一份。
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f");
        let name = match suffix {
            Some(suffix) => format!("{stem}-{ts}-{suffix}.json"),
            None => format!("{stem}-{ts}.json"),
        };
        let archive = archive_dir.join(name);
        std::fs::write(&archive, data).map_err(|e| format!("write archive failed: {e}"))?;
        Self::prune_archives(&archive_dir, stem, Self::DEFAULT_ARCHIVE_MAX_FILES);
        Ok(archive.display().to_string())
    }

    /// 归档当前会话文件的**内存快照**到 `archives/{stem}-{ts}-{reason}.json`。
    ///
    /// 压缩前调用：快照用序列化出的当前态（比直接复制磁盘文件准——磁盘
    /// 可能落后一次 tick）。未配置持久化路径时返回 Err（纯内存部署无档
    /// 可归，调用方继续压缩、只在结果里注明）。
    pub async fn archive_snapshot(&self, reason: &str) -> Result<String, String> {
        let data = self
            .serialize()
            .ok_or_else(|| "会话快照序列化失败（时间线锁被占用）".to_string())?;
        self.write_archive_file(&data, Some(reason))
    }

    /// 落地压缩：把各组前缀替换为其摘要（Compaction 事件带会话归属与
    /// 归档路径），投影随之更新并立即持久化。
    ///
    /// 切片口径（2026-10 审计 🔴 修复两处读-改-写问题）：
    /// - 每组替换**预览时算好的前缀长度**（[`CompactionSummary::replaced_count`]），
    ///   不再按"应用时点 `len - keep_recent`"重算——摘要生成窗口内新到的
    ///   k 条事件会把旧保留尾最老 k 条挤进被替换前缀而**无摘要**（静默
    ///   丢档，仅归档可救）；改用预览切片后，窗口期事件全部留在保留尾。
    /// - 整个读-改-写在 [`echo_session::EventLog::transform`] 的单次取锁内
    ///   完成——"克隆 → 替换"两步之间并发 append 被整体覆盖丢弃的窗口
    ///   （含整日志克隆 + 按会话重排，长会话可达数十 ms）随之关闭。
    ///
    /// `summaries` 按会话键匹配预览分组；缺失的组保持原样（不压缩）。
    /// `keep_recent` 仅用于结果文案（保留尾实际 ≥ 该值）。
    pub async fn apply_compaction(
        &self,
        keep_recent: usize,
        summaries: &[CompactionSummary],
        archive: Option<&str>,
    ) -> Result<CompactionApplied, String> {
        let outcome = self.event_log.transform(|events| {
            let mut new_log: Vec<echo_session::SessionEvent> = Vec::new();
            let mut total_replaced = 0usize;
            let mut groups_done = 0usize;
            for (key, group) in group_events_by_session(events) {
                let prepared = summaries.iter().find(|s| s.session == key);
                let Some(prepared) = prepared else {
                    new_log.extend(group);
                    continue;
                };
                let replace = prepared.replaced_count;
                if replace == 0 || replace >= group.len() {
                    // 组在预览后被外部改动（append-only 下理论不会收缩）：
                    // 保守跳过——宁可不压缩，也不把无摘要内容换出。
                    tracing::warn!(
                        session = ?key,
                        preview_replaced = replace,
                        group_len = group.len(),
                        "compaction: group changed since preview; skipping group"
                    );
                    new_log.extend(group);
                    continue;
                }
                new_log.push(echo_session::SessionEvent::Compaction(
                    echo_session::CompactionEvent {
                        replaced_count: replace,
                        summary: prepared.summary.clone(),
                        archive: archive.map(str::to_string),
                        session: key.clone(),
                    },
                ));
                new_log.extend(group[replace..].to_vec());
                total_replaced += replace;
                groups_done += 1;
            }
            if total_replaced == 0 {
                return None;
            }
            Some((new_log, (total_replaced, groups_done)))
        });
        let Some((total_replaced, groups_done)) = outcome else {
            return Err(format!(
                "历史不足（{} 条事件，无需压缩）",
                self.event_log.len()
            ));
        };
        // 变换已原子提交；投影重建并立即持久化。
        self.reproject_all();
        let archive_note = archive
            .map(|p| format!("，原事件已归档：{p}"))
            .unwrap_or_default();
        self.push_timeline_blocking(crate::event::TimelineMessage::system(
            format!(
                "历史已压缩：{total_replaced} 条事件 → {groups_done} 条摘要（各会话保留最近 ≥{keep_recent} 条）{archive_note}"
            ),
            summaries
                .first()
                .and_then(|summary| summary.session.clone())
                .unwrap_or_default(),
            chrono::Utc::now().timestamp(),
        ))
        .await;
        self.mark_dirty();
        self.save_now().await;
        Ok(CompactionApplied {
            total_replaced,
            groups: groups_done,
            kept: keep_recent,
        })
    }

    // ── Event-sourced session log (Phase 3) ────────────────────────────────
}
