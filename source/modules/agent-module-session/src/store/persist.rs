//! 持久化：周期保存、落盘互斥、序列化与（含旧版格式迁移的）反序列化。
//!
//! 2026-10 架构评审：自 `session.rs`（4300 行）拆出，纯搬移、行为不变。

use super::*;

/// 一次落盘尝试的**本次结论**（[`TrunkStore::write_to_disk_inner`] 的内部
/// 返回值）。
///
/// 与 [`PersistSaveError`] 分开：`Saved` / `NoPath` / 未加载且无改动都是
/// "成功"（checked 接口返回 `Ok`），只有真正"该写却没写成"才变成错误。
enum SaveAttempt {
    /// 当前快照已提交到磁盘（或磁盘上本来没有档，本次是第一次写）。
    Saved,
    /// 纯内存 store：没有 `persist_path`，没有东西可写。代次已对齐。
    NoPath,
    /// 有路径但历史加载失败：拒绝覆盖原文件。
    LoadFailed {
        path: std::path::PathBuf,
        error: String,
    },
    /// 磁盘上已有档但本 store 还没全量加载：拒绝用不全的快照覆盖。
    NotLoaded { path: std::path::PathBuf },
    /// 锁 / 指纹 / 序列化 / 写 tmp / rename 的失败（已记录到
    /// `last_save_error` 的路径会把同一个错误原样带出）。
    Failed(PersistSaveError),
}

/// **结构化**内存快照：锁内只 clone 数据，MB 级 pretty JSON 序列化与
/// 文件 IO 在锁外完成（归档两阶段提交，2026-11）。
///
/// 字段与持久化文档一一对应，捕获时的顺序/内容就是归档要写出的内容。
pub(super) struct PersistSnapshot {
    pub(super) header: Option<echo_session::SessionHeader>,
    /// 事件日志（事实来源；每会话投影加载时重建）。
    pub(super) events: Vec<echo_session::SessionEvent>,
    pub(super) identities: Vec<serde_json::Value>,
    pub(super) timeline: Vec<agent_module_management::protocol::TimelineMessage>,
}

impl PersistSnapshot {
    /// 序列化为持久化文档。返回 `None` 时调用方应跳过写入（不能写过
    /// 半份/空文档覆盖历史）。
    pub(super) fn serialize(&self) -> Option<String> {
        serde_json::to_string_pretty(&serde_json::json!({
            "version": PERSIST_VERSION,
            // The event log is the source of truth; per-session histories are
            // re-projected from it on load.
            "header": self.header,
            "events": self.events,
            "identities": self.identities,
            "timeline": self.timeline,
        }))
        .map_err(|error| {
            tracing::error!(%error, "trunk serialization failed, skipping save");
            error
        })
        .ok()
    }
}

impl TrunkStore {
    /// Save sessions to JSON file if dirty.
    pub async fn maybe_save(&self) {
        if !self.is_dirty() {
            return;
        }
        self.write_to_disk().await;
    }

    /// Force a save now, ignoring the dirty flag (used at shutdown).
    pub async fn save_now(&self) {
        self.write_to_disk().await;
    }

    /// **带结果**的强制落盘（2026-11 跨组闭环）。
    ///
    /// 为什么需要它：`save_now()` 的签名是 `()`——周期性保存可以只记日志，
    /// 但**人格退场的最终快照**不能。拒写（加载失败 / 陈旧快照 / 已有档
    /// 未加载）与 IO 失败（锁、指纹、写 tmp、rename）此前一律被当成成功，
    /// Strict 退休于是放开重建屏障，重启后这段对话却不存在。
    ///
    /// 成功口径（返回 `Ok`）只有两种：
    /// - 当前快照真的写到了磁盘（含"磁盘上还没有档，本次是第一次写"）；
    /// - 纯内存 store（无 `persist_path`）：没有磁盘目标，代次照常对齐
    ///   —— **不把 memory-only 人格报成永久未落盘**。
    ///
    /// 失败口径：加载失败（有路径）、陈旧快照、已有档未加载且仍有未落盘
    /// 改动、序列化失败、以及任何 IO 失败。结论在既有 `save_lock` 内产生
    /// 并**直接返回本次尝试的结果**——不读 [`Self::last_save_error`] 的
    /// 历史值（上一轮周期保存留下的错误 / 本次跳过错过的清理都可能误判）。
    pub async fn save_now_checked(&self) -> Result<(), PersistSaveError> {
        match self.write_to_disk_inner().await {
            SaveAttempt::Saved | SaveAttempt::NoPath => Ok(()),
            SaveAttempt::LoadFailed { path, error } => {
                Err(PersistSaveError::LoadFailed { path, error })
            }
            // 已有档未加载：**没有新改动** = 没东西要落盘，不算失败；
            // 有未落盘改动 = 本次没写成，必须报错（否则改动静默丢失）。
            SaveAttempt::NotLoaded { path } => {
                if self.is_dirty() {
                    Err(PersistSaveError::NotLoaded { path })
                } else {
                    Ok(())
                }
            }
            SaveAttempt::Failed(error) => Err(error),
        }
    }

    /// Write sessions to disk. On failure the dirty flag stays set so the
    /// periodic task retries; failures are logged, not silently dropped.
    ///
    /// 2026-11 第二轮：删除进程级 `path_save_lock` 静态注册表，改为
    /// **实例级文件指纹乐观并发**。同一份文件被两个独立 store 打开时，
    /// 后写者如果基线不是磁盘现状（陈旧快照），明确拒绝——不是靠排队
    /// 让"最后一个 rename"获胜。
    ///
    /// 本包装保持 `()` 契约（周期性保存 / 既有调用方），实际结论由
    /// [`Self::write_to_disk_inner`] 返回；需要知道成败的调用方用
    /// [`Self::save_now_checked`]。
    pub(super) async fn write_to_disk(&self) {
        let _ = self.write_to_disk_inner().await;
    }

