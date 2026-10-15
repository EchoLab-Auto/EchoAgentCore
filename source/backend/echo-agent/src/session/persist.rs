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
    }

    /// Load sessions from JSON file. Returns count of restored sessions.
    pub async fn load_from_file(&self) -> usize {
        let path = {
            self.persist_path
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        };
        let Some(ref path) = path else {
            return 0;
        };
        let data = match tokio::fs::read_to_string(path).await {
            Ok(d) => d,
            Err(_) => return 0,
        };
        self.deserialize(&data)
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
            if let Ok(header) = serde_json::from_value::<Option<echo_session::SessionHeader>>(
                root["header"].clone(),
            ) {
                *self
                    .header
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = header;
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
                let count = self.restore_identity_labels(&root);
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
                                serde_json::from_value::<crate::event::TimelineMessage>(
                                    entry.clone(),
                                )
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
            let count = self.restore_identity_labels(&root);
            self.ensure_histories_for_events(&all_events);
            self.reproject_all();
            let mut restored_timeline = root["timeline"]
                .as_array()
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| {
                            serde_json::from_value::<crate::event::TimelineMessage>(entry.clone())
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
    pub(super) fn restore_identity_labels(&self, root: &serde_json::Value) -> usize {
        let identity_values = match root {
            serde_json::Value::Array(sessions) => sessions.as_slice(),
            serde_json::Value::Object(_) => root["identities"]
                .as_array()
                .or_else(|| root["sessions"].as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            _ => &[],
        };
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
                        serde_json::from_value::<crate::event::TimelineMessage>(entry.clone()).ok()
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
