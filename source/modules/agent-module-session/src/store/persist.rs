//! 持久化：周期保存、落盘互斥、序列化与（含旧版格式迁移的）反序列化。
//!
//! 2026-10 架构评审：自 `session.rs`（4300 行）拆出，纯搬移、行为不变。

use super::*;

impl TrunkStore {
    /// Save sessions to JSON file if dirty.
    pub async fn maybe_save(&self) {
        if !self.dirty.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.write_to_disk().await;
    }

    /// Force a save now, ignoring the dirty flag (used at shutdown).
    pub async fn save_now(&self) {
        self.write_to_disk().await;
    }

    /// Write sessions to disk. On failure the dirty flag stays set so the
    /// periodic task retries; failures are logged, not silently dropped.
    pub(super) async fn write_to_disk(&self) {
        // 落盘互斥（2026-10 审计）：与周期保存 / 其它 save_now 串行。
        // 串行化后不存在"旧快照后 rename 覆盖新快照"的窗口——等待者在
        // 锁内重新序列化，写出的永远是最新（或更新）状态。
        let _save_guard = self.save_lock.lock().await;
        let path = {
            self.persist_path
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        };
        let Some(ref path) = path else {
            self.dirty
                .store(false, std::sync::atomic::Ordering::Relaxed);
            return;
        };
        // **没读过就别盖掉已有的档**（2026-11 懒加载的第一条硬约束）。
        //
        // 未加载的 trunk 序列化出来是一份「events: []、timeline: []」的空
        // 文档——写下去等于把磁盘上那份 20 MB 历史抹掉。`dirty` 保持置位，
        // 等真正加载完再落盘。
        //
        // 判据是**文件在不在**而不是"阶段是不是 Full"：新建人格的档本来
        // 就不存在（第一次保存必须能写出去）。文件存在 + 没全量加载 =
        // 内存里不是这份文件的内容 → 跳过。
        if self.load_stage() != LoadStage::Full && path.exists() {
            tracing::debug!(
                path = %path.display(),
                stage = ?self.load_stage(),
                "session save skipped: existing file not loaded yet"
            );
            return;
        }
        let Some(data) = self.serialize() else {
            // Serialization failure — nothing was written, retry next tick.
            return;
        };
        let tmp = format!("{}.tmp", path.display());
        if let Err(e) = tokio::fs::write(&tmp, &data).await {
            tracing::warn!(error = %e, path = %path.display(), "session save: tmp write failed, will retry");
            return;
        }
        if let Err(e) = tokio::fs::rename(&tmp, path).await {
            tracing::warn!(error = %e, path = %path.display(), "session save: rename failed, will retry");
            let _ = tokio::fs::remove_file(&tmp).await;
            return;
        }
        self.dirty
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // 写出去了 = 内存里的状态就是磁盘上的状态。这条路径包含"新建人格的
        // 第一份档"（本来没有历史可读），不必再回读一次；也让后续写入不再
        // 被上面那条 `path.exists()` 守卫挡住。
        if self.load_stage() != LoadStage::Full {
            self.set_load_stage(LoadStage::Full);
        }
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

    /// 读磁盘上的会话文件（`None` = 没有持久化路径 / 读失败）。
    async fn read_persist_file(&self) -> Option<String> {
        let path = self.persist_path.lock().ok().and_then(|slot| slot.clone())?;
        tokio::fs::read_to_string(path).await.ok()
    }

    /// Load sessions from JSON file. Returns count of restored sessions.
    pub async fn load_from_file(&self) -> usize {
        let Some(data) = self.read_persist_file().await else {
            // 没有路径 / 读不出来：没有东西可加载，直接视为已加载（否则
            // 每次请求都会重试读盘一次）。
            self.set_load_stage(LoadStage::Full);
            return 0;
        };
        let restored = self.deserialize(&data);
        self.set_load_stage(LoadStage::Full);
        // 加载完成 = 内存里的状态就是磁盘上的状态，不必再写一遍。
        self.dirty
            .store(false, std::sync::atomic::Ordering::Relaxed);
        restored
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
        let Some(data) = self.read_persist_file().await else {
            self.set_load_stage(LoadStage::Full);
            return;
        };
        #[derive(serde::Deserialize)]
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
        if self.len() == 0 {
            tracing::debug!(
                "session metadata has no identities; falling back to full load"
            );
            self.load_from_file().await;
            return;
        }
        self.set_load_stage(LoadStage::Metadata);
    }

    /// **确保全量数据在内存里**（事件日志 + 显示时间线 + 各会话投影）。
    ///
    /// 所有"要读历史 / 要跑 turn / 要写事件"的入口在动手前调用本方法。
    /// 并发的首批请求由 `load_gate` 串行化——只有第一个真读盘。
    ///
    /// 没有持久化路径的纯内存 store 直接进 `Full`（没有东西可读）。
    pub async fn ensure_loaded(&self) {
        if self.is_loaded() {
            return;
        }
        let _gate = self.load_gate.lock().await;
        if self.is_loaded() {
            return;
        }
        self.load_from_file().await;
        // 与启动期同语义：没有 header 的档补一个顶层 header（分支/续跑
        // 元数据依赖它）。
        if self.header().is_none() {
            self.set_header(echo_session::SessionHeader::top_level("trunk"));
        }
    }

    /// Serialize the trunk and identity labels. Returns `None` on failure so
    /// the caller can skip the write instead of silently replacing the file
    /// with garbage.
    pub(super) fn serialize(&self) -> Option<String> {
        // 短锁内 clone 后立即释放：序列化（MB 级 pretty JSON）不占用
        // timeline 锁，热路径 push 不会被落盘拖住。
        let timeline = self.timeline_snapshot();
        let identities = self.all().iter().map(identity_metadata).collect::<Vec<_>>();
        let events = self.event_log.log();
        let header = self
            .header
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        serde_json::to_string_pretty(&serde_json::json!({
            "version": PERSIST_VERSION,
            // The event log is the source of truth; per-session histories are
            // re-projected from it on load.
            "header": header,
            "events": events,
            "identities": identities,
            "timeline": &timeline,
        }))
        .map_err(|error| {
            tracing::error!(%error, "trunk serialization failed, skipping save");
            error
        })
        .ok()
    }

    /// Restore a persisted file.
    ///
    /// v5 (event-sourced): the event log is the source of truth and re-derives
    /// the trunk projection. v4 and older (v1 per-session array, v2 shared
    /// context, v3 single trunk): the document is migrated into the event log
    /// (`echo_session::legacy`), preserving old-history compatibility. Returns
    /// the number of restored identities.
    pub(super) fn deserialize(&self, data: &str) -> usize {
        let Ok(root) = serde_json::from_str::<serde_json::Value>(data) else {
            return 0;
        };

        // v5/v6: event log is authoritative. Re-project per session from it.
        if let Some(events) = root["events"].as_array() {
            // 只在文件**确实带 header**时覆盖：懒加载下内存里可能已经装好
            // 默认 header（`ensure_loaded` 补的），文件里是 null 不该把它清掉。
            if let Ok(Some(header)) = serde_json::from_value::<Option<echo_session::SessionHeader>>(
                root["header"].clone(),
            ) {
                *self
                    .header
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(header);
            }
            let mut events: Vec<echo_session::SessionEvent> = events
                .iter()
                .filter_map(|event| serde_json::from_value(event.clone()).ok())
                .collect();
            // 旧版（v5 及更早）事件没有会话归属：加载期归因迁移，否则
            // 多会话投影会把整段历史丢弃（或互相泄漏）。
            let attributed = attribute_legacy_events(&mut events);
            // 媒体引用迁移（2026-09-24）：历史事件里内嵌的 data URI 图片
            // 落盘并改写为 `/media/<id>` 引用——不丢图片（模型上下文与
            // 面板展示都还原），但文件从数十 MB 收缩到 KB 级。
            let media_migrated = spill_event_media(&mut events);
            let version = root["version"].as_u64();
            if !events.is_empty() || version == Some(5) || version == Some(6) {
                self.event_log.extend(events.clone());
                if attributed {
                    tracing::info!(
                        events = events.len(),
                        "legacy session events attributed to per-session contexts"
                    );
                    self.mark_dirty();
                }
                if media_migrated {
                    self.mark_dirty();
                }
                let count = self.restore_identity_labels(&identity_values_of(&root));
                // 事件日志里出现但身份元数据缺失的会话也要补建（含归属）——
                // 否则 Panel 会话列表丢项。
                self.ensure_identities_for_events(&events);
                // 按会话重建投影（身份 + 事件中出现的所有会话）。
                self.ensure_histories_for_events(&events);
                self.reproject_all();
                let mut timeline = root["timeline"]
                    .as_array()
                    .map(|entries| {
                        entries
                            .iter()
                            .filter_map(|entry| {
                                serde_json::from_value::<
                                    agent_module_management::protocol::TimelineMessage,
                                >(entry.clone())
                                .ok()
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                // 同一迁移作用于显示时间线：内嵌图片落盘 → `/media/<id>`。
                for entry in timeline.iter_mut() {
                    if let Some(images) = entry.images.as_mut() {
                        for image in images.iter_mut() {
                            *image = echo_defs::media_store::spill_or_keep(image);
                        }
                    }
                }
                self.restore_timeline(timeline);
                return count;
            }
        }

        // v4 and older: migrate into the event log (compatibility read).
        if let Ok(migrated) = echo_session::legacy::migrate_v4_document(data) {
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
            if attributed {
                self.mark_dirty();
            }
            self.event_log.extend(all_events.clone());
            // Fall through to the identity/timeline restore below.
            let count = self.restore_identity_labels(&identity_values_of(&root));
            self.ensure_histories_for_events(&all_events);
            self.reproject_all();
            let mut restored_timeline = root["timeline"]
                .as_array()
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| {
                            serde_json::from_value::<
                                agent_module_management::protocol::TimelineMessage,
                            >(entry.clone())
                            .ok()
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for entry in restored_timeline.iter_mut() {
                if let Some(images) = entry.images.as_mut() {
                    for image in images.iter_mut() {
                        *image = echo_defs::media_store::spill_or_keep(image);
                    }
                }
            }
            self.restore_timeline(restored_timeline);
            return count;
        }
        self.deserialize_legacy(root)
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

    /// Restore a persisted file. Accepts v1 (per-session array), v2 (shared
    /// context object) and v3 (single trunk) formats; older formats migrate
    /// into the single trunk. Returns the number of restored identities.
    pub(super) fn deserialize_legacy(&self, root: serde_json::Value) -> usize {
        let (identity_values, persisted_trunk) = match &root {
            serde_json::Value::Array(sessions) => (sessions.as_slice(), None),
            serde_json::Value::Object(_) => {
                let Some(sessions) = root["identities"]
                    .as_array()
                    .or_else(|| root["sessions"].as_array())
                else {
                    return 0;
                };
                (
                    sessions.as_slice(),
                    root["trunk_history"]
                        .as_array()
                        .or_else(|| root["shared_history"].as_array()),
                )
            }
            _ => return 0,
        };
        let mut count = 0;
        // 最老格式（v1/v2，2026-09 多会话改造）：把消息转成**带归属**的事件
        // ——v1 的每身份历史归入各自会话；v2/v3 的共享 trunk 归入首个身份
        // （没有身份时归本地会话），此后一切走事件日志投影。
        let mut events: Vec<echo_session::SessionEvent> = Vec::new();
        let shared_owner: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
        for s in identity_values {
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
            let session = self.get_or_create(&key, String::new(), None);
            if let Some(last_active) = s["last_active"].as_i64() {
                session.last_active.store(last_active, Ordering::Relaxed);
            }
            if shared_owner.lock().ok().is_some_and(|o| o.is_none()) {
                *shared_owner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session.id.clone());
            }
            if persisted_trunk.is_none() {
                if let Some(history) = s["history"].as_array() {
                    events.extend(events_from_messages(
                        &deserialize_messages(history),
                        Some(session.id.clone()),
                    ));
                }
            }
            count += 1;
        }
        if let Some(shared) = persisted_trunk {
            let owner = shared_owner
                .lock()
                .ok()
                .and_then(|o| o.clone())
                .unwrap_or_else(|| SessionKey::local_tui().to_session_id());
            events.extend(events_from_messages(
                &deserialize_messages(shared),
                Some(owner),
            ));
        }
        if !events.is_empty() {
            self.event_log.extend(events.clone());
            self.ensure_histories_for_events(&events);
            self.reproject_all();
            self.mark_dirty();
        }

        // Restore the display timeline (v4+). Older files have no timeline —
        // an empty history is the correct fallback.
        let restored_timeline = root["timeline"]
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
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let timeline_entries = restored_timeline.len();
        {
            let mut timeline = self.timeline.lock();
            *timeline = restored_timeline;
        }
        // 整表替换（旧格式迁移）：置位探针重基线标志（见 `probe_timeline`）。
        self.timeline_rebaseline
            .store(true, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(
            entries = timeline_entries,
            "display timeline replaced from legacy document"
        );
        count
    }
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