    async fn write_to_disk_inner(&self) -> SaveAttempt {
        let _save_guard = self.save_lock.lock().await;
        // **快照代次必须在序列化之前取**（2026-11 B1 修复）。
        //
        // 成功后只把 `persisted_gen` 推进到**本次快照覆盖的那一代**；
        // 期间的新 `mark_dirty` 留下更高代次，`is_dirty()` 仍为真。
        let generation = self.dirty_generation();
        let path = {
            self.persist_path
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        };
        let Some(ref path) = path else {
            // 纯内存 store：没有东西可写。代次照常对齐。
            self.note_persisted(generation);
            return SaveAttempt::NoPath;
        };
        // **加载失败的档绝不覆盖**（2026-11 A）。
        if self.load_stage() == LoadStage::Failed {
            let error = match self.load_error() {
                Some(error) => error.to_string(),
                None => "会话历史加载失败（无具体原因记录）".to_string(),
            };
            tracing::debug!(
                path = %path.display(),
                %error,
                "session save skipped: load failed, keeping the original file"
            );
            return SaveAttempt::LoadFailed {
                path: path.clone(),
                error,
            };
        }
        // **同一路径的跨实例写锁**（2026-11 第二轮，替代 static
        // `path_save_lock` 注册表）：OS 文件锁随进程退出自动释放，
        // 不存在"静态注册表泄漏/旧写者"问题。锁覆盖"指纹检查 → 序列化
        // → rename"整个提交窗口，保证两个独立 store 不会同时通过检查后
        // 互相覆盖；后到者在锁内会看到新指纹并明确 StaleSnapshot。
        let _path_guard = match SavePathLock::acquire(path).await {
            Ok(guard) => guard,
            Err(error) => {
                let error = PersistSaveError::Io {
                    path: path.clone(),
                    error,
                };
                self.set_save_error(error.clone());
                return SaveAttempt::Failed(error);
            }
        };
        // 先取磁盘现状：既用于"没读过就别盖掉已有档"的门禁，也用于
        // 乐观并发比对。取不到（权限等）就当 IO 失败，绝不盲目写。
        let current = match FileStamp::capture(path) {
            Ok(stamp) => stamp,
            Err(error) => {
                let error = PersistSaveError::Io {
                    path: path.clone(),
                    error: format!("读取文件指纹失败：{error}"),
                };
                self.set_save_error(error.clone());
                return SaveAttempt::Failed(error);
            }
        };
        // **没读过就别盖掉已有的档**（2026-11 懒加载的第一条硬约束）。
        //
        // 未加载的 trunk 序列化出来是一份「events: []、timeline: []」的空
        // 文档——写下去等于把磁盘上那份 20 MB 历史抹掉。`dirty` 保持置位，
        // 等真正加载完再落盘。判据是**文件在不在**而不是"阶段是不是 Full"：
        // 新建人格的档本来就不存在（第一次保存必须能写出去）。
        if self.load_stage() != LoadStage::Full && current.is_some() {
            tracing::debug!(
                path = %path.display(),
                stage = ?self.load_stage(),
                "session save skipped: existing file not loaded yet"
            );
            return SaveAttempt::NotLoaded { path: path.clone() };
        }
        let expected = self.persist_origin();
        if !expected.matches(current) {
            let error = PersistSaveError::StaleSnapshot {
                path: path.clone(),
                expected: expected.describe(),
                actual: describe_stamp(current),
            };
            self.set_save_error(error.clone());
            return SaveAttempt::Failed(error);
        }
        let Some(data) = self.serialize() else {
            // Serialization failure — nothing was written, retry next tick.
            return SaveAttempt::Failed(PersistSaveError::Io {
                path: path.clone(),
                error: "序列化失败：内存快照无法序列化，未写入".into(),
            });
        };
        // 仅测试：把"快照之后又 append 了"塞进这个窗口（生产为空操作）。
        self.run_before_commit_hook();
        // 序列化是 MB 级、可能耗时；期间别的 store 可能已经提交过新快照。
        // 提交前再比对一次，把"检查 → 序列化 → rename"缩到只剩 syscall 窗口。
        let current_after = match FileStamp::capture(path) {
            Ok(stamp) => stamp,
            Err(error) => {
                let error = PersistSaveError::Io {
                    path: path.clone(),
                    error: format!("提交前读取文件指纹失败：{error}"),
                };
                self.set_save_error(error.clone());
                return SaveAttempt::Failed(error);
            }
        };
        if !expected.matches(current_after) {
            let error = PersistSaveError::StaleSnapshot {
                path: path.clone(),
                expected: expected.describe(),
                actual: describe_stamp(current_after),
            };
            self.set_save_error(error.clone());
            return SaveAttempt::Failed(error);
        }
        let tmp = format!("{}.tmp", path.display());
        if let Err(e) = tokio::fs::write(&tmp, &data).await {
            let error = PersistSaveError::Io {
                path: path.clone(),
                error: format!("写临时文件失败：{e}"),
            };
            self.set_save_error(error.clone());
            return SaveAttempt::Failed(error);
        }
        // rename 保留 tmp 的 inode/mtime：先取指纹，提交成功后它就是
        // 目标文件的新基线（不必再读一遍 20 MB）。
        let tmp_stamp = match std::fs::metadata(&tmp) {
            Ok(metadata) => Some(FileStamp::from_metadata(&metadata)),
            Err(error) => {
                tracing::warn!(error = %error, path = %path.display(), "session save: tmp fingerprint failed");
                None
            }
        };
        if let Err(e) = tokio::fs::rename(&tmp, path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            let error = PersistSaveError::Io {
                path: path.clone(),
                error: format!("rename 失败：{e}"),
            };
            self.set_save_error(error.clone());
            return SaveAttempt::Failed(error);
        }
        self.note_persisted(generation);
        self.set_persist_origin(match tmp_stamp {
            Some(stamp) => PersistOrigin::At(stamp),
            // 指纹取不到：不假装基线可信，下次写已有文件会被拒绝（安全侧）。
            None => PersistOrigin::Unbaselined,
        });
        self.clear_save_error();
        // 写出去了 = 内存里的状态就是磁盘上的状态。这条路径包含"新建人格的
        // 第一份档"（本来没有历史可读），不必再回读一次；也让后续写入不再
        // 被上面那条 `path.exists()` 守卫挡住。
        if self.load_stage() != LoadStage::Full {
            self.set_load_stage(LoadStage::Full);
        }
        SaveAttempt::Saved
    }

