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
    pub async fn clear_history(&self) -> Result<(), PersistSaveError> {
        // 压缩互斥（2026-10 巡检 P1）：compact_history 全程持有 compact_lock
        // （预览→归档→LLM 摘要→replace_all 落地，秒级窗口）。不取锁的话，
        // 清理落在压缩窗口内时，压缩的 replace_all 会把**清理前**的旧事件
        // 重新灌回日志——「清空」被静默复活（清空发生在压缩落地之后才正确）。
        // 与 compact_history 同锁互斥；无嵌套调用（compact 流程不调
        // clear_history），取锁无死锁风险。
        let _compact_guard = self.compact_lock().await;
        let (events_cleared, timeline_cleared) = self.clear_history_in_memory();
        self.save_now_checked().await?;
        tracing::info!(
            events_cleared,
            timeline_cleared,
            "clear_history: all sessions wiped and saved (events + projections + timeline)"
        );
        Ok(())
    }

    /// [`Self::clear_history`] 的内存部分（同步，不跨 await）：
    /// `commit_barrier` **写锁**覆盖"清日志 + 清投影 + 清时间线 + 推进
    /// 历史代次"。
    ///
    /// 与加载提交的互斥（2026-11 第二轮补充）：加载提交持同一把写锁，
    /// 且在写锁内比对 `history_epoch`。清空要么整个发生在加载提交之前
    /// （提交看到代次变化 → 拒绝安装旧历史），要么整个发生在之后
    /// （提交装完，清空再抹掉）——不会出现"清空后旧历史又被装回来"。
    ///
    /// 锁序：`compact_lock` → `commit_barrier`（只此顺序；加载路径只取
    /// `commit_barrier`、不取 `compact_lock`，故无环）。
    pub(super) fn clear_history_in_memory(&self) -> (usize, usize) {
        let _commit_guard = self.commit_barrier.write();
        self.clear_history_under_commit_guard()
    }

    fn clear_history_under_commit_guard(&self) -> (usize, usize) {
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
        self.bump_history_epoch();
        self.mark_dirty();
        (events_cleared, timeline_cleared)
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
    pub async fn clear_session(&self, session_id: &str) -> Result<(), PersistSaveError> {
        let _compact_guard = self.compact_lock().await;
        let (removed, timeline_removed, timeline_remaining) =
            self.clear_session_in_memory(session_id);
        self.save_now_checked().await?;
        tracing::info!(
            session = %session_id,
            removed_events = removed,
            timeline_removed,
            timeline_remaining,
            "session memory cleared and saved (scoped)"
        );
        Ok(())
    }

    /// [`Self::clear_session`] 的内存部分；`commit_barrier` 写锁 + 历史
    /// 代次推进与 [`Self::clear_history_in_memory`] 同语义——加载提交在
    /// 写锁内看到代次变化就拒绝安装文件历史，不会把刚清掉的会话装回来。
    pub(super) fn clear_session_in_memory(&self, session_id: &str) -> (usize, usize, usize) {
        let _commit_guard = self.commit_barrier.write();
        self.clear_session_under_commit_guard(session_id)
    }

    fn clear_session_under_commit_guard(&self, session_id: &str) -> (usize, usize, usize) {
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
        self.bump_history_epoch();
        self.mark_dirty();
        (removed, timeline_removed, timeline_remaining)
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
        self.archive_scoped(None).await
    }

    /// 归档整档快照并只清理指定会话。快照写出前不会清理任何事件。
    pub async fn archive_session(&self, session_id: &str) -> Result<String, String> {
        self.archive_scoped(Some(session_id.to_string())).await
    }

    async fn archive_scoped(&self, session: Option<String>) -> Result<String, String> {
        // 必须在取快照之前取得：与压缩等待期间新增的事件不能漏备份。
        let _compact_guard = self.compact_lock().await;
        let store = self.clone();
        let archive = tokio::task::spawn_blocking(move || {
            // 同步阻塞区：快照、可靠写出、清空是单一边界。append/push 要么
            // 在快照前（已备份），要么在清理后（在线保留），没有双失窗口。
            let _commit_guard = store.commit_barrier.write();
            let has_content = match session.as_deref() {
                Some(id) => {
                    !store.event_log.events_for_session(id).is_empty()
                        || store
                            .timeline
                            .lock()
                            .iter()
                            .any(|entry| entry.session_id == id)
                }
                None => store.has_archivable_content(),
            };
            if !has_content {
                return Err("没有可归档的历史（会话为空）".to_string());
            }
            let data = store
                .serialize_under_commit_guard()
                .ok_or_else(|| "会话快照序列化失败，历史未改动".to_string())?;
            let archive = store.write_archive_file(&data, None)?;
            match session.as_deref() {
                Some(id) => {
                    store.clear_session_under_commit_guard(id);
                }
                None => {
                    store.clear_history_under_commit_guard();
                }
            }
            let mut timeline = store.timeline.lock();
            store.push_timeline_locked(
                &mut timeline,
                agent_module_management::protocol::TimelineMessage::system(
                    match session.as_deref() {
                        Some(id) => format!("会话历史已归档：{archive}（会话 {id}）"),
                        None => format!("历史已归档：{archive}"),
                    },
                    session.unwrap_or_default(),
                    chrono::Utc::now().timestamp(),
                ),
            );
            store.mark_dirty();
            Ok::<_, String>(archive)
        })
        .await
        .map_err(|error| format!("归档任务异常，请核对内存及归档状态：{error}"))??;
        self.save_now_checked().await.map_err(|error| format!(
            "归档快照已写入 {archive}，内存历史已清理，但清理结果尚未落盘；重启可能恢复旧历史：{error}"
        ))?;
        Ok(archive)
    }

    /// 压缩计划（多会话，2026-09）：**按会话分别**计算该会话前
    /// `keep_recent` 条之外、将被 Compaction 摘要替换的事件——纯只读，
    /// 不改动日志。调用方据此先归档、再逐组生成摘要，最后
    /// [`Self::apply_compaction`] 落地。
    ///
    /// 未归因的旧事件单独成组（None 键，与日志同序）。
    ///
    /// **原子快照（2026-11 压缩审计）**：日志克隆与历史代次在同一把
    /// `commit_barrier` 读锁内取得。清空 / 上一次压缩持写锁并推进代次，
    /// 加载提交也持写锁（并在锁内比对代次）——预览若在"读完日志、还没
    /// 读代次"之间被它们插进来，得到的计划会自称属于新代次、内容却来自
    /// 旧日志。读锁保证两者成对，落地再按同一代次 + 前缀指纹校验。
    ///
    /// 普通 append 也持读锁，所以预览不阻塞消息写入；`EventLog::log()`
    /// 自身在日志锁内克隆，是单次一致的快照。
    pub fn compact_preview(&self, keep_recent: usize) -> Result<CompactPreview, String> {
        // 只在这段临界区里持读锁：日志快照与代次必须成对，之后的归组 /
        // 指纹计算不必挡住加载提交（它要同一把锁的写端）。
        let (events, history_epoch) = {
            let _commit_guard = self.commit_barrier.read();
            (self.event_log.log(), self.history_epoch())
        };
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
                prefix_digest: echo_session::CompactionPrefix::of(&replaced),
                history_epoch,
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
        // 同毫秒也可能并发归档；create_new 从文件系统层拒绝覆盖旧备份。
        let mut serial = 0u64;
        let (archive, mut file) = loop {
            let reason = match suffix {
                Some(reason) => format!("-{reason}"),
                None => String::new(),
            };
            let disambiguator = if serial == 0 {
                String::new()
            } else {
                format!("-{serial}")
            };
            let candidate = archive_dir.join(format!("{stem}-{ts}{reason}{disambiguator}.json"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => break (candidate, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    serial += 1;
                }
                Err(error) => return Err(format!("create archive failed: {error}")),
            }
        };
        use std::io::Write as _;
        if let Err(error) = file
            .write_all(data.as_bytes())
            .and_then(|_| file.sync_all())
        {
            let _ = std::fs::remove_file(&archive);
            return Err(format!("write/sync archive failed（历史未清理）: {error}"));
        }
        // 备份作为删除原事件的前置条件，Unix 下还需同步目录项。
        #[cfg(unix)]
        std::fs::File::open(&archive_dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| format!("sync archive directory failed（历史未清理）: {error}"))?;
        Self::prune_archives(&archive_dir, stem, Self::DEFAULT_ARCHIVE_MAX_FILES);
        Ok(archive.display().to_string())
    }

    /// 归档当前会话文件的**内存快照**到 `archives/{stem}-{ts}-{reason}.json`。
    ///
    /// 压缩前调用：快照用序列化出的当前态（比直接复制磁盘文件准——磁盘
    /// 可能落后一次 tick）。未配置持久化路径时返回 Err；压缩默认中止。
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
    /// **提交校验（2026-11 压缩审计，PR2）**——全部在
    /// [`Self::commit_barrier`] 写锁内完成，校验与替换之间没有可插入的
    /// 窗口：
    /// - **前缀指纹**：当前组的前 `replaced_count` 条必须与预览摘要携带的
    ///   [`echo_session::CompactionPrefix`] 一致。普通 append 不改前缀
    ///   （允许）；`insert_after_sequence` 若把回答插进前缀中间则指纹变化
    ///   （拒绝该组）。否则摘要描述的内容与实际被换掉的事件错配。
    /// - **历史代次**：摘要必须来自同一 `history_epoch`；清空 / 另一次
    ///   压缩会推进代次，跨越它们的计划一律视为过期。
    /// - **重复计划**：同一会话在一次落地里出现多条摘要无法判定优先级，
    ///   整份计划拒绝（日志不动）。
    /// - **失败组保留**：某个组校验失败只跳过该组（原事件原样保留），
    ///   其他组照常落地；若所有组都失败则返回 `Err` 且日志不变。成功落地
    ///   会推进代次，被跳过的组要重试必须**重新预览**（旧摘要已过期）。
    ///
    /// `summaries` 按会话键匹配预览分组；缺失的组保持原样（不压缩）。
    /// `keep_recent` 仅用于结果文案（保留尾实际 ≥ 该值）。
    pub async fn apply_compaction(
        &self,
        keep_recent: usize,
        summaries: &[CompactionSummary],
        archive: Option<&str>,
    ) -> Result<CompactionApplied, String> {
        // 与加载提交互斥（2026-11 第二轮补充）：变换 + 代次推进在同一把
        // `commit_barrier` 写锁内完成。加载提交拿到同一把写锁后会比对
        // `history_epoch`——加载期间发生的压缩会推进代次，提交随之中止，
        // 不会把压缩前的旧文件历史装回来（否则刚压缩掉的旧事件会"复活"）。
        // 写锁只包同步内存操作，`save_now().await` 在锁外。
        let mut rejected: Vec<String> = Vec::new();
        let outcome = {
            let _commit_guard = self.commit_barrier.write();
            // 重复计划拒绝：必须在任何变更之前判定（同一会话两条摘要，
            // 谁先谁后都无法自证正确）。
            let mut seen: std::collections::HashSet<Option<&str>> = Default::default();
            for prepared in summaries {
                if !seen.insert(prepared.session.as_deref()) {
                    return Err(format!(
                        "压缩计划重复：会话 {} 出现多条摘要，拒绝落地（日志未改动）",
                        prepared.session.as_deref().unwrap_or("<legacy>")
                    ));
                }
            }
            let current_epoch = self.history_epoch();
            let mut matched = vec![false; summaries.len()];
            let outcome = self.event_log.transform(|events| {
                let mut new_log: Vec<echo_session::SessionEvent> = Vec::new();
                let mut total_replaced = 0usize;
                let mut groups_done = 0usize;
                for (key, group) in group_events_by_session(events) {
                    let Some(index) = summaries.iter().position(|s| s.session == key) else {
                        new_log.extend(group);
                        continue;
                    };
                    matched[index] = true;
                    let prepared = &summaries[index];
                    let who = key.as_deref().unwrap_or("<legacy>");
                    if prepared.history_epoch != current_epoch {
                        rejected.push(format!(
                            "{who}: 预览代次 {} ≠ 当前代次 {current_epoch}（期间发生清空或另一次压缩）",
                            prepared.history_epoch
                        ));
                        tracing::warn!(
                            session = %who,
                            preview_epoch = prepared.history_epoch,
                            current_epoch,
                            "compaction: plan epoch changed since preview; skipping group"
                        );
                        new_log.extend(group);
                        continue;
                    }
                    let replace = prepared.replaced_count;
                    if replace == 0 || replace >= group.len() {
                        // 组在预览后被外部改动（append-only 下理论不会收缩，
                        // 但插入/清空可能让切片越界）：保守跳过——宁可不压缩，
                        // 也不把无摘要内容换出。
                        rejected.push(format!(
                            "{who}: 预览切片 {replace} 条 ≥ 当前组 {} 条（组已收缩或计划过期）",
                            group.len()
                        ));
                        tracing::warn!(
                            session = %who,
                            preview_replaced = replace,
                            group_len = group.len(),
                            "compaction: group changed since preview; skipping group"
                        );
                        new_log.extend(group);
                        continue;
                    }
                    // 前缀校验：当前前 N 条必须**逐字节**等于预览时那 N 条
                    // （SHA-256 指纹）。append 不影响；插入到前缀中间会变。
                    let actual_prefix = echo_session::CompactionPrefix::of(&group[..replace]);
                    if actual_prefix != prepared.expected_prefix {
                        rejected.push(format!(
                            "{who}: 被替换前缀已变化（预览 {}，当前 {}）",
                            prepared.expected_prefix, actual_prefix
                        ));
                        tracing::warn!(
                            session = %who,
                            preview = %prepared.expected_prefix,
                            current = %actual_prefix,
                            preview_replaced = replace,
                            group_len = group.len(),
                            "compaction: prefix changed since preview; skipping group"
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
                    new_log.extend(group[replace..].iter().cloned());
                    total_replaced += replace;
                    groups_done += 1;
                }
                if total_replaced == 0 {
                    return None;
                }
                Some((new_log, (total_replaced, groups_done)))
            });
            if outcome.is_some() {
                self.bump_history_epoch();
                self.mark_dirty();
            }
            // 摘要指向的组已不在日志里（会话被清空/删除）：同样是过期计划。
            for (index, prepared) in summaries.iter().enumerate() {
                if !matched[index] {
                    rejected.push(format!(
                        "{}: 预览组已不在日志中",
                        prepared.session.as_deref().unwrap_or("<legacy>")
                    ));
                }
            }
            outcome
        };
        let Some((total_replaced, groups_done)) = outcome else {
            if !rejected.is_empty() {
                return Err(format!(
                    "压缩计划已过期，未改动日志：{}",
                    rejected.join("；")
                ));
            }
            return Err(format!(
                "历史不足（{} 条事件，无需压缩）",
                self.event_log.len()
            ));
        };
        if !rejected.is_empty() {
            tracing::warn!(
                skipped = rejected.len(),
                reasons = %rejected.join("；"),
                "compaction: some groups rejected by prefix/epoch validation; others applied"
            );
        }
        // 变换已原子提交；投影重建并立即持久化。
        self.reproject_all();
        // **不往显示时间线推"历史已压缩"回执**（2026-11）。
        //
        // 旧行为把一条全局系统行 push 进时间线，于是它夹在对话中间（而且带
        // 的是完整归档路径）。压缩本来就以**每条会话自己的** `Compaction`
        // 事件留在事件日志里——`display_history_for_session` 会为它派生一行
        // 「上下文已压缩 · 替换 N 条事件（原件已归档，可查看压缩前的历史）」，
        // 位置正确、归属正确、还带归档名。回执是重复信息，且只该进进度流
        // （`CompactionProgress`，见 `commands.rs`）。
        //
        // `total_replaced` / `groups_done` 仍作为返回值给调用方写进度事件。
        let _ = (total_replaced, groups_done);
        self.mark_dirty();
        self.save_now_checked().await.map_err(|error| {
            format!("压缩已在内存提交，但尚未落盘（未回滚内存，待重试保存）：{error}")
        })?;
        Ok(CompactionApplied {
            total_replaced,
            groups: groups_done,
            kept: keep_recent,
        })
    }

    // ── Event-sourced session log (Phase 3) ────────────────────────────────
}