    /// 当前加载阶段。
    pub fn load_stage(&self) -> LoadStage {
        LoadStage::from_u8(self.load_stage.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn set_load_stage(&self, stage: LoadStage) {
        self.load_stage
            .store(stage as u8, std::sync::atomic::Ordering::Relaxed);
    }

    /// 全量数据是否已在内存里。
    pub fn is_loaded(&self) -> bool {
        self.load_stage() == LoadStage::Full
    }

    /// 读磁盘上的会话文件。
    ///
    /// 三态必须分开（2026-11 A）：**没有路径 / 文件不存在**是"新人格，
    /// 没东西可读"；**读失败**是错误（权限 / IO / 非 UTF-8）；两者压成
    /// 同一个 `None` 会让"读不出来"被当成"空档"，下一次保存就覆盖历史。
    ///
    /// 2026-11 第二轮：返回**读前/读后一致的**文件指纹。读期间被替换
    /// （读前读后指纹不同）直接报 `Changed`，不拿半新半旧的内容去提交。
    async fn read_persist_file(
        &self,
    ) -> Result<(std::path::PathBuf, String, Option<FileStamp>), ReadFailure> {
        let Some(path) = self.persist_path.lock().ok().and_then(|slot| slot.clone()) else {
            return Err(ReadFailure::NoPath);
        };
        let before = match FileStamp::capture(&path) {
            Ok(stamp) => stamp,
            Err(error) => {
                return Err(ReadFailure::Error {
                    path,
                    error: error.to_string(),
                })
            }
        };
        let Some(before) = before else {
            return Err(ReadFailure::NotFound);
        };
        let data = match tokio::fs::read_to_string(&path).await {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // 读前还在、读时没了：被别的写者移走/替换，不能当"新人格"。
                return Err(ReadFailure::Changed { path });
            }
            Err(error) => {
                return Err(ReadFailure::Error {
                    path,
                    error: error.to_string(),
                })
            }
        };
        let after = match FileStamp::capture(&path) {
            Ok(stamp) => stamp,
            Err(error) => {
                return Err(ReadFailure::Error {
                    path,
                    error: error.to_string(),
                })
            }
        };
        if after != Some(before) {
            return Err(ReadFailure::Changed { path });
        }
        Ok((path, data, Some(before)))
    }

    /// Load sessions from JSON file. Returns count of restored sessions.
    ///
    /// 失败**不**等于"加载完成"（2026-11 A）：读失败 / 格式非法时置
    /// [`LoadStage::Failed`]，此后 [`Self::write_to_disk`] 拒绝覆盖原文件，
    /// 原因可从 [`Self::load_error`] 读到。
    ///
    /// 2026-11 第二轮：Failed 是负缓存——再次调用直接返回 0，不再自动
    /// 重试/重复 append；重试只能走 [`Self::retry_failed_load`] /
    /// [`Self::recover_failed_load`]。
    pub async fn load_from_file(&self) -> usize {
        let _gate = self.load_gate.lock().await;
        self.load_from_file_locked().await
    }

    async fn load_from_file_locked(&self) -> usize {
        match self.load_stage() {
            LoadStage::Full => return 0,
            LoadStage::Failed => {
                tracing::debug!(
                    error = ?self.load_error(),
                    "session load skipped: failed state is a negative cache (explicit recovery required)"
                );
                return 0;
            }
            _ => {}
        }
        let Some(path) = self.persist_path() else {
            self.clear_load_error();
            self.set_persist_origin(PersistOrigin::Missing);
            self.set_load_stage(LoadStage::Full);
            return 0;
        };
        // **加载不许发生在非空 store 上**（2026-11 第二轮）：把文件事件
        // 追加到已有事件后面会错序，整表 timeline 覆盖会丢新条目。
        // Metadata 阶段预读的身份是同一份文件的元信息，可以升级；Untouched
        // 阶段已有的身份则视为调用方新建的数据。
        let existing_events = self.event_log.with_events(|events| events.len());
        let existing_timeline = self.timeline.lock().len();
        // 身份标签只是元信息（可从文件/事件重建），不算"历史数据"；
        // 真正会错序/被覆盖的是事件日志与显示时间线。
        let non_empty = existing_events > 0 || existing_timeline > 0;
        if non_empty {
            match FileStamp::capture(&path) {
                Ok(Some(_)) => {
                    self.set_load_error(PersistLoadError::NotEmpty {
                        path,
                        events: existing_events,
                        timeline: existing_timeline,
                    });
                    self.set_load_stage(LoadStage::Failed);
                    return 0;
                }
                Ok(None) => {
                    // 没有文件：内存就是全部事实（新人格在首次加载前就写了
                    // 数据），直接进入 Full，第一次保存会写出去。
                    self.clear_load_error();
                    self.set_persist_origin(PersistOrigin::Missing);
                    self.set_load_stage(LoadStage::Full);
                    return 0;
                }
                Err(error) => {
                    self.set_load_error(PersistLoadError::Read {
                        path,
                        error: error.to_string(),
                    });
                    self.set_load_stage(LoadStage::Failed);
                    return 0;
                }
            }
        }
        // 关键：代次在**读盘之前**捕获。完成时只用它标记"文件内容覆盖到的
        // 那一代"，加载期间的新 append 会留下更高代次，不会被打包清掉。
        let generation_at_start = self.dirty_generation();
        // 历史内容代次同样在**解析之前**捕获（2026-11 第二轮补充）：解析
        // 期间若发生 clear_history / clear_session / apply_compaction，
        // 提交时在 `commit_barrier` 写锁内看到代次不一致 → 拒绝安装这份
        // 文件历史，避免"刚清空又被加载装回来"。
        let epoch_at_start = self.history_epoch();
        let (path, data, stamp) = match self.read_persist_file().await {
            Ok(value) => value,
            Err(ReadFailure::NoPath | ReadFailure::NotFound) => {
                self.clear_load_error();
                self.set_persist_origin(PersistOrigin::Missing);
                self.set_load_stage(LoadStage::Full);
                return 0;
            }
            Err(ReadFailure::Changed { path }) => {
                self.set_load_error(PersistLoadError::ChangedDuringLoad { path });
                self.set_load_stage(LoadStage::Failed);
                return 0;
            }
            Err(ReadFailure::Error { path, error }) => {
                self.set_load_error(PersistLoadError::Read { path, error });
                self.set_load_stage(LoadStage::Failed);
                return 0;
            }
        };
        self.set_load_stage(LoadStage::Loading);
        // 隔离解析：parse_document 只构造 ParsedDocument，不碰 store。
        let parsed = match parse_document(&data) {
            Ok(parsed) => parsed,
            Err(ParseError::UnsupportedVersion { version, supported }) => {
                self.set_load_error(PersistLoadError::UnsupportedVersion {
                    path,
                    version,
                    supported,
                });
                self.set_load_stage(LoadStage::Failed);
                return 0;
            }
            Err(ParseError::Corrupt {
                section,
                index,
                error,
            }) => {
                self.set_load_error(PersistLoadError::Corrupt {
                    path,
                    section,
                    index,
                    error,
                });
                self.set_load_stage(LoadStage::Failed);
                return 0;
            }
            Err(ParseError::Invalid(error)) => {
                self.set_load_error(PersistLoadError::Invalid { path, error });
                self.set_load_stage(LoadStage::Failed);
                return 0;
            }
        };
        // 仅测试：确定性地把 append / mark_dirty 塞进"解析完成 → 提交"窗口。
        self.run_before_load_commit_hook();
        // 解析期间文件是否被其他写者换掉：换掉就拒绝提交这份陈旧快照。
        match FileStamp::capture(&path) {
            Ok(current) if current == stamp => {}
            Ok(_) => {
                self.set_load_error(PersistLoadError::ChangedDuringLoad { path });
                self.set_load_stage(LoadStage::Failed);
                return 0;
            }
            Err(error) => {
                self.set_load_error(PersistLoadError::Read {
                    path,
                    error: error.to_string(),
                });
                self.set_load_stage(LoadStage::Failed);
                return 0;
            }
        }
        match self.commit_loaded_document(parsed, epoch_at_start) {
            Ok(restored) => {
                // 只推进到加载开始前的那一代（不是当前代次！）。
                self.note_persisted(generation_at_start);
                self.clear_load_error();
                self.set_persist_origin(match stamp {
                    Some(stamp) => PersistOrigin::At(stamp),
                    None => PersistOrigin::Missing,
                });
                self.set_load_stage(LoadStage::Full);
                restored
            }
            Err(conflict) => {
                let events = self.event_log.with_events(|events| events.len());
                let timeline = self.timeline.lock().len();
                tracing::warn!(
                    path = %path.display(),
                    %conflict,
                    events,
                    timeline,
                    "session load commit rejected: store changed during isolated parse"
                );
                self.set_load_error(PersistLoadError::ConcurrentWrite {
                    path,
                    events,
                    timeline,
                });
                self.set_load_stage(LoadStage::Failed);
                0
            }
        }
    }

    /// **只读元信息**（身份 + 头部），跳过事件日志与显示时间线。
    ///
    /// 面板启动就要会话列表与计数，但那份 20 MB 事件日志只有聊天/看历史时
    /// 才用得上。这里用 [`serde::de::IgnoredAny`] 让解析器**跳过** `events`
    /// 与 `timeline`（只做词法推进、不建树）：实测 21 MB 文件从"解析成整棵
    /// JSON 树（数百 MB 峰值）"降到常数级内存。
    ///
    /// 已经是 `Metadata` 或 `Full` 阶段时是空操作。
    pub async fn ensure_metadata(&self) {
        if self.load_stage() != LoadStage::Untouched {
            return;
        }
        let _gate = self.load_gate.lock().await;
        if self.load_stage() != LoadStage::Untouched {
            return;
        }
        // 三态（2026-11 A）：没有文件 = 空档；读失败 = **失败态**，不写盘。
        let (path, data, stamp) = match self.read_persist_file().await {
            Ok(value) => value,
            Err(ReadFailure::NoPath | ReadFailure::NotFound) => {
                self.clear_load_error();
                self.set_persist_origin(PersistOrigin::Missing);
                self.set_load_stage(LoadStage::Full);
                return;
            }
            Err(ReadFailure::Changed { path }) => {
                self.set_load_error(PersistLoadError::ChangedDuringLoad { path });
                self.set_load_stage(LoadStage::Failed);
                return;
            }
            Err(ReadFailure::Error { path, error }) => {
                self.set_load_error(PersistLoadError::Read { path, error });
                self.set_load_stage(LoadStage::Failed);
                return;
            }
        };
        // 元信息读到的也是同一份文件：记录指纹，后续全量加载会再核对一次。
        self.set_persist_origin(
            stamp
                .map(PersistOrigin::At)
                .unwrap_or(PersistOrigin::Missing),
        );
        #[derive(serde::Deserialize)]
        #[allow(dead_code)] // `events` / `timeline` 只用 IgnoredAny 跳过，不读取
        struct MetadataDocument {
            #[serde(default)]
            header: Option<echo_session::SessionHeader>,
            #[serde(default)]
            identities: Vec<serde_json::Value>,
            /// 跳过（不建树）。
            #[serde(default)]
            events: serde::de::IgnoredAny,
            #[serde(default)]
            timeline: serde::de::IgnoredAny,
        }
        match serde_json::from_str::<MetadataDocument>(&data) {
            Ok(document) => {
                if let Some(header) = document.header {
                    self.set_header(header);
                }
                self.restore_identity_labels(&document.identities);
            }
            // 旧格式（v1–v4：数组根 / 无 events 字段）：元信息档读不出来，
            // 交给全量加载时的迁移路径处理（那里能读 v1–v4）。
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    path = %path.display(),
                    "session metadata preload failed; deferring to full load"
                );
            }
        }
        // 与全量加载同语义：没有 header 的档补顶层 header（分支/续跑元数据
        // 依赖它）。放在这里而不是只由调用方补——两处补会出现"谁先谁后"的
        // 隐性约定。
        if self.header().is_none() {
            self.set_header(echo_session::SessionHeader::top_level("trunk"));
        }
        // **身份为空 = 元信息档不够用，退回全量**（关键回退，别删）。
        //
        // 旧档（v5 及更早写入过的，或曾由 `attribute_legacy_events` 迁移的）
        // 里 `identities` 是空数组，会话只能从**事件的 `session` 字段**派生。
        // 元信息档刻意跳过事件，于是会话列表会是空的——面板会显示"没有会话"，
        // 用户以为历史丢了。实测本机 alix / Alice 就是这种档（703 / 20 条事件、
        // identities 为 0）。
        if self.is_empty() {
            tracing::debug!("session metadata has no identities; falling back to full load");
            self.load_from_file_locked().await;
            return;
        }
        self.set_load_stage(LoadStage::Metadata);
    }

    /// **确保全量数据在内存里**（事件日志 + 显示时间线 + 各会话投影）。
    ///
    /// 所有"要读历史 / 要跑 turn / 要写事件"的入口在动手前调用本方法。
    /// 并发的首批请求由 `load_gate` 串行化——只有第一个真读盘。
    ///
    /// 2026-11 第二轮：`Failed` 是**负缓存**——直接返回，不再每轮自动
    /// 重试（重试会重复 append 文件事件 / 覆盖时间线）。没有持久化路径的
    /// 纯内存 store 直接进 `Full`。
    pub async fn ensure_loaded(&self) {
        if self.is_loaded() {
            return;
        }
        if self.load_stage() == LoadStage::Failed {
            tracing::debug!(error = ?self.load_error(), "session load skipped: failed negative cache");
            return;
        }
        let _gate = self.load_gate.lock().await;
        if self.is_loaded() || self.load_stage() == LoadStage::Failed {
            return;
        }
        self.load_from_file_locked().await;
        if self.is_loaded() && self.header().is_none() {
            self.set_header(echo_session::SessionHeader::top_level("trunk"));
        }
    }

    /// 带结果的 [`Self::ensure_loaded`]：**上层入口**在开始 turn / 执行写
    /// 命令前应当用它，而不是裸 `ensure_loaded`。
    ///
    /// 背景（2026-11 第二轮补充）：`append_event` 的签名是 `void`，失败态
    /// 里它只 `warn` 一句就把事件放进内存，而 [`Self::write_to_disk`] 在
    /// `Failed` 下拒绝落盘——于是 turn 照常跑、模型照常回复，重启后这段
    /// 对话却完全不存在（"假回复"）。本方法把这个失败显式交还调用方：
    /// `Err` 时入口应拒绝开始 turn / 拒绝写命令，并把原因回给用户。
    ///
    /// 注意：调用本方法**不会**每轮重读坏档——`Failed` 是负缓存，只有
    /// [`Self::retry_failed_load`] / [`Self::recover_failed_load`] 能解除。
    pub async fn ensure_loaded_or_err(&self) -> Result<(), String> {
        self.ensure_loaded().await;
        if self.load_stage() != LoadStage::Failed {
            return Ok(());
        }
        Err(match self.load_error() {
            Some(error) => error.to_string(),
            None => "会话历史加载失败（无具体原因记录），已拒绝写入以免覆盖原文件".to_string(),
        })
    }

    /// 失败态是否处于**负缓存**（不会每轮自动重试）。
    pub fn is_load_failed(&self) -> bool {
        self.load_stage() == LoadStage::Failed
    }

    /// **显式**清除 Failed 负缓存，允许下一次 `ensure_loaded` 重试一次。
    ///
    /// 只允许干净空 store（无事件 / 无时间线）：如果失败期间已经有新写入，
    /// 重试会把文件历史追加到新事件后面（错序）或覆盖新时间线，必须先由
    /// 调用方导出/处理新数据。不会自动改名原文件。
    pub fn retry_failed_load(&self) -> Result<(), String> {
        if self.load_stage() != LoadStage::Failed {
            return Err(format!(
                "store 当前不是 Failed（stage={:?}），无需显式重试",
                self.load_stage()
            ));
        }
        let events = self.event_log.with_events(|events| events.len());
        let timeline = self.timeline.lock().len();
        if events != 0 || timeline != 0 {
            return Err(format!(
                "store 非空（events={events}, timeline={timeline}），拒绝隐式重试；请先导出/处理新数据"
            ));
        }
        self.clear_load_error();
        self.clear_save_error();
        self.set_persist_origin(PersistOrigin::Unbaselined);
        self.set_load_stage(LoadStage::Untouched);
        tracing::info!("session load failure explicitly cleared for a single retry");
        Ok(())
    }

    /// 兼容既有调用方的显式重试入口（原第一轮修复引入的 `retry_load`）。
    ///
    /// 返回恢复出来的会话数；内部仍走 [`Self::retry_failed_load`] 的
    /// "只允许干净空 store"检查——非空 store 不会被静默清空/重载。
    pub async fn retry_load(&self) -> usize {
        match self.retry_failed_load() {
            Ok(()) => self.load_from_file().await,
            Err(error) => {
                tracing::warn!(%error, "explicit retry_load refused");
                0
            }
        }
    }

    /// **显式安全恢复出口**：不自动改名坏档。
    ///
    /// - `backup = None`：等同 [`Self::retry_failed_load`]（人工修好文件后重试）；
    /// - `backup = Some(path)`：把当前坏档挪到调用方指定的**确切**路径，然后
    ///   以空 store 进入 `Full`。拒绝覆盖已存在的备份文件，也不自动命名。
    ///
    /// 两种方式都只允许干净空 store；失败期间的新事件必须先处理。
    pub async fn recover_failed_load(
        &self,
        backup: Option<&std::path::Path>,
    ) -> Result<(), String> {
        if self.load_stage() != LoadStage::Failed {
            return Err(format!(
                "store 当前不是 Failed（stage={:?}），无需恢复",
                self.load_stage()
            ));
        }
        let events = self.event_log.with_events(|events| events.len());
        let timeline = self.timeline.lock().len();
        if events != 0 || timeline != 0 {
            return Err(format!(
                "store 非空（events={events}, timeline={timeline}），拒绝恢复；请先导出/处理新数据"
            ));
        }
        let Some(backup) = backup else {
            return self.retry_failed_load();
        };
        let Some(path) = self.persist_path() else {
            return Err("没有 persist_path，无法备份坏档".into());
        };
        if backup == path {
            return Err("备份路径不能等于原文件路径".into());
        }
        if backup.exists() {
            return Err(format!("备份路径已存在，拒绝覆盖：{}", backup.display()));
        }
        if !path.exists() {
            // 坏档已经不在了（人工移走/删除）：等价于一次显式重试。
            return self.retry_failed_load();
        }
        tokio::fs::rename(&path, backup)
            .await
            .map_err(|error| format!("把坏档移到 {} 失败：{error}", backup.display()))?;
        self.clear_load_error();
        self.clear_save_error();
        // 原文件已移走：以空 store 继续，下一次保存会写一份新档。
        self.set_persist_origin(PersistOrigin::Missing);
        if self.header().is_none() {
            self.set_header(echo_session::SessionHeader::top_level("trunk"));
        }
        self.set_load_stage(LoadStage::Full);
        tracing::warn!(
            from = %path.display(),
            to = %backup.display(),
            "failed session file explicitly moved to caller-provided backup"
        );
        Ok(())
    }

    /// Serialize the trunk and identity labels. Returns `None` on failure so
    /// the caller can skip the write instead of silently replacing the file
    /// with garbage.
    pub(super) fn serialize(&self) -> Option<String> {
        // 与清理/归档/压缩提交互斥，避免保存跨越破坏性操作的混合快照。
        let _commit_guard = self.commit_barrier.read();
        self.serialize_under_commit_guard()
    }
    /// 调用方已持有 commit_barrier（save 为读锁、归档为读锁捕获），
    /// 不得再次取同一把锁。
    pub(super) fn serialize_under_commit_guard(&self) -> Option<String> {
        self.snapshot_under_commit_guard().serialize()
    }

    /// 在调用方持有的 `commit_barrier` 临界区内捕获结构化快照。
    ///
    /// 只做 clone（timeline / identities / events / header），不做 JSON
    /// 序列化——归档把它移到锁外（blocking 线程）执行，缩短写锁窗口。
    pub(super) fn snapshot_under_commit_guard(&self) -> PersistSnapshot {
        PersistSnapshot {
            // 短锁内 clone 后立即释放：序列化（MB 级 pretty JSON）不占用
            // timeline 锁，热路径 push 不会被落盘拖住。
            timeline: self.timeline_snapshot(),
            identities: self.all().iter().map(identity_metadata).collect::<Vec<_>>(),
            events: self.event_log.log(),
            header: self
                .header
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    /// 把隔离解析出来的文档一次性安装进**空 store**。
    ///
    /// 调用方（`load_from_file_locked`）已持有 `load_gate`；这里再拿
    /// `commit_barrier` 写锁，确保"检查空 → 安装"之间没有 append/push
    /// 插进来。任何检查失败都在改事件日志之前返回；timeline 的整表替换
    /// 自身也要求空表（失败不会留下半个事件日志）。
    fn commit_loaded_document(
        &self,
        parsed: ParsedDocument,
        epoch_at_start: u64,
    ) -> Result<usize, String> {
        let ParsedDocument {
            header,
            identity_values,
            events,
            timeline,
            attributed,
            media_migrated,
            timeline_media_migrated,
            legacy_rewrite,
        } = parsed;
        let _commit_guard = self.commit_barrier.write();
        // **历史代次比对**（2026-11 第二轮补充）：解析期间发生过清空/
        // 压缩（它们在同一个写锁内推进代次并改内存），这份文件历史已经
        // 过时——绝不能把它装回来。空 store 检查挡不住清空（清空后恰好
        // 是空的），所以这步必须在空检查**之前**：错误原因更准确，也
        // 避免"空 store 通过、安装旧历史"。
        let epoch_now = self.history_epoch();
        if epoch_now != epoch_at_start {
            return Err(format!(
                "历史代次已变化（加载期间发生了清空/压缩：{epoch_at_start} → {epoch_now}），拒绝安装过时文件历史"
            ));
        }
        let existing_events = self.event_log.with_events(|events| events.len());
        let existing_timeline = self.timeline.lock().len();
        if existing_events != 0 || existing_timeline != 0 {
            return Err(format!(
                "store 非空（events={existing_events}, timeline={existing_timeline}），拒绝加载合并"
            ));
        }
        // 先装时间线：它要求空表，失败时事件日志还没来得及追加。
        self.restore_timeline(timeline)?;
        if let Some(header) = header {
            self.set_header(header);
        }
        let restored = self.restore_identity_labels(&identity_values);
        if !events.is_empty() {
            // 事件日志里出现但身份元数据缺失的会话也要补建（含归属）——
            // 否则 Panel 会话列表丢项。
            self.ensure_identities_for_events(&events);
            // 按会话重建投影（身份 + 事件中出现的所有会话）。
            self.ensure_histories_for_events(&events);
            self.event_log.extend(events.clone());
            self.reproject_all();
        }
        if attributed {
            tracing::info!(
                events = events.len(),
                "legacy session events attributed to per-session contexts"
            );
            self.mark_dirty();
        }
        if media_migrated || timeline_media_migrated || legacy_rewrite {
            self.mark_dirty();
        }
        Ok(restored)
    }

    /// Restore a persisted file (lenient: 非法文档 = 0)。
    ///
    /// **仅测试**：生产加载路径走 `load_from_file_locked`（要区分
    /// "非法/未来版本"与"空档"，并且要维护 Failed 负缓存与文件指纹）。
    /// 这里保留宽松语义给既有用例（v1–v4 迁移、单测里的 deserialize）。
    #[cfg(test)]
    pub(super) fn deserialize(&self, data: &str) -> usize {
        match parse_document(data) {
            Ok(parsed) => match self.commit_loaded_document(parsed, self.history_epoch()) {
                Ok(restored) => restored,
                Err(error) => {
                    tracing::warn!(%error, "test deserialize commit rejected");
                    0
                }
            },
            Err(error) => {
                tracing::warn!(%error, "test deserialize rejected");
                0
            }
        }
    }

    /// Restore identity labels from a v4/v5 document root.
    pub(super) fn restore_identity_labels(&self, identity_values: &[serde_json::Value]) -> usize {
        let mut count = 0;
        for s in identity_values {
            let Some(_id) = s["id"].as_str() else {
                continue;
            };
            // 旧档里的 id 可能带 `@account` 后缀（多实例）：以 id 为准解析，
            // 缺字段的旧记录按默认实例处理。
            let key = SessionKey::parse(_id).unwrap_or(SessionKey {
                platform: s["platform"].as_str().unwrap_or("local").into(),
                scope: s["scope"].as_str().unwrap_or("tui").into(),
                scope_id: s["scope_id"].as_str().unwrap_or("").into(),
                user_id: s["user_id"].as_str().unwrap_or("0").into(),
                account: None,
            });
            let nickname = s["nickname"].as_str().unwrap_or("").into();
            let group_name = s["group_name"].as_str().map(|g| g.to_string());
            let session = self.get_or_create(&key, nickname, group_name);
            // 归属持久化（2026-09-18 起）：旧档无此字段时保持 None，
            // 由 get_or_create 的补标逻辑或事件归属兜底。
            if let Some(team_id) = s["team_id"].as_str() {
                if session.team_id.is_none() {
                    if let Some(mut entry) = self.identities.get_mut(&session.id) {
                        entry.team_id = Some(team_id.to_string());
                    }
                }
            }
            if let Some(last_active) = s["last_active"].as_i64() {
                session.last_active.store(last_active, Ordering::Relaxed);
            }
            count += 1;
        }
        count
    }
}

/// `read_persist_file` 的失败三态（2026-11 A）。
///
/// 单独一个类型而不是 `Option`：`None` 把"没路径 / 没文件"和"读失败"
/// 压成同一件事，于是读失败被当成空档 → 清 dirty → 下次保存覆盖历史。
enum ReadFailure {
    /// 没有持久化路径（纯内存 store）。
    NoPath,
    /// 路径存在但文件不在：新人格，没东西可读。
    NotFound,
    /// 读之前文件还在，读的时候/读之后被替换（指纹变化）。
    Changed { path: std::path::PathBuf },
    /// 读文件失败（权限 / IO / 非 UTF-8）。**不是**"空档"。
    Error {
        path: std::path::PathBuf,
        error: String,
    },
}

/// 隔离解析出来的持久化文档（**不碰 store**）。
///
/// 解析阶段只做纯数据转换；所有 store 变更集中在
/// `TrunkStore::commit_loaded_document`，便于"先完整校验、后一次提交"。
#[derive(Default)]
struct ParsedDocument {
    header: Option<echo_session::SessionHeader>,
    identity_values: Vec<serde_json::Value>,
    events: Vec<echo_session::SessionEvent>,
    timeline: Vec<agent_module_management::protocol::TimelineMessage>,
    /// 旧事件缺少 `session` 归属，加载期做了归因迁移（需要回写）。
    attributed: bool,
    /// 事件里的内嵌媒体被落盘成 `/media/<id>` 引用（需要回写）。
    media_migrated: bool,
    /// 显示时间线里的内嵌媒体被落盘（需要回写）。
    timeline_media_migrated: bool,
    /// 极老格式（未走 v4 迁移）转成事件日志后需要回写。
    legacy_rewrite: bool,
}

/// 解析失败（在接触 store 之前）。分类只用于映射成可观察的加载错误。
enum ParseError {
    Invalid(String),
    UnsupportedVersion {
        version: u64,
        supported: u32,
    },
    Corrupt {
        section: &'static str,
        index: usize,
        error: String,
    },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(error) => write!(f, "{error}"),
            Self::UnsupportedVersion { version, supported } => {
                write!(f, "未来版本 v{version}（最高支持 v{supported}）")
            }
            Self::Corrupt {
                section,
                index,
                error,
            } => write!(f, "{section}[{index}] 不是合法条目：{error}"),
        }
    }
}

/// 文档形状：事件溯源（v5+）或旧格式（v1–v4）。
enum DocumentShape {
    EventSourced {
        header: Option<echo_session::SessionHeader>,
        events: Vec<echo_session::SessionEvent>,
        timeline: Vec<agent_module_management::protocol::TimelineMessage>,
    },
    Legacy,
}

/// 解析一份持久化文档为纯 `ParsedDocument`（2026-11 第二轮）。
///
/// 关键约束：**当前版/未来版本一律严格**，旧格式才宽容。
/// - 空文件/全空白 = 合法空档（第一次保存会写成合法文档）；
/// - `version > PERSIST_VERSION` → 拒绝；
/// - 带 `events` 数组（v5/v6，或无 version 的手写档）→ 每个事件都必须
///   能反序列化；未知 `kind` / 字段类型错 → 拒绝，不能被 filter_map 吞；
/// - `timeline` / `header` 在事件溯源文档里同样严格；
/// - v1–v4 / 数组根 → legacy 迁移（保留旧档宽容）。
fn parse_document(data: &str) -> Result<ParsedDocument, ParseError> {
    if data.trim().is_empty() {
        // 0 字节 / 全空白：没有历史可保护，让第一次落盘写成合法文档。
        return Ok(ParsedDocument::default());
    }
    let root = serde_json::from_str::<serde_json::Value>(data)
        .map_err(|error| ParseError::Invalid(format!("非法 JSON：{error}")))?;
    match validate_document_shape(&root)? {
        DocumentShape::EventSourced {
            header,
            events,
            timeline,
        } => {
            let mut events = events;
            // 旧版（v5 及更早）事件没有会话归属：加载期归因迁移，否则
            // 多会话投影会把整段历史丢弃（或互相泄漏）。这一步只改纯数据。
            let attributed = attribute_legacy_events(&mut events);
            // 媒体引用迁移（2026-09-24）：历史事件里内嵌的 data URI 图片
            // 落盘并改写为 `/media/<id>` 引用——不丢图片（模型上下文与
            // 面板展示都还原），但文件从数十 MB 收缩到 KB 级。
            let media_migrated = spill_event_media(&mut events);
            let mut timeline = timeline;
            let timeline_media_migrated = spill_timeline_media(&mut timeline);
            Ok(ParsedDocument {
                header,
                identity_values: identity_values_of(&root),
                events,
                timeline,
                attributed,
                media_migrated,
                timeline_media_migrated,
                legacy_rewrite: false,
            })
        }
        DocumentShape::Legacy => {
            // v4 and older: migrate into the event log (compatibility read).
            if let Ok(migrated) = echo_session::legacy::migrate_v4_document(data) {
                Ok(parse_migrated_v4(&root, migrated))
            } else {
                Ok(parse_legacy_document(&root))
            }
        }
    }
}

/// 形状 + 版本 + 当前版条目的严格校验。
///
/// 判据只认"曾经被写出过 / 被迁移代码认识"的形状，宁可报错也不猜：
/// - 数组根 = v1 每身份一份历史；
/// - 带 `events` 数组的对象 = v5+ 事件溯源（逐条严格）；
/// - 对象根带 `identities` / `sessions` / `trunk_history` /
///   `shared_history` 之一 = v2–v4 的某种合法旧文档；
/// - 只有 `version <= 4` 的对象 = 旧版空档（`migrate_v4_document` 认得）；
/// - 其余（截断 JSON 之外的任意形状，例如 `{"foo":1}`）= 非法。
fn validate_document_shape(root: &serde_json::Value) -> Result<DocumentShape, ParseError> {
    match root {
        serde_json::Value::Array(_) => Ok(DocumentShape::Legacy),
        serde_json::Value::Object(_) => {
            let version = match root.get("version") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::Number(number)) => Some(
                    number
                        .as_u64()
                        .ok_or_else(|| ParseError::Invalid("version 不是非负整数".into()))?,
                ),
                Some(_) => {
                    return Err(ParseError::Invalid("version 字段不是整数".into()));
                }
            };
            if let Some(version) = version {
                if version > u64::from(PERSIST_VERSION) {
                    return Err(ParseError::UnsupportedVersion {
                        version,
                        supported: PERSIST_VERSION,
                    });
                }
            }
            let has_events_array = root.get("events").is_some_and(|value| value.is_array());
            // 当前代码对"只要带 events 数组"就走事件溯源（与 version 无关，
            // 早期 v5 迁移档也这样）；v5+ 则必须有 events。
            let event_sourced = has_events_array || version.is_some_and(|value| value >= 5);
            if event_sourced {
                if !has_events_array {
                    return Err(ParseError::Invalid(format!(
                        "v{} 会话文档缺少 events 数组（不能静默当作空历史）",
                        version.unwrap_or(5)
                    )));
                }
                let events = parse_strict_events(root["events"].as_array().unwrap())?;
                let timeline = parse_strict_timeline(root)?;
                let header = parse_strict_header(root)?;
                for key in ["identities", "sessions"] {
                    if let Some(value) = root.get(key) {
                        if !value.is_array() {
                            return Err(ParseError::Invalid(format!(
                                "{key} 字段不是数组（不能静默丢弃身份）"
                            )));
                        }
                    }
                }
                return Ok(DocumentShape::EventSourced {
                    header,
                    events,
                    timeline,
                });
            }
            let recognized = ["identities", "sessions", "trunk_history", "shared_history"]
                .iter()
                .any(|key| root.get(*key).is_some_and(|value| value.is_array()));
            match version {
                Some(version) if version <= 4 => Ok(DocumentShape::Legacy),
                Some(version) => Err(ParseError::Invalid(format!(
                    "v{version} 会话文档缺少 events / identities / sessions 等可识别字段"
                ))),
                None if recognized => Ok(DocumentShape::Legacy),
                None => Err(ParseError::Invalid(
                    "对象根既没有 events 也没有 identities/sessions".into(),
                )),
            }
        }
        serde_json::Value::Null => Err(ParseError::Invalid("文档是 null".into())),
        other => Err(ParseError::Invalid(format!(
            "文档根节点是 {}，不是对象或数组",
            match other {
                serde_json::Value::Bool(_) => "布尔值",
                serde_json::Value::Number(_) => "数字",
                serde_json::Value::String(_) => "字符串",
                _ => "未知类型",
            }
        ))),
    }
}

fn parse_strict_events(
    events: &[serde_json::Value],
) -> Result<Vec<echo_session::SessionEvent>, ParseError> {
    let mut parsed = Vec::with_capacity(events.len());
    for (index, event) in events.iter().enumerate() {
        let event = serde_json::from_value::<echo_session::SessionEvent>(event.clone()).map_err(
            |error| ParseError::Corrupt {
                section: "events",
                index,
                error: error.to_string(),
            },
        )?;
        parsed.push(event);
    }
    Ok(parsed)
}

fn parse_strict_timeline(
    root: &serde_json::Value,
) -> Result<Vec<agent_module_management::protocol::TimelineMessage>, ParseError> {
    match root.get("timeline") {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(entries)) => {
            let mut parsed = Vec::with_capacity(entries.len());
            for (index, entry) in entries.iter().enumerate() {
                let entry = serde_json::from_value::<
                    agent_module_management::protocol::TimelineMessage,
                >(entry.clone())
                .map_err(|error| ParseError::Corrupt {
                    section: "timeline",
                    index,
                    error: error.to_string(),
                })?;
                parsed.push(entry);
            }
            Ok(parsed)
        }
        Some(_) => Err(ParseError::Invalid("timeline 字段不是数组".into())),
    }
}

fn parse_strict_header(
    root: &serde_json::Value,
) -> Result<Option<echo_session::SessionHeader>, ParseError> {
    let value = root
        .get("header")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    serde_json::from_value::<Option<echo_session::SessionHeader>>(value).map_err(|error| {
        ParseError::Corrupt {
            section: "header",
            index: 0,
            error: error.to_string(),
        }
    })
}

/// v2–v4 迁移结果 → 纯 ParsedDocument。
fn parse_migrated_v4(
    root: &serde_json::Value,
    migrated: Vec<echo_session::legacy::MigratedSession>,
) -> ParsedDocument {
    let mut all_events: Vec<echo_session::SessionEvent> = Vec::new();
    let mut first_events: Option<Vec<echo_session::SessionEvent>> = None;
    for session in &migrated {
        // v2–v4 共享 trunk：每个身份携带同一份事件，只保留首份
        //（归首个身份）。v1 是每身份各自历史，逐个归入自身会话。
        let duplicate_shared = first_events
            .as_ref()
            .is_some_and(|first| *first == session.events);
        if duplicate_shared {
            continue;
        }
        if first_events.is_none() {
            first_events = Some(session.events.clone());
        }
        let mut events = session.events.clone();
        // 合成 "trunk"（无身份标签的早期档案）留给启发式归因。
        if session.session_id != "trunk" {
            for event in events.iter_mut() {
                if event.session().is_none() {
                    *event.session_mut() = Some(session.session_id.clone());
                }
            }
        }
        all_events.extend(events);
    }
    all_events.dedup();
    // 剩余未归因（合成 trunk 等）：归因迁移补全。
    let attributed = attribute_legacy_events(&mut all_events);
    let mut timeline = parse_timeline_lenient(root);
    let timeline_media_migrated = spill_timeline_media(&mut timeline);
    ParsedDocument {
        header: None,
        identity_values: identity_values_of(root),
        events: all_events,
        timeline,
        attributed,
        media_migrated: false,
        timeline_media_migrated,
        legacy_rewrite: false,
    }
}

/// v1/v2/v3 极老格式（`migrate_v4_document` 也读不了时）→ 纯 ParsedDocument。
///
/// 这里保留原来的宽容语义：非对象条目跳过、缺字段用默认值；旧档不值得
/// 因为一条坏记录整份拒绝。
fn parse_legacy_document(root: &serde_json::Value) -> ParsedDocument {
    let (identity_values, persisted_trunk) = match root {
        serde_json::Value::Array(sessions) => (sessions.clone(), None),
        serde_json::Value::Object(_) => {
            let Some(sessions) = root["identities"]
                .as_array()
                .or_else(|| root["sessions"].as_array())
            else {
                return ParsedDocument::default();
            };
            (
                sessions.clone(),
                root["trunk_history"]
                    .as_array()
                    .or_else(|| root["shared_history"].as_array()),
            )
        }
        _ => return ParsedDocument::default(),
    };
    // 最老格式（v1/v2，2026-09 多会话改造）：把消息转成**带归属**的事件
    // ——v1 的每身份历史归入各自会话；v2/v3 的共享 trunk 归入首个身份
    // （没有身份时归本地会话），此后一切走事件日志投影。
    let mut events: Vec<echo_session::SessionEvent> = Vec::new();
    let mut shared_owner: Option<String> = None;
    for s in &identity_values {
        let Some(id) = s["id"].as_str() else {
            continue;
        };
        // 旧档里的 id 可能带 `@account` 后缀（多实例）：以 id 为准解析，
        // 缺字段的旧记录按默认实例处理。
        let key = SessionKey::parse(id).unwrap_or(SessionKey {
            platform: s["platform"].as_str().unwrap_or("local").into(),
            scope: s["scope"].as_str().unwrap_or("tui").into(),
            scope_id: s["scope_id"].as_str().unwrap_or("").into(),
            user_id: s["user_id"].as_str().unwrap_or("0").into(),
            account: None,
        });
        let session_id = key.to_session_id();
        if shared_owner.is_none() {
            shared_owner = Some(session_id.clone());
        }
        if persisted_trunk.is_none() {
            if let Some(history) = s["history"].as_array() {
                events.extend(events_from_messages(
                    &deserialize_messages(history),
                    Some(session_id),
                ));
            }
        }
    }
    if let Some(shared) = persisted_trunk {
        let owner = shared_owner.unwrap_or_else(|| SessionKey::local_tui().to_session_id());
        events.extend(events_from_messages(
            &deserialize_messages(shared),
            Some(owner),
        ));
    }
    let mut timeline = parse_timeline_lenient(root);
    let timeline_media_migrated = spill_timeline_media(&mut timeline);
    let legacy_rewrite = !events.is_empty();
    ParsedDocument {
        header: None,
        identity_values,
        events,
        timeline,
        attributed: false,
        media_migrated: false,
        timeline_media_migrated,
        legacy_rewrite,
    }
}

fn parse_timeline_lenient(
    root: &serde_json::Value,
) -> Vec<agent_module_management::protocol::TimelineMessage> {
    root["timeline"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    serde_json::from_value::<agent_module_management::protocol::TimelineMessage>(
                        entry.clone(),
                    )
                    .ok()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 显示时间线里的内嵌媒体 → `/media/<id>` 引用；返回是否有改动。
fn spill_timeline_media(
    timeline: &mut [agent_module_management::protocol::TimelineMessage],
) -> bool {
    let mut changed = false;
    for entry in timeline.iter_mut() {
        if let Some(images) = entry.images.as_mut() {
            for image in images.iter_mut() {
                let spilled = echo_defs::media_store::spill_or_keep(image);
                if spilled != *image {
                    changed = true;
                    *image = spilled;
                }
            }
        }
    }
    changed
}

/// 同一路径的跨实例写锁（OS advisory lock）。
///
/// 用 `<session>.save-lock` 作为锁文件，`File::try_lock` 是 OS 级独占锁：
/// 进程退出/文件关闭自动释放，不会留下"需要人工清理的 stale lock"。
/// 所有本模块的写者都走这里；旧版本/外部写者不遵守锁时，仍由文件指纹
/// 在提交前拒绝陈旧快照。锁文件本身不再删除（删除会与等待者的 open
/// 产生 unlink 竞态），内容为空、常驻成本可忽略。
struct SavePathLock {
    _file: std::fs::File,
}

impl SavePathLock {
    async fn acquire(path: &std::path::Path) -> Result<Self, String> {
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".save-lock");
        let lock_path = std::path::PathBuf::from(lock_path);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| format!("打开写锁文件 {} 失败：{error}", lock_path.display()))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(format!("等待写锁超时：{}", lock_path.display()));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!("获取写锁失败（{}）：{error}", lock_path.display()));
                }
            }
        }
    }
}

fn describe_stamp(stamp: Option<FileStamp>) -> String {
    stamp
        .map(|stamp| stamp.describe())
        .unwrap_or_else(|| "absent".into())
}

/// 从持久化文档里取身份数组（v1 数组根 / v5+ 的 `identities`、兼容 `sessions`）。
fn identity_values_of(root: &serde_json::Value) -> Vec<serde_json::Value> {
    match root {
        serde_json::Value::Array(sessions) => sessions.clone(),
        serde_json::Value::Object(_) => root["identities"]
            .as_array()
            .or_else(|| root["sessions"].as_array())
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}
