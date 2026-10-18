use super::*;
use proptest::prelude::*;

proptest! {
    /// Round-trip for any key whose fields don't contain the `:` separator
    /// (the wire format's only structural character).
    #[test]
    fn session_key_roundtrip(
        platform in "[a-z0-9_]{1,10}",
        scope in "dm|group|tui",
        scope_id in "[0-9]{0,12}",
        user_id in "[0-9]{1,15}",
    ) {
        let key = SessionKey { platform, scope, scope_id, user_id, account: None };
        let id = key.to_session_id();
        let parsed = SessionKey::parse(&id).expect("roundtrip parse");
        prop_assert_eq!(parsed, key);
    }

    /// Arbitrary strings must never panic in parse; malformed input
    /// either parses to a key whose serialisation round-trips or is None.
    #[test]
    fn session_key_parse_never_panics(
        input in ".*",
    ) {
        if let Some(key) = SessionKey::parse(&input) {
            prop_assert_eq!(SessionKey::parse(&key.to_session_id()), Some(key));
        }
    }
}

/// P3-2b：迁移导入把事件追加进事实来源日志并物化历史句柄。
#[test]
fn import_session_events_appends_to_fact_log() {
    let store = TrunkStore::new(100_000);
    let sid = "local:tui::local_user";
    let event = echo_session::SessionEvent::UserMessage(echo_session::UserMessage {
        content: "hello".into(),
        timestamp: 1,
        message_sequence: Some(1),
        source: None,
        images: Vec::new(),
        session: Some(sid.into()),
    });
    store.import_session_events(sid, vec![event.clone()]);
    assert_eq!(store.event_log(), vec![event]);
    assert!(store.history_for(sid).is_some());
}

// ── federation Phase 0：node:// 命名空间 ──

#[test]
fn parse_strips_node_prefix_for_local_handling() {
    let key = SessionKey::parse("node://node-abc/qq:group:987:123").expect("qualified id parses");
    assert_eq!(key.platform, "qq");
    assert_eq!(key.scope, "group");
    assert_eq!(key.scope_id, "987");
    assert_eq!(key.user_id, "123");
    // 本机 id 完全不受新前缀逻辑影响
    let plain = SessionKey::parse("qq:group:987:123").unwrap();
    assert_eq!(key, plain);
}

#[test]
fn parse_qualified_preserves_node_attribution() {
    let (node, key) = SessionKey::parse_qualified("node://node-abc/local:workspace:ws1:local_user")
        .expect("qualified parse");
    assert_eq!(node.as_deref(), Some("node-abc"));
    assert_eq!(key.scope, "workspace");

    let (node, key) = SessionKey::parse_qualified("local:tui::local_user").unwrap();
    assert!(node.is_none());
    assert_eq!(key, SessionKey::local_tui());
}

/// 旧版事件归因：hook 内容推导 + 粘滞继承 + 兜底。
#[test]
fn legacy_attribution_splits_interleaved_sessions() {
    use echo_session::event::{AssistantMessage, ToolCallEvent, ToolResultEvent, UserMessage};
    let qq_hook = "<qq_message_hook>\n{\n  \"platform\": \"qq\",\n  \"adapter\": \"qq\",\n  \"channel\": {\"type\": \"private\"},\n  \"sender\": {\"user_id\": \"1828980067\"}\n}\n</qq_message_hook>".to_string();
    let group_hook = "<qq_message_hook>\n{\n  \"platform\": \"qq\",\n  \"adapter\": \"qq\",\n  \"channel\": {\"type\": \"group\", \"group_id\": \"999\"},\n  \"sender\": {\"user_id\": \"42\"}\n}\n</qq_message_hook>".to_string();
    let backend_hook =
        "<backend_message_hook>\n{\"content\": \"你好\"}\n</backend_message_hook>".to_string();
    let mut events = vec![
        echo_session::SessionEvent::UserMessage(UserMessage {
            content: qq_hook.clone(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
            session: None,
        }),
        echo_session::SessionEvent::ToolCall(ToolCallEvent {
            id: "c1".into(),
            name: "send_private_msg".into(),
            arguments: "{}".into(),
            session: None,
            started_at_ms: None,
        }),
        echo_session::SessionEvent::ToolResult(ToolResultEvent {
            tool_call_id: "c1".into(),
            result: "sent".into(),
            images: vec![],
            session: None,
            elapsed_ms: None,
        }),
        echo_session::SessionEvent::AssistantMessage(AssistantMessage {
            content: "已回复。".into(),
            reasoning_content: None,
            tool_calls: vec![],
            session: None,
        }),
        echo_session::SessionEvent::UserMessage(UserMessage {
            content: backend_hook.clone(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
            session: None,
        }),
        echo_session::SessionEvent::AssistantMessage(AssistantMessage {
            content: "你好，本地".into(),
            reasoning_content: None,
            tool_calls: vec![],
            session: None,
        }),
        echo_session::SessionEvent::UserMessage(UserMessage {
            content: group_hook.clone(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
            session: None,
        }),
    ];
    assert!(attribute_legacy_events(&mut events), "changed");
    let sessions: Vec<Option<&str>> = events.iter().map(|e| e.session()).collect();
    assert_eq!(
        sessions,
        vec![
            Some("qq:dm::1828980067"),
            Some("qq:dm::1828980067"),
            Some("qq:dm::1828980067"),
            Some("qq:dm::1828980067"),
            Some("local:tui::local_user"),
            Some("local:tui::local_user"),
            Some("qq:group:999:42"),
        ]
    );
    // 幂等：再跑一遍无改动。
    assert!(!attribute_legacy_events(&mut events));
}

/// 未归因的前导事件挂到首个已知会话；完全无可推导时挂本地。
#[test]
fn legacy_attribution_falls_back_to_first_known_session() {
    use echo_session::event::{AssistantMessage, UserMessage};
    let mut events = vec![
        echo_session::SessionEvent::AssistantMessage(AssistantMessage {
            content: "孤儿应答".into(),
            reasoning_content: None,
            tool_calls: vec![],
            session: None,
        }),
        echo_session::SessionEvent::UserMessage(UserMessage {
            content: "<qq_message_hook>\n{\"platform\":\"qq\",\"channel\":{\"type\":\"private\"},\"sender\":{\"user_id\":\"7\"}}\n</qq_message_hook>".into(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
            session: None,
        }),
    ];
    assert!(attribute_legacy_events(&mut events));
    assert_eq!(
        events[0].session(),
        Some("qq:dm::7"),
        "leading orphan joins first known"
    );

    // 完全无法推导：统一挂本地 TUI 会话。
    let mut plain = vec![echo_session::SessionEvent::UserMessage(UserMessage {
        content: "普通文本（无 hook 标记）".into(),
        timestamp: 0,
        message_sequence: None,
        source: None,
        images: vec![],
        session: None,
    })];
    assert!(attribute_legacy_events(&mut plain));
    assert_eq!(plain[0].session(), Some("local:tui::local_user"));
}

/// 同一事件日志交错写入两个会话：各自快照互不可见。
#[tokio::test]
async fn interleaved_events_project_per_session() {
    use echo_session::event::{AssistantMessage, UserMessage};
    let store = TrunkStore::new(10_000);
    let local = store.get_or_create(&SessionKey::local_tui(), "local".into(), None);
    let qq_key = SessionKey::parse("qq:dm::9").unwrap();
    let qq = store.get_or_create(&qq_key, "u".into(), None);
    let pairs = [
        (local.id.clone(), "本地一"),
        (qq.id.clone(), "qq 一"),
        (local.id.clone(), "本地二"),
        (qq.id.clone(), "qq 二"),
    ];
    for (session_id, content) in pairs {
        store.append_event(echo_session::SessionEvent::UserMessage(UserMessage {
            content: content.into(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
            session: Some(session_id.clone()),
        }));
        store.append_event(echo_session::SessionEvent::AssistantMessage(
            AssistantMessage {
                content: format!("{content} 的回复"),
                reasoning_content: None,
                tool_calls: vec![],
                session: Some(session_id),
            },
        ));
    }
    let local_hist = store.snapshot_for(&local.id).await;
    let qq_hist = store.snapshot_for(&qq.id).await;
    assert_eq!(local_hist.len(), 4, "local sees only its 4 messages");
    assert_eq!(qq_hist.len(), 4, "qq sees only its 4 messages");
    assert!(local_hist.iter().all(|m| !m.content.starts_with("qq")));
    assert!(qq_hist.iter().all(|m| !m.content.starts_with("本地")));
}

#[test]
fn session_key_format() {
    let key = SessionKey {
        platform: "qq".into(),
        scope: "dm".into(),
        scope_id: "".into(),
        user_id: "123".into(),
        account: None,
    };
    assert_eq!(key.to_session_id(), "qq:dm::123");
}

#[test]
fn session_key_parse() {
    let key = SessionKey::parse("qq:group:456:789").unwrap();
    assert_eq!(key.platform, "qq");
    assert_eq!(key.scope, "group");
    assert_eq!(key.scope_id, "456");
    assert_eq!(key.user_id, "789");
}

#[test]
fn session_key_parse_legacy() {
    let key = SessionKey::parse("user_12345").unwrap();
    assert_eq!(key.platform, "local");
    assert_eq!(key.scope, "tui");
    assert_eq!(key.user_id, "12345");
}

#[test]
fn session_key_local_tui() {
    let key = SessionKey::local_tui();
    assert_eq!(key.to_session_id(), "local:tui::local_user");
}

#[test]
fn session_key_local_workspace_roundtrip() {
    let key = SessionKey::local_workspace("echo-agent");
    assert_eq!(key.to_session_id(), "local:workspace:echo-agent:local_user");
    // parse 往返：scope=workspace / scope_id=工作区 id
    let parsed = SessionKey::parse("local:workspace:echo-agent:local_user").unwrap();
    assert_eq!(parsed.platform, "local");
    assert_eq!(parsed.scope, "workspace");
    assert_eq!(parsed.scope_id, "echo-agent");
    assert_eq!(parsed.user_id, "local_user");
    assert_eq!(parsed.to_session_id(), key.to_session_id());
}

#[test]
fn session_key_parse_malformed() {
    assert!(SessionKey::parse("abc").is_none());
    assert!(SessionKey::parse("a:b").is_none());
}

/// 工作区通道不随闲置回收（列表行常驻），普通身份照常回收。
#[tokio::test]
async fn evict_idle_keeps_workspace_channels() {
    let store = TrunkStore::new(1000);
    let channel = store.ensure_workspace_channel("proj", "Proj");
    let plain = store.get_or_create(&SessionKey::parse("qq:dm::111").unwrap(), "a".into(), None);
    let past = chrono::Utc::now().timestamp() - (IDENTITY_IDLE_TTL_MS / 1000) * 2;
    channel
        .last_active
        .store(past, std::sync::atomic::Ordering::Relaxed);
    plain
        .last_active
        .store(past, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(store.len(), 2);
    store.evict_idle();
    assert!(
        store.get("local:workspace:proj:local_user").is_some(),
        "workspace channel survives eviction"
    );
    assert!(store.get("qq:dm::111").is_none(), "stale identity evicted");
}

#[tokio::test]
async fn trunk_get_or_create_deduplicates_identities() {
    let store = TrunkStore::new(1000);
    let key = SessionKey::parse("qq:dm::111").unwrap();
    let s = store.get_or_create(&key, "alice".into(), None);
    assert_eq!(s.id, "qq:dm::111");
    assert_eq!(store.len(), 1);

    // Same key returns existing identity.
    let s2 = store.get_or_create(&key, "bob".into(), None);
    assert_eq!(store.len(), 1);
    assert_eq!(s2.nickname, "alice"); // original nickname preserved
}

#[tokio::test]
async fn sessions_have_independent_histories_and_shared_turn_lock() {
    let store = TrunkStore::new(1000);
    let local = store.get_or_create(&SessionKey::local_tui(), "local".into(), None);
    let qq = store.get_or_create(
        &SessionKey::parse("qq:dm::123").unwrap(),
        "alice".into(),
        None,
    );

    // 多会话（2026-09）：上下文相互独立，只有写入锁共享。
    assert!(!Arc::ptr_eq(&local.history, &qq.history));
    assert!(Arc::ptr_eq(&local.turn_lock, &qq.turn_lock));

    // 归属 local 的事件只进 local 的上下文，qq 不受影响。
    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            content: "local only".into(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
            session: Some("local:tui::local_user".into()),
        },
    ));
    assert_eq!(local.history.lock()[0].content, "local only");
    assert!(qq.history.lock().is_empty(), "qq context untouched");
    assert_eq!(store.trunk_len(), 1, "aggregate over sessions");
}

#[tokio::test]
async fn session_info_has_platform_fields() {
    let store = TrunkStore::new(1000);
    let key = SessionKey::parse("qq:group:789:456").unwrap();
    let s = store.get_or_create(&key, "bob".into(), Some("TestGroup".into()));
    let info = s.info("last msg".into());
    assert_eq!(info.platform, "qq");
    assert_eq!(info.scope, "group");
    assert_eq!(info.user_id, "456");
    assert_eq!(info.group_name, Some("TestGroup".into()));
    assert_eq!(info.last_message, "last msg");
}

// ── Persistence ─────────────────────────────────────────────────────────

fn temp_sessions_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("echo-trunk-{name}-{}.json", std::process::id()))
}

#[tokio::test]
async fn save_and_load_roundtrip_restores_trunk_and_identities() {
    let path = temp_sessions_path("roundtrip");
    let _ = std::fs::remove_file(&path);

    let store = TrunkStore::new(1000);
    store.set_persist_path(&path);
    let key = SessionKey::parse("qq:group:789:456").unwrap();
    let _s = store.get_or_create(&key, "bob".into(), Some("TestGroup".into()));
    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: Some(key.to_session_id()),
            content: "你好".into(),
            timestamp: 1700000000,
            message_sequence: None,
            source: None,
            images: vec![],
        },
    ));
    store.append_event(echo_session::SessionEvent::AssistantMessage(
        echo_session::event::AssistantMessage {
            session: Some(key.to_session_id()),
            content: "回复".into(),
            reasoning_content: None,
            tool_calls: vec![],
        },
    ));
    store.save_now().await;
    assert!(path.exists(), "trunk file written");

    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(root["version"], 6, "v6 multi-session format");
    assert!(
        root.get("trunk_histories").is_none(),
        "per-session projections are derived from the event log, not persisted"
    );
    assert_eq!(root["events"].as_array().unwrap().len(), 2);
    assert!(root["identities"]
        .as_array()
        .unwrap()
        .iter()
        .all(|identity| identity.get("history").is_none()));

    // A fresh store restores everything.
    let store2 = TrunkStore::new(1000);
    store2.set_persist_path(&path);
    let restored = store2.load_from_file().await;
    assert_eq!(restored, 1, "one identity restored");
    let s2 = store2.get_or_create(&key, "bob".into(), Some("TestGroup".into()));
    let hist = s2.history.lock().clone();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0].content, "你好");
    assert_eq!(hist[1].content, "回复");

    let _ = std::fs::remove_file(&path);
}

// ── 媒体落盘迁移（2026-09-24）──
//
// 这两个用例改的是进程级环境变量 `ECHO_MEDIA_DIR`，必须串行执行。
// tokio Mutex：其中一个用例是 async（持锁跨 await），std 锁会触发
// clippy::await_holding_lock；两者语义一致（仍按进程级环境变量串行）。
static MEDIA_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[test]
fn spill_event_media_migrates_inline_images_to_refs() {
    let _serial = MEDIA_ENV_LOCK.blocking_lock();
    use echo_session::event::{ToolResultEvent, UserMessage};
    let dir = std::env::temp_dir().join(format!(
        "echo-session-media-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::env::set_var("ECHO_MEDIA_DIR", &dir);

    // 超过落盘阈值的 data URI（~5.4KB base64）
    let payload = echo_defs::media_store::tests_support_base64(vec![7u8; 4096]);
    let data_uri = format!("data:image/png;base64,{payload}");
    // 真实形态：content 是 hook JSON（内嵌 images 的 data URI）+ images 字段
    let content = format!(
        "<qq_message_hook>\n{{\"content\":\"看图\",\"images\":[\"{data_uri}\"]}}\n</qq_message_hook>"
    );
    let mut events = vec![
        echo_session::SessionEvent::UserMessage(UserMessage {
            content,
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![data_uri.clone(), "https://example.com/a.png".into()],
            session: Some("local:tui::local_user".into()),
        }),
        echo_session::SessionEvent::ToolResult(ToolResultEvent {
            tool_call_id: "c1".into(),
            result: "ok".into(),
            images: vec![data_uri.clone()],
            session: Some("local:tui::local_user".into()),
            elapsed_ms: None,
        }),
    ];
    let changed = spill_event_media(&mut events);
    std::env::remove_var("ECHO_MEDIA_DIR");

    assert!(changed, "migration reports change");
    let (content, images) = match &events[0] {
        echo_session::SessionEvent::UserMessage(m) => (&m.content, &m.images),
        other => panic!("unexpected: {other:?}"),
    };
    // content 里的内嵌 data URI 也被改写为引用（QQ hook JSON 场景）
    assert!(content.contains("/media/"), "content: {content}");
    assert!(!content.contains("data:image"), "content payload gone");
    assert!(images[0].starts_with("/media/"), "{:?}", images[0]);
    assert!(images[0].ends_with(".png"));
    // 远端 URL 原样保留
    assert_eq!(images[1], "https://example.com/a.png");
    let tool_images = match &events[1] {
        echo_session::SessionEvent::ToolResult(r) => &r.images,
        other => panic!("unexpected: {other:?}"),
    };
    assert!(tool_images[0].starts_with("/media/"));

    // 幂等：再跑一次没有改动
    std::env::set_var("ECHO_MEDIA_DIR", &dir);
    assert!(!spill_event_media(&mut events));
    std::env::remove_var("ECHO_MEDIA_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn persisted_refs_inline_on_projection_for_model() {
    let _serial = MEDIA_ENV_LOCK.lock().await;
    // 事件日志里存引用（轻量）；投影（模型上下文）出口还原为 data URI。
    let dir = std::env::temp_dir().join(format!(
        "echo-session-inline-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::env::set_var("ECHO_MEDIA_DIR", &dir);
    let bytes = vec![9u8; 4096];
    let id = echo_defs::media_store::save_image_bytes(&bytes, "image/png").unwrap();
    let reference = echo_defs::media_store::media_ref(&id);

    let store = TrunkStore::new(10_000);
    let _session = store.get_or_create(&SessionKey::local_tui(), "local".into(), None);
    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            content: "看图".into(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![reference.clone()],
            session: Some("local:tui::local_user".into()),
        },
    ));
    let snapshot = store.snapshot_for("local:tui::local_user").await;
    std::env::remove_var("ECHO_MEDIA_DIR");

    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].images.len(), 1, "image survives projection");
    assert!(
        snapshot[0].images[0].starts_with("data:image/png;base64,"),
        "refs inline into data URI for the model: {}",
        snapshot[0].images[0]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn persisted_history_restores_per_session() {
    let path = temp_sessions_path("shared-roundtrip");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(1000);
    store.set_persist_path(&path);
    let _local = store.get_or_create(&SessionKey::local_tui(), "local".into(), None);
    let qq_key = SessionKey::parse("qq:dm::123").unwrap();
    let _qq = store.get_or_create(&qq_key, "alice".into(), None);
    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: Some("local:tui::local_user".into()),
            content: "from tui".into(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
        },
    ));
    store.append_event(echo_session::SessionEvent::AssistantMessage(
        echo_session::event::AssistantMessage {
            session: Some("local:tui::local_user".into()),
            content: "tui reply".into(),
            reasoning_content: None,
            tool_calls: vec![],
        },
    ));
    store.save_now().await;

    let restored = TrunkStore::new(1000);
    restored.set_persist_path(&path);
    assert_eq!(restored.load_from_file().await, 2);
    let local = restored.get("local:tui::local_user").unwrap();
    let qq = restored.get(&qq_key.to_session_id()).unwrap();
    // 上下文按会话隔离：local 有 2 条，qq 为空。
    assert!(!Arc::ptr_eq(&local.history, &qq.history));
    assert_eq!(local.history.lock().len(), 2);
    assert!(qq.history.lock().is_empty(), "qq context stays empty");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn v1_array_migrates_distinct_histories_per_session() {
    let store = TrunkStore::new(1000);
    let restored = store.deserialize(
        r#"[
            {"id":"local:tui::local_user","platform":"local","scope":"tui","scope_id":"","user_id":"local_user","nickname":"local","last_active":1,"history":[{"role":"user","content":"local"}]},
            {"id":"qq:dm::123","platform":"qq","scope":"dm","scope_id":"","user_id":"123","nickname":"alice","last_active":2,"history":[{"role":"user","content":"qq"}]}
        ]"#,
    );

    assert_eq!(restored, 2);
    // 多会话（2026-09）：v1 的每身份历史归入各自会话的上下文。
    let local = store
        .history_for("local:tui::local_user")
        .expect("local history");
    assert_eq!(local.try_lock().unwrap()[0].content, "local");
    let qq = store.history_for("qq:dm::123").expect("qq history");
    assert_eq!(qq.try_lock().unwrap()[0].content, "qq");
    assert_eq!(store.trunk_len(), 2, "aggregate over both sessions");
}

#[test]
fn v2_shared_object_migrates_into_first_identity() {
    let store = TrunkStore::new(1000);
    let restored = store.deserialize(
        r#"{
            "version": 2,
            "shared_context": true,
            "shared_history": [{"role":"user","content":"shared"}],
            "sessions": [
                {"id":"local:tui::local_user","platform":"local","scope":"tui","scope_id":"","user_id":"local_user","nickname":"local","last_active":1},
                {"id":"qq:dm::123","platform":"qq","scope":"dm","scope_id":"","user_id":"123","nickname":"alice","last_active":2}
            ]
        }"#,
    );

    assert_eq!(restored, 2);
    // v2 共享上下文没有归属信息：归入首个身份（local）。
    let history = store
        .history_for("local:tui::local_user")
        .expect("local history");
    assert_eq!(history.try_lock().unwrap()[0].content, "shared");
    assert!(store
        .history_for("qq:dm::123")
        .map(|h| h.try_lock().unwrap().is_empty())
        .unwrap_or(true));
    assert_eq!(store.len(), 2, "both identities restored");
}

#[test]
fn timeline_push_caps_at_max_and_keeps_newest() {
    let store = TrunkStore::new(1000);
    for i in 0..(TRUNK_TIMELINE_MAX + 50) {
        store.push_timeline(crate::event::TimelineMessage {
            kind: "user".into(),
            content: format!("msg-{i}"),
            session_id: "local:tui::local_user".into(),
            time: i as i64,
            source: None,
            reasoning: None,
            tool: None,
            images: None,
            seq: 0,
        });
    }
    let timeline = store.timeline_snapshot();
    assert_eq!(timeline.len(), TRUNK_TIMELINE_MAX, "capped at the max");
    assert_eq!(
        timeline.first().unwrap().content,
        "msg-50",
        "oldest dropped"
    );
    assert_eq!(
        timeline.last().unwrap().content,
        format!("msg-{}", TRUNK_TIMELINE_MAX + 49)
    );
}

#[tokio::test]
async fn clear_history_wipes_log_trunk_timeline_and_disk() {
    let path = temp_sessions_path("clear-history");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(1000);
    store.set_persist_path(&path);
    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: Some("local:tui::local_user".into()),
            content: "记住我".into(),
            timestamp: 1700000000,
            message_sequence: None,
            source: None,
            images: vec![],
        },
    ));
    store.push_timeline(crate::event::TimelineMessage {
        kind: "user".into(),
        content: "记住我".into(),
        session_id: "local:tui::local_user".into(),
        time: 1700000000,
        source: None,
        reasoning: None,
        tool: None,
        images: None,
        seq: 0,
    });
    store.save_now().await;
    assert_eq!(store.trunk_len(), 1);
    assert_eq!(store.timeline_snapshot().len(), 1);

    store.clear_history().await;

    assert_eq!(store.trunk_len(), 0, "trunk projection cleared");
    assert!(store.event_log().is_empty(), "event log cleared");
    assert!(store.timeline_snapshot().is_empty(), "timeline cleared");
    // The persisted file must already reflect the wipe.
    let restored = TrunkStore::new(1000);
    restored.set_persist_path(&path);
    restored.load_from_file().await;
    assert_eq!(restored.trunk_len(), 0, "cleared history survives restart");
    assert!(restored.timeline_snapshot().is_empty());
    let _ = std::fs::remove_file(&path);
}

/// 会话级清理（2026-10 用户报告回归）：清一个会话，**另一会话完好**。
///
/// 背景：面板「清理历史记忆」此前映射到全量 `clear_history()`——
/// 同一人格下其他会话（QQ 群/私聊、其他 workspace 会话）的记忆被一并
/// 抹掉。本测试钉住 `clear_session` 的精确性：事件日志、上下文投影、
/// 显示时间线三面都只清目标会话，且磁盘持久化一致。
#[tokio::test]
async fn clear_session_only_wipes_target_and_keeps_siblings() {
    let path = temp_sessions_path("clear-session");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(1000);
    store.set_persist_path(&path);

    let user_event = |session: &str, content: &str, ts: i64| {
        echo_session::SessionEvent::UserMessage(echo_session::event::UserMessage {
            session: Some(session.into()),
            content: content.into(),
            timestamp: ts,
            message_sequence: None,
            source: None,
            images: vec![],
        })
    };
    let timeline_msg = |session: &str, content: &str, ts: i64| crate::event::TimelineMessage {
        kind: "user".into(),
        content: content.into(),
        session_id: session.into(),
        time: ts,
        source: None,
        reasoning: None,
        tool: None,
        images: None,
        seq: 0,
    };

    // 会话 A（待清理）与 B（必须完好）各写事件 + 时间线条目。
    store.append_event(user_event(
        "local:workspace:proj:alice",
        "A-记忆",
        1700000001,
    ));
    store.append_event(user_event("qq:group:123:456", "B-记忆", 1700000002));
    store.push_timeline(timeline_msg(
        "local:workspace:proj:alice",
        "A-线",
        1700000001,
    ));
    store.push_timeline(timeline_msg("qq:group:123:456", "B-线", 1700000002));
    store.save_now().await;
    assert_eq!(store.event_log().len(), 2);

    store.clear_session("local:workspace:proj:alice").await;

    // ① 事件日志：只剩 B。
    let log = store.event_log();
    assert_eq!(log.len(), 1, "只应保留另一会话的事件: {log:?}");
    assert_eq!(log[0].session(), Some("qq:group:123:456"));
    // ② 上下文投影：A 的投影条目已删、B 的保留。
    let a_cleared = match store.history_for("local:workspace:proj:alice") {
        Some(h) => h.lock().is_empty(),
        None => true,
    };
    assert!(a_cleared, "目标会话投影应清空");
    // ③ 时间线：只剩 B 的条目。
    let timeline = store.timeline_snapshot();
    assert_eq!(timeline.len(), 1, "时间线只应保留另一会话条目");
    assert_eq!(timeline[0].session_id, "qq:group:123:456");

    // ④ 持久化：重启加载后同样只有 B（清理落盘）。
    let restored = TrunkStore::new(1000);
    restored.set_persist_path(&path);
    restored.load_from_file().await;
    let restored_log = restored.event_log();
    assert_eq!(restored_log.len(), 1, "磁盘应只存另一会话");
    assert!(
        restored
            .timeline_snapshot()
            .iter()
            .all(|m| m.session_id == "qq:group:123:456"),
        "重启后时间线也不含被清会话"
    );
    let _ = std::fs::remove_file(&path);
}

/// 2026-10 上下文审计 🔴 回归：会话级清理后，身份对象持有的**同一
/// Arc** 必须同步清空——否则 `get_or_create` 命中的旧 `Session.history`
/// 把已清理的内容带进下一次 LLM 请求（清理"复活"），且新投影写进新
/// Arc 与旧 Arc 永久脱节（新消息"失忆"）。
#[tokio::test]
async fn clear_session_empties_the_shared_history_arc() {
    let store = TrunkStore::new(1000);
    let key = SessionKey::parse("local:workspace:proj:alice").unwrap();
    // 身份 + 事件 + 投影就位；`session` 持有 history Arc 的一份拷贝。
    let session = store.get_or_create(&key, "alice".into(), None);
    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: Some(session.id.clone()),
            content: "被清理的记忆".into(),
            timestamp: 1700000001,
            message_sequence: None,
            source: None,
            images: vec![],
        },
    ));
    assert!(!session.history.lock().is_empty(), "投影已物化");

    store.clear_session(&session.id).await;

    // 身份持有的旧 Arc 已原地清空（复活路径切断）。
    assert!(
        session.history.lock().is_empty(),
        "clear_session 必须清空身份持有的共享 Arc"
    );
    // 新事件写入后：投影重新物化且只含新内容（新旧 Arc 未脱节）。
    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: Some(session.id.clone()),
            content: "清理后的新消息".into(),
            timestamp: 1700000002,
            message_sequence: None,
            source: None,
            images: vec![],
        },
    ));
    let history = store.history_for(&session.id).expect("投影重建");
    let contents: Vec<String> = history.lock().iter().map(|m| m.content.clone()).collect();
    assert!(
        contents.iter().all(|c| !c.contains("被清理的记忆")),
        "旧内容不得复活: {contents:?}"
    );
    assert!(
        contents.iter().any(|c| c.contains("清理后的新消息")),
        "新消息应进投影: {contents:?}"
    );
}

/// 2026-10-15 用户报障回归（uuapi 400 "field messages is required"）：
/// 会话级清理后，**身份对象**持有的 Arc 必须继续接收新投影——否则该
/// 会话此后每个请求都只带系统提示词（严格端点 400 / 宽松端点回复退化
/// 成自我介绍式问候），直到重启。
///
/// 上面 `clear_session_empties_the_shared_history_arc` 只断言了
/// `history_for`（投影缓存映射里的新 Arc），身份 Arc 脱节时该映射照样
/// 有内容——必须直接断言 `session.history`（`process_message` 读的
/// 就是它）。
#[tokio::test]
async fn clear_session_keeps_identity_arc_connected_after_new_events() {
    let store = TrunkStore::new(1000);
    let key = SessionKey::parse("local:workspace:proj:bob").unwrap();
    let session = store.get_or_create(&key, "bob".into(), None);
    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: Some(session.id.clone()),
            content: "旧内容".into(),
            timestamp: 1700000101,
            message_sequence: None,
            source: None,
            images: vec![],
        },
    ));
    assert!(!session.history.lock().is_empty(), "投影已物化");

    store.clear_session(&session.id).await;
    assert!(session.history.lock().is_empty(), "清理后身份 Arc 为空");

    // 清理后的新事件：身份 Arc 必须同步收敛（不得与新建投影脱节）。
    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: Some(session.id.clone()),
            content: "清理后的新消息".into(),
            timestamp: 1700000102,
            message_sequence: None,
            source: None,
            images: vec![],
        },
    ));
    let contents: Vec<String> = session
        .history
        .lock()
        .iter()
        .map(|m| m.content.clone())
        .collect();
    assert!(
        contents.iter().any(|c| c.contains("清理后的新消息")),
        "身份 Arc 必须随新投影收敛（否则会话永久失忆）: {contents:?}"
    );
    assert!(
        contents.iter().all(|c| !c.contains("旧内容")),
        "旧内容不得复活: {contents:?}"
    );
}

/// 旧版无归属事件（session=None）在会话级清理中**不被误删**（保守语义）。
#[tokio::test]
async fn clear_session_keeps_legacy_unattributed_events() {
    let path = temp_sessions_path("clear-session-legacy");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(1000);
    store.set_persist_path(&path);

    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: Some("s1".into()),
            content: "归属 s1".into(),
            timestamp: 1,
            message_sequence: None,
            source: None,
            images: vec![],
        },
    ));
    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: None, // 旧版事件：无归属
            content: "无归属".into(),
            timestamp: 2,
            message_sequence: None,
            source: None,
            images: vec![],
        },
    ));

    store.clear_session("s1").await;

    let log = store.event_log();
    assert_eq!(log.len(), 1, "仅删该会话事件，旧版无归属事件保留: {log:?}");
    assert_eq!(log[0].session(), None);
    let _ = std::fs::remove_file(&path);
}

/// 时间线写路径计数（探针基础设施）：push / cap-drain / 会话级删除三个
/// 单调计数与真实变化一致——[`TrunkStore::probe_timeline`] 的对账恒等式
/// 依赖它们。
#[tokio::test]
async fn timeline_probe_counters_track_writes() {
    use std::sync::atomic::Ordering::Relaxed;
    let store = TrunkStore::new(1000);
    let message = |session: &str| crate::event::TimelineMessage {
        kind: "user".into(),
        content: "x".into(),
        session_id: session.into(),
        time: 1,
        source: None,
        reasoning: None,
        tool: None,
        images: None,
        seq: 0,
    };
    for i in 0..(TRUNK_TIMELINE_MAX + 3) {
        store.push_timeline(message(if i % 2 == 0 { "sA" } else { "sB" }));
    }
    assert_eq!(
        store.timeline_pushed.load(Relaxed),
        (TRUNK_TIMELINE_MAX + 3) as u64
    );
    assert_eq!(store.timeline_drained.load(Relaxed), 3, "cap-drain counted");
    store.clear_session("sA").await;
    assert!(
        store.timeline_removed.load(Relaxed) > 0,
        "session-scoped timeline removal counted"
    );
    // 冒烟：探针可执行、不 panic（是否告警由日志观察）。
    store.probe_timeline();
}

/// 探针对账恒等式：期望条目数 = 上次 + Δpush − Δdrain − Δremove；
/// 条目"绕过写路径"地减少时恒等式失衡（探针据此告警，2026-10 时间线
/// 无声消失事故定位基建）。
#[test]
fn timeline_probe_accounting_identity() {
    let last = TimelineProbeSample {
        count: 10,
        head_seq: 5,
        pushed: 10,
        drained: 0,
        removed: 0,
    };
    let now = TimelineProbeSample {
        count: 10,
        head_seq: 5,
        pushed: 13,
        drained: 1,
        removed: 2,
    };
    assert_eq!(last.expected_count_after(&now), 10, "10 + 3 − 1 − 2");
    let vanished = TimelineProbeSample { count: 3, ..now };
    assert_ne!(
        last.expected_count_after(&vanished),
        3,
        "无声减少必须被对账检出"
    );
}

#[tokio::test]
async fn timeline_roundtrips_through_the_persisted_file() {
    let path = temp_sessions_path("timeline-roundtrip");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(1000);
    store.set_persist_path(&path);
    store.push_timeline(crate::event::TimelineMessage {
        kind: "user".into(),
        content: "你好".into(),
        session_id: "qq:dm::123".into(),
        time: 1700000000,
        source: Some(crate::event::TimelineSource {
            instance_name: "qq".into(),
            platform: "qq".into(),
            user_id: "123".into(),
            user_name: "alice".into(),
            channel: "direct".into(),
            group_name: None,
            received_at_ms: 1700000000123,
            message_sequence: 7,
        }),
        reasoning: None,
        tool: None,
        images: None,
        seq: 0,
    });
    store.push_timeline(crate::event::TimelineMessage {
        kind: "backend".into(),
        content: "回复".into(),
        session_id: "qq:dm::123".into(),
        time: 1700000001,
        source: None,
        reasoning: Some(vec!["先分析".into()]),
        tool: None,
        images: None,
        seq: 0,
    });
    store.save_now().await;

    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(root["version"], 6, "timeline persisted as v6");
    assert_eq!(root["timeline"].as_array().unwrap().len(), 2);

    let restored = TrunkStore::new(1000);
    restored.set_persist_path(&path);
    restored.load_from_file().await;
    let timeline = restored.timeline_snapshot();
    assert_eq!(timeline.len(), 2);
    assert_eq!(timeline[0].kind, "user");
    assert_eq!(timeline[0].content, "你好");
    assert_eq!(
        timeline[0].source.as_ref().unwrap().message_sequence,
        7,
        "source provenance restored"
    );
    assert_eq!(
        timeline[1].reasoning.as_deref(),
        Some(vec!["先分析".to_string()].as_slice()),
        "reasoning restored"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn v3_file_without_timeline_loads_with_empty_timeline() {
    let store = TrunkStore::new(1000);
    let restored = store.deserialize(
        r#"{
            "version": 3,
            "trunk_history": [{"role":"user","content":"hi"}],
            "identities": []
        }"#,
    );
    assert_eq!(restored, 0);
    assert!(store.timeline_snapshot().is_empty(), "no timeline → empty");
}

fn push_user(store: &TrunkStore, content: &str) {
    store.push_timeline(crate::event::TimelineMessage {
        kind: "user".into(),
        content: content.into(),
        session_id: "local:tui::local_user".into(),
        time: 0,
        source: None,
        reasoning: None,
        tool: None,
        images: None,
        seq: 0,
    });
}

#[test]
fn snapshot_since_filters_by_entry_seq() {
    let store = TrunkStore::new(1000);
    push_user(&store, "a");
    push_user(&store, "b");
    push_user(&store, "c");
    // seq: a=1, b=2, c=3
    let (delta, seq) = store.timeline_snapshot_since(1).expect("window intact");
    assert_eq!(seq, 3);
    assert_eq!(
        delta.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
        vec!["b", "c"]
    );
    // 空增量：只推进游标，不返回条目（前端不得据此清空聊天）。
    let (delta, seq) = store.timeline_snapshot_since(3).expect("no-op");
    assert!(delta.is_empty());
    assert_eq!(seq, 3);
}

/// 2026-10 架构评审回归：timeline 锁改为同步短锁后，并发 push 与快照
/// 交错时既不丢条目（旧 `try_lock` 失败即丢），也不在 async 中 sleep。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_push_and_snapshot_lose_nothing() {
    let store = Arc::new(TrunkStore::new(1000));
    let mut tasks = Vec::new();
    for writer in 0..8 {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            for i in 0..50 {
                push_user(&store, &format!("w{writer}-{i}"));
                // 交错读：快照与增量快照都必须拿到结果（不再有 None 降级）。
                let _ = store.timeline_snapshot();
                tokio::task::yield_now().await;
            }
        }));
    }
    for task in tasks {
        task.await.expect("writer task");
    }
    let timeline = store.timeline_snapshot();
    assert_eq!(timeline.len(), 400, "8×50 条并发写入一条都不能丢");
    assert_eq!(store.timeline_seq(), 400);
    let mut seqs: Vec<u64> = timeline.iter().map(|m| m.seq).collect();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), 400, "seq 唯一");
}

#[test]
fn snapshot_since_redelivers_in_place_updated_entry() {
    let store = TrunkStore::new(1000);
    push_user(&store, "a");
    // 模拟 update_timeline_tool 的就地更新：bump 计数器并写回条目 seq。
    let new_seq = store.bump_timeline_seq();
    {
        let mut timeline = store.timeline_mut();
        timeline[0].seq = new_seq;
        timeline[0].content = "a-updated".into();
    }
    let (delta, seq) = store.timeline_snapshot_since(1).expect("window intact");
    assert_eq!(seq, 2);
    assert_eq!(delta.len(), 1, "updated entry re-delivered");
    assert_eq!(delta[0].content, "a-updated");
}

#[test]
fn snapshot_since_returns_none_on_gap_or_future_cursor() {
    let store = TrunkStore::new(1000);
    for i in 0..(TRUNK_TIMELINE_MAX + 10) {
        push_user(&store, &format!("msg-{i}"));
    }
    // 头部已滚出：最老条目 seq = 11，since_seq = 1 的窗口不完整 → None。
    assert!(
        store.timeline_snapshot_since(1).is_none(),
        "gap → full fallback"
    );
    // 恰好贴着现存最老条目（seq 11 的前一个）仍可增量。
    let cursor = store.timeline_snapshot().first().unwrap().seq - 1;
    assert!(store.timeline_snapshot_since(cursor).is_some());
    // 游标超前于当前序号（来自 core 重启前）→ None。
    let current = store.timeline_seq();
    assert!(store.timeline_snapshot_since(current + 1).is_none());
}

#[test]
fn restore_marks_dangling_running_tool_as_interrupted() {
    // 进程被杀（如自更新重启）时 ToolResult 永远不会到达；恢复时必须
    // 把 output=null 的僵尸 running 条目标注为中断并分配新 seq，
    // 否则前端会一直显示"运行中"。
    let store = TrunkStore::new(1000);
    store.deserialize(
        r#"{
            "version": 5,
            "events": [],
            "timeline": [
                {"kind":"tool","content":"bash","session_id":"s","time":1,
                 "tool":{"name":"bash","input":"x","output":null,"failed":false}},
                {"kind":"tool","content":"read_file","session_id":"s","time":2,
                 "tool":{"name":"read_file","input":"y","output":"ok","failed":false}}
            ]
        }"#,
    );
    let timeline = store.timeline_snapshot();
    let zombie = timeline[0].tool.as_ref().unwrap();
    assert!(zombie.failed, "dangling entry marked failed");
    assert!(
        zombie
            .output
            .as_deref()
            .unwrap_or_default()
            .contains("已中断"),
        "interrupted note attached"
    );
    assert!(timeline[0].seq > 0, "interrupted entry gets a fresh seq");
    // 已完成的条目不受影响。
    let done = timeline[1].tool.as_ref().unwrap();
    assert!(!done.failed);
    assert_eq!(done.output.as_deref(), Some("ok"));
    assert_eq!(timeline[1].seq, 0);
    assert_eq!(
        store.timeline_seq(),
        1,
        "seq counter past the patched entry"
    );
}

#[tokio::test]
async fn timeline_seq_survives_save_load() {
    let path = temp_sessions_path("timeline-seq-rehydrate");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(1000);
    store.set_persist_path(&path);
    push_user(&store, "a");
    push_user(&store, "b");
    store.save_now().await;

    let restored = TrunkStore::new(1000);
    restored.set_persist_path(&path);
    restored.load_from_file().await;
    assert_eq!(
        restored.timeline_seq(),
        2,
        "seq counter rehydrated from disk"
    );
    // 重启后继续 push，序号连续不回退。
    push_user(&restored, "c");
    assert_eq!(restored.timeline_seq(), 3);
    let (delta, _) = restored.timeline_snapshot_since(2).expect("intact");
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0].content, "c");
    let _ = std::fs::remove_file(&path);
}

/// BASELINE (Phase 0): the v4 persistence format dropped `tool_calls` and
/// `tool_call_id` on the tool round-trip — a tool message serialized then
/// deserialized degraded to plain text. Phase 3 replaced the format with
/// the event-sourced log (`echo_session`), whose event round-trip
/// preserves tool structure (see `echo_session::event` tests). This
/// legacy serializer is retained only for v4-compatible writes and
/// documents the old lossy behaviour.
#[test]
fn baseline_tool_message_structure_is_lost_on_roundtrip() {
    let message = ChatMessage {
        role: crate::llm::ChatRole::Assistant,
        content: "".into(),
        reasoning_content: None,
        tool_calls: Some(vec![crate::llm::ToolCall {
            id: "call_1".into(),
            name: "send_private_msg".into(),
            arguments: r#"{"user_id":123,"content":"hi"}"#.into(),
        }]),
        tool_call_id: None,
        images: vec![],
    };
    let serialized = serialize_messages(std::slice::from_ref(&message));
    let restored = deserialize_messages(&serialized);
    assert_eq!(restored.len(), 1);
    assert!(
        restored[0].tool_calls.is_none(),
        "baseline: tool_calls are dropped by the v4 format (Phase 3 target: preserved)"
    );
}

#[tokio::test]
async fn load_from_missing_file_returns_zero() {
    let store = TrunkStore::new(1000);
    store.set_persist_path(temp_sessions_path("missing"));
    assert_eq!(store.load_from_file().await, 0);
}

#[tokio::test]
async fn load_handles_malformed_entries_leniently() {
    let path = temp_sessions_path("malformed");
    std::fs::write(
        &path,
        r#"[
            {"id": "ok", "platform": "qq", "scope": "dm", "scope_id": "", "user_id": "1", "nickname": "a", "history": []},
            {"id": "bad", "platform": "qq", "scope": "dm"},
            "not an object"
        ]"#,
    )
    .unwrap();
    let store = TrunkStore::new(1000);
    store.set_persist_path(&path);
    let restored = store.load_from_file().await;
    // Entries with an id are restored leniently (missing fields fall back
    // to defaults); non-object entries are skipped.
    assert_eq!(restored, 2);
    // The partially-populated entry falls back to defaults for the
    // missing fields only (scope_id/user_id → empty/0).
    let fallback = store.get("qq:dm::0").expect("lenient default identity");
    assert_eq!(fallback.session_key.scope_id, "");
    assert_eq!(fallback.session_key.user_id, "0");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_get_or_create_yields_single_identity() {
    let store = TrunkStore::new(1000);
    let key = SessionKey::parse("qq:dm::999").unwrap();
    let key2 = key.clone();

    // 32 concurrent callers racing on the same key.
    let mut handles = Vec::new();
    for _ in 0..32 {
        let store = store.clone();
        let key = key.clone();
        handles.push(tokio::spawn(async move {
            store.get_or_create(&key, "alice".into(), None)
        }));
    }
    for h in handles {
        let _ = h.await.unwrap();
    }
    assert_eq!(store.len(), 1, "single identity for concurrent callers");
    let _ = key2;
}

#[test]
fn oversized_newest_message_is_truncated_to_fit_the_budget() {
    // 600 CJK chars ≈ 600 tokens, budget 100 → must be truncated.
    let mut history = vec![ChatMessage::user("x".repeat(600))];
    trim_by_tokens(&mut history, 100);
    assert_eq!(history.len(), 1);
    let tokens = crate::llm::estimate_message_tokens(&history[0]);
    assert!(
        tokens <= 100,
        "truncated message must fit the budget, got {tokens} tokens"
    );
    assert!(
        history[0].content.contains("内容过长已截断"),
        "truncation marker expected"
    );
}

#[test]
fn short_histories_are_never_trimmed() {
    let mut history = vec![ChatMessage::user("你好"), ChatMessage::assistant("好的")];
    trim_by_tokens(&mut history, 100);
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].content, "你好");
}

#[test]
fn structured_hook_keeps_its_sequence_after_truncation() {
    let hook = r#"<qq_message_hook>
{"channel":{"type":"private"},"message_sequence":42,"content":"请分析这份超长文档"}</qq_message_hook>"#;
    let mut history = vec![ChatMessage::user(hook)];
    trim_by_tokens(&mut history, 60);
    let remaining = &history[0].content;
    // Sequence must survive the truncation (it lives at the head).
    assert!(remaining.starts_with("<qq_message_hook>"));
    assert!(remaining.contains("\"message_sequence\":42"));
}

#[test]
fn evict_idle_drops_stale_identity_but_keeps_context() {
    let store = TrunkStore::new(1000);
    let key = SessionKey::parse("qq:dm::111").unwrap();
    let stale = store.get_or_create(&key, "alice".into(), None);
    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: Some(key.to_session_id()),
            content: "kept".into(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
        },
    ));
    // Force last_active far in the past (2× TTL).
    let past = chrono::Utc::now().timestamp() - (IDENTITY_IDLE_TTL_MS / 1000) * 2;
    stale
        .last_active
        .store(past, std::sync::atomic::Ordering::Relaxed);

    let key2 = SessionKey::parse("qq:dm::222").unwrap();
    let recent = store.get_or_create(&key2, "bob".into(), None);
    recent.touch(); // now

    assert_eq!(store.len(), 2);
    store.evict_idle();
    assert_eq!(store.len(), 1);
    assert!(
        store.get(&key2.to_session_id()).is_some(),
        "recent identity survives"
    );
    // 身份被回收，但上下文（投影缓存）保留（历史不随身份标签删除）。
    assert_eq!(store.trunk_len(), 1, "context survives eviction");
    assert!(store.history_for(&key.to_session_id()).is_some());
}

// ── 压缩计划 / 归档快照 / 落地（2026-09 LLM 摘要改造）──

fn ev_user(session: &str, content: &str) -> echo_session::SessionEvent {
    echo_session::SessionEvent::UserMessage(echo_session::event::UserMessage {
        session: Some(session.into()),
        content: content.into(),
        timestamp: 1700000000,
        message_sequence: None,
        source: None,
        images: vec![],
    })
}

fn ev_tool(session: &str, id: &str) -> echo_session::SessionEvent {
    echo_session::SessionEvent::ToolCall(echo_session::event::ToolCallEvent {
        id: id.into(),
        name: "bash".into(),
        arguments: "{}".into(),
        session: Some(session.into()),
        started_at_ms: None,
    })
}

#[tokio::test]
async fn compact_preview_groups_by_session_and_counts() {
    let store = TrunkStore::new(100_000);
    // 会话 A：5 条（将被压前 3 条），会话 B：1 条（不动）。
    let a = store.ensure_workspace_channel("proj", "Proj");
    let a_id = a.id.clone();
    let b_id = "local:tui::local_user".to_string();
    for i in 0..5 {
        store.append_event(ev_user(&a_id, &format!("a{i}")));
        if i < 3 {
            store.append_event(ev_tool(&a_id, &format!("c{i}")));
        }
    }
    store.append_event(ev_user(&b_id, "b0"));

    // keep=3 → A 组 8 条事件压 5 留 3；B 组只有 1 条不动。
    let preview = store.compact_preview(3).expect("preview");
    assert_eq!(preview.groups.len(), 1, "only session A compacts");
    assert_eq!(preview.total_events, 9);
    let g = &preview.groups[0];
    assert_eq!(g.session.as_deref(), Some(a_id.as_str()));
    assert_eq!(g.replaced_count, 5);
    assert_eq!(g.users, 3, "a0,a1,a2 are the first three user messages");
    assert_eq!(g.tools, 2);

    // keep=10 → 两组都不足（A=8, B=1 ≤ 11）→ 无需压缩。
    assert!(store.compact_preview(10).is_err());
}

#[tokio::test]
async fn compact_preview_rejects_short_log() {
    let store = TrunkStore::new(1000);
    store.append_event(ev_user("local:tui::local_user", "唯一一条"));
    let err = store.compact_preview(40).unwrap_err();
    assert!(err.contains("历史不足"), "got: {err}");
}

#[tokio::test]
async fn apply_compaction_installs_body_summary_and_archive_path() {
    let path = temp_sessions_path("apply-compact");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(100_000);
    store.set_persist_path(&path);
    let sid = "local:tui::local_user";
    for i in 0..6 {
        store.append_event(ev_user(sid, &format!("m{i}")));
    }
    store.save_now().await;

    let preview = store.compact_preview(2).expect("preview");
    assert_eq!(preview.total_replaced, 4);
    let summaries = vec![CompactionSummary {
        session: Some(sid.into()),
        summary: "用户先问了 m0、m1，随后推进到 m2、m3。".into(),
        replaced_count: preview.total_replaced,
    }];
    let applied = store
        .apply_compaction(2, &summaries, Some("/tmp/archives/x-precompact.json"))
        .await
        .expect("apply");
    assert_eq!(applied.total_replaced, 4);
    assert_eq!(applied.groups, 1);

    // 事件日志：Compaction（正文无前缀 + 归档路径）+ 尾部 2 条。
    let events = store.event_log();
    assert_eq!(events.len(), 3, "1 compaction + 2 kept");
    match &events[0] {
        echo_session::SessionEvent::Compaction(c) => {
            assert_eq!(c.replaced_count, 4);
            assert!(
                !c.summary.contains("[历史摘要]"),
                "body must not embed the marker"
            );
            assert_eq!(
                c.archive.as_deref(),
                Some("/tmp/archives/x-precompact.json")
            );
            assert_eq!(c.session.as_deref(), Some(sid));
        }
        other => panic!("expected compaction, got {other:?}"),
    }

    // 投影：唯一前缀 + 保留的尾部。
    let history = store.snapshot_for(sid).await;
    assert_eq!(history.len(), 3);
    assert!(
        history[0].content.starts_with("[历史摘要] "),
        "got: {}",
        history[0].content
    );
    assert!(!history[0].content.contains("[历史摘要] [历史摘要]"));
    assert!(history[1].content.contains("m4"));
    assert!(history[2].content.contains("m5"));

    // 立即持久化：磁盘文件里能找到归档路径。
    let disk = std::fs::read_to_string(&path).unwrap();
    assert!(disk.contains("x-precompact.json"));
    assert!(disk.contains("m0") || disk.contains("replaced_count"));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn apply_compaction_keeps_groups_without_a_summary() {
    let store = TrunkStore::new(100_000);
    let a_id = store.ensure_workspace_channel("proj", "Proj").id.clone();
    let b_id = "local:tui::local_user".to_string();
    for i in 0..6 {
        store.append_event(ev_user(&a_id, &format!("a{i}")));
    }
    for i in 0..6 {
        store.append_event(ev_user(&b_id, &format!("b{i}")));
    }
    let preview = store.compact_preview(2).expect("preview");
    assert_eq!(preview.groups.len(), 2);

    // 只为 A 组准备摘要：B 组必须原样保留（不能换出无摘要的 Compaction）。
    let summaries = vec![CompactionSummary {
        session: Some(a_id.clone()),
        summary: "A 组摘要".into(),
        replaced_count: 4,
    }];
    let applied = store
        .apply_compaction(2, &summaries, None)
        .await
        .expect("apply");
    assert_eq!(applied.groups, 1, "only the summarized group compacts");
    assert_eq!(applied.total_replaced, 4);

    let events = store.event_log();
    let compactions = events
        .iter()
        .filter(|e| matches!(e, echo_session::SessionEvent::Compaction(_)))
        .count();
    assert_eq!(compactions, 1);
    // B 组全部 6 条仍在。
    let b_left = events
        .iter()
        .filter(|e| e.session() == Some(b_id.as_str()))
        .count();
    assert_eq!(b_left, 6);
}

#[tokio::test]
async fn archive_snapshot_writes_memory_state_with_reason() {
    let path = temp_sessions_path("archive-snapshot");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(1000);
    store.set_persist_path(&path);
    store.append_event(ev_user("local:tui::local_user", "未落盘的最新消息"));
    // 故意不 save_now()：快照必须来自内存，而不是落后的磁盘文件。

    let archive = store.archive_snapshot("precompact").await.expect("archive");
    assert!(archive.contains("precompact"), "reason in name: {archive}");
    let data = std::fs::read_to_string(&archive).unwrap();
    assert!(
        data.contains("未落盘的最新消息"),
        "snapshot must serialize the in-memory state"
    );
    assert!(data.contains("\"events\""));
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn archive_history_copies_file_clears_and_marks_timeline() {
    let path = temp_sessions_path("archive-history");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(1000);
    store.set_persist_path(&path);
    store.append_event(ev_user("local:tui::local_user", "归档前的消息"));
    store.save_now().await;
    assert!(path.exists());

    let archive = store.archive_history().await.expect("archive path");
    // 归档文件可读且含原消息
    let data = std::fs::read_to_string(&archive).unwrap();
    assert!(data.contains("归档前的消息"));
    assert!(std::path::Path::new(&archive).starts_with(path.parent().unwrap().join("archives")));
    // 归档返回后（clear + 重新 push 之后）内存 timeline 即带系统提示行
    {
        let timeline = store.timeline.lock();
        assert!(
            timeline.iter().any(|m| m.content.contains("历史已归档")),
            "timeline carries the archive notice, got: {:?}",
            timeline.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
    }
    // 历史已清空：重载为 0 条事件，但提示行随 clear 后的落盘持久化——
    // 磁盘上的 timeline 保留归档提示（用户下次打开能看到"历史已归档"）。
    let restored = store.load_from_file().await;
    assert_eq!(restored, 0, "history cleared after archive");
    {
        let timeline = store.timeline.lock();
        assert!(
            timeline.iter().any(|m| m.content.contains("历史已归档")),
            "archive notice persists across reload"
        );
    }
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn archive_history_without_persist_path_errors() {
    let store = TrunkStore::new(1000);
    store.append_event(ev_user("local:tui::local_user", "内存消息"));
    let err = store.archive_history().await.expect_err("no persist path");
    assert!(err.contains("没有可归档"), "{err}");
}

#[tokio::test]
async fn archive_history_without_existing_file_errors() {
    let path = temp_sessions_path("archive-missing");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(1000);
    store.set_persist_path(&path); // 路径已配但文件尚未写过
    let err = store.archive_history().await.expect_err("no file yet");
    assert!(err.contains("没有可归档"), "{err}");
}

#[test]
fn prune_archives_keeps_newest_and_ignores_foreign_stems() {
    let dir = std::env::temp_dir().join(format!("echo-prune-{}", std::process::id()));
    let archives = dir.join("archives");
    std::fs::create_dir_all(&archives).unwrap();
    // 6 份本 stem 归档（mtime 递增）+ 1 份别的 stem + 1 个非 json
    for i in 0..6 {
        let f = archives.join(format!("echo-sessions-x-2026100{i}-000000-precompact.json"));
        std::fs::write(&f, "{}").unwrap();
        let mtime = filetime::FileTime::from_unix_time(1_700_000_000 + i as i64, 0);
        filetime::set_file_mtime(&f, mtime).unwrap();
    }
    std::fs::write(archives.join("other-stem-20261001-000000.json"), "{}").unwrap();
    std::fs::write(archives.join("echo-sessions-x-note.txt"), "x").unwrap();

    TrunkStore::prune_archives(&archives, "echo-sessions-x", 2);

    let left: Vec<String> = std::fs::read_dir(&archives)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    // 本 stem 只剩最新 2 份
    let own: Vec<&String> = left
        .iter()
        .filter(|n| n.starts_with("echo-sessions-x-") && n.ends_with(".json"))
        .collect();
    assert_eq!(own.len(), 2, "left: {left:?}");
    assert!(left.contains(&"echo-sessions-x-20261004-000000-precompact.json".to_string()));
    assert!(left.contains(&"echo-sessions-x-20261005-000000-precompact.json".to_string()));
    // 别的 stem 与非 json 不受影响
    assert!(left.contains(&"other-stem-20261001-000000.json".to_string()));
    assert!(left.contains(&"echo-sessions-x-note.txt".to_string()));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn prune_archives_does_not_swallow_longer_stems() {
    // 2026-10 审计回归：前缀匹配锚定时间戳段——stem `echo-sessions-x`
    // 不得统计/删除 `echo-sessions-x-y` 的归档（旧 starts_with 会命中）。
    let dir = std::env::temp_dir().join(format!("echo-prune-anchor-{}", std::process::id()));
    let archives = dir.join("archives");
    std::fs::create_dir_all(&archives).unwrap();
    let long_stem = archives.join("echo-sessions-x-y-20261001-000000.json");
    std::fs::write(&long_stem, "{}").unwrap();
    filetime::set_file_mtime(
        &long_stem,
        filetime::FileTime::from_unix_time(1_700_000_000, 0),
    )
    .unwrap();
    for (i, ts) in ["20261002-000000", "20261003-000000"].iter().enumerate() {
        let f = archives.join(format!("echo-sessions-x-{ts}.json"));
        std::fs::write(&f, "{}").unwrap();
        filetime::set_file_mtime(
            &f,
            filetime::FileTime::from_unix_time(1_700_000_100 + i as i64, 0),
        )
        .unwrap();
    }

    TrunkStore::prune_archives(&archives, "echo-sessions-x", 1);

    let left: Vec<String> = std::fs::read_dir(&archives)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    // 本 stem 只保留最新 1 份；更长 stem 的文件不受影响。
    assert!(
        left.contains(&"echo-sessions-x-20261003-000000.json".to_string()),
        "newest own kept: {left:?}"
    );
    assert!(
        !left.contains(&"echo-sessions-x-20261002-000000.json".to_string()),
        "older own pruned: {left:?}"
    );
    assert!(
        left.contains(&"echo-sessions-x-y-20261001-000000.json".to_string()),
        "longer stem untouched: {left:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 2026-10 性能回归：单会话 append 后的重投影**只看本会话**的事件。
///
/// 旧实现先 `event_log.log()` 深克隆整份日志（含所有会话）再过滤，等于每次
/// append 都付 O(全部会话) 的字符串拷贝。现在走
/// `EventLog::events_for_session`。这个测试锁定"别把热路径改回全量克隆"的
/// 行为面：多会话下写一个会话，另一个会话的投影不受影响、且本会话内容正确。
#[test]
fn append_reprojects_only_the_owning_session() {
    let store = TrunkStore::new(10_000);
    let a = store.get_or_create(&SessionKey::local_tui(), "user-a".into(), None);
    let b = store.get_or_create(
        &SessionKey {
            platform: "local".into(),
            scope: "workspace".into(),
            scope_id: "other".into(),
            user_id: "local_user".into(),
            account: None,
        },
        "user-b".into(),
        None,
    );

    for i in 0..3 {
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some(a.id.clone()),
                content: format!("A{i}"),
                timestamp: 1_700_000_000 + i,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some(b.id.clone()),
                content: format!("B{i}"),
                timestamp: 1_700_000_000 + i,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
    }

    // 两个会话的投影各自独立、互不串显
    let a_contents: Vec<String> = a.history.lock().iter().map(|m| m.content.clone()).collect();
    let b_contents: Vec<String> = b.history.lock().iter().map(|m| m.content.clone()).collect();
    assert_eq!(a_contents, vec!["A0", "A1", "A2"], "A 只含自己的消息");
    assert_eq!(b_contents, vec!["B0", "B1", "B2"], "B 只含自己的消息");
}

/// 推理（Reasoning）事件经重投影进入**显示历史**、但不进模型上下文。
///
/// 这条把 2026-10 根治的三处接线串起来验：落日志 → 派生含推理 →
/// 模型投影跳过。
#[test]
fn reasoning_events_reach_display_history_but_not_model_context() {
    let store = TrunkStore::new(10_000);
    let session = store.get_or_create(&SessionKey::local_tui(), "user".into(), None);
    let sid = session.id.clone();

    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: Some(sid.clone()),
            content: "问题".into(),
            timestamp: 1_700_000_000,
            message_sequence: None,
            source: None,
            images: vec![],
        },
    ));
    store.append_event(echo_session::SessionEvent::Reasoning(
        echo_session::event::ReasoningEvent {
            content: "内部思考".into(),
            timestamp: 1_700_000_001,
            session: Some(sid.clone()),
        },
    ));
    store.append_event(echo_session::SessionEvent::AssistantMessage(
        echo_session::event::AssistantMessage {
            content: "回答".into(),
            reasoning_content: None,
            tool_calls: vec![],
            session: Some(sid.clone()),
        },
    ));

    // 显示历史：含推理条目
    let (messages, total, _) = store.display_history_for_session(&sid, 100);
    let kinds: Vec<&str> = messages.iter().map(|m| m.kind.as_str()).collect();
    assert_eq!(total, 3);
    assert_eq!(kinds, vec!["user", "reasoning", "backend"], "{messages:?}");

    // 模型上下文：不含推理正文（只有 user + assistant）
    let ctx: Vec<String> = session
        .history
        .lock()
        .iter()
        .map(|m| m.content.clone())
        .collect();
    assert_eq!(ctx, vec!["问题", "回答"], "推理不得进入模型上下文: {ctx:?}");
}

/// 2026-10 上下文审计 🔴 回归：token 预算热更——收窄立即重投影裁剪
/// 生效；getter 反映新值（此前预算构造时固化，保存后要重启才生效）。
#[tokio::test]
async fn set_memory_limit_tokens_reprojects_immediately() {
    let store = TrunkStore::new(10_000);
    let key = SessionKey::local_tui();
    let session = store.get_or_create(&key, "user".into(), None);
    for i in 0..10 {
        store.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                session: Some(session.id.clone()),
                content: format!("消息{i}-{}", "长".repeat(200)),
                timestamp: 1700000000 + i,
                message_sequence: None,
                source: None,
                images: vec![],
            },
        ));
    }
    let before = session.history.lock().len();
    assert_eq!(before, 10, "宽预算下全部保留");

    // 收窄到约 1 条消息的预算：立即重投影生效。
    store.set_memory_limit_tokens(200);
    assert_eq!(store.memory_limit_tokens(), 200);
    let after = session.history.lock().len();
    assert!(after < before, "收窄后投影应被裁剪: {after} !< {before}");

    // 放宽回大预算：下一条事件后投影按新预算容纳（旧事件不可恢复）。
    store.set_memory_limit_tokens(10_000);
    store.append_event(echo_session::SessionEvent::UserMessage(
        echo_session::event::UserMessage {
            session: Some(session.id.clone()),
            content: "新消息".into(),
            timestamp: 1700000100,
            message_sequence: None,
            source: None,
            images: vec![],
        },
    ));
    let contents: Vec<String> = session
        .history
        .lock()
        .iter()
        .map(|m| m.content.clone())
        .collect();
    assert!(contents.iter().any(|c| c.contains("新消息")));
}

/// 2026-10 上下文审计回归：归档名带毫秒段——同秒两次归档不得同名
/// 覆盖；新旧格式（含 -precompact）都被茎匹配识别。
#[test]
fn archive_name_with_millis_never_collides_within_a_second() {
    // 新格式（带毫秒）茎匹配
    for name in [
        "echo-sessions-x-20261013-074421-123.json",
        "echo-sessions-x-20261013-074421-123-precompact.json",
        // 旧格式（无毫秒）向后兼容
        "echo-sessions-x-20261013-074421.json",
        "echo-sessions-x-20261013-074421-precompact.json",
    ] {
        assert!(
            TrunkStore::is_own_archive_name(name, "echo-sessions-x"),
            "recognized: {name}"
        );
    }
    // 非本 stem / 非归档名不误识
    for name in [
        "echo-sessions-x-y-20261013-074421-123.json",
        "echo-sessions-x-note.txt",
        "echo-sessions-x-2026101X-074421-123.json",
        "echo-sessions-x-20261013-07442112.json", // 时间戳连写无分隔
    ] {
        assert!(
            !TrunkStore::is_own_archive_name(name, "echo-sessions-x"),
            "rejected: {name}"
        );
    }
}

/// 2026-10 上下文审计回归：淘汰按文件名内嵌时间戳排序——mtime 失真
/// （文件被复制/恢复）不影响"保留最新"。
#[test]
fn prune_archives_orders_by_filename_timestamp_not_mtime() {
    let dir = std::env::temp_dir().join(format!("echo-prune-name-{}", std::process::id()));
    let archives = dir.join("archives");
    std::fs::create_dir_all(&archives).unwrap();
    // 文件名时间戳递增，但 mtime 故意**倒置**（最新文件 mtime 最旧）。
    for i in 0..4 {
        let f = archives.join(format!("echo-sessions-x-2026101{i}-000000-001.json"));
        std::fs::write(&f, "{}").unwrap();
        let mtime = filetime::FileTime::from_unix_time(1_700_000_000 - i as i64 * 100, 0);
        filetime::set_file_mtime(&f, mtime).unwrap();
    }

    TrunkStore::prune_archives(&archives, "echo-sessions-x", 2);

    let left: Vec<String> = std::fs::read_dir(&archives)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        left.contains(&"echo-sessions-x-20261012-000000-001.json".to_string())
            && left.contains(&"echo-sessions-x-20261013-000000-001.json".to_string()),
        "filename-newest kept despite inverted mtimes: {left:?}"
    );
    assert_eq!(left.len(), 2, "left: {left:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn apply_compaction_keeps_events_arriving_after_preview() {
    let path = temp_sessions_path("compact-drift");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(100_000);
    store.set_persist_path(&path);
    let sid = "local:tui::local_user";
    // 预览口径：10 条事件，keep_recent=3 → 压缩前 7 条
    for i in 0..10 {
        store.append_event(ev_user(sid, &format!("消息 {i}")));
    }
    let preview = store.compact_preview(3).expect("preview");
    assert_eq!(preview.total_replaced, 7);
    // 摘要生成窗口内新到 2 条（漂移）——旧实现会把它们挤出保留尾
    store.append_event(ev_user(sid, "窗口期新消息 A"));
    store.append_event(ev_user(sid, "窗口期新消息 B"));

    let summaries = vec![crate::session::CompactionSummary {
        session: Some(sid.to_string()),
        summary: "[历史摘要] 前 7 条的摘要".into(),
        replaced_count: preview.total_replaced,
    }];
    let applied = store
        .apply_compaction(3, &summaries, None)
        .await
        .expect("apply");

    let events = store.event_log.log();
    // 修复后的切片语义：替换 = **预览切片**（前 7 条），窗口期新到的
    // A/B 与旧保留尾（消息 7/8/9）全部留在保留尾——不再有"按应用时点
    // len-keep 重算"把旧尾最老 a 条挤进无摘要前缀的静默丢档。
    let compaction = events
        .iter()
        .find_map(|e| match e {
            echo_session::SessionEvent::Compaction(c) => Some(c),
            _ => None,
        })
        .expect("compaction event");
    assert_eq!(
        compaction.replaced_count, 7,
        "replaced slice must be the preview count, not runtime drift"
    );
    let texts: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            echo_session::SessionEvent::UserMessage(m) => Some(m.content.clone()),
            _ => None,
        })
        .collect();
    for expected in [
        "消息 7",
        "消息 8",
        "消息 9",
        "窗口期新消息 A",
        "窗口期新消息 B",
    ] {
        assert!(
            texts.iter().any(|t| t.contains(expected)),
            "{expected} must survive: {texts:?}"
        );
    }
    assert_eq!(texts.len(), 5, "5 kept user messages: {texts:?}");
    assert_eq!(applied.total_replaced, 7);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn archive_history_uses_memory_snapshot_not_stale_disk_file() {
    // 2026-10 审计回归：归档源必须是**内存快照**——磁盘最多落后
    // 30s 保存周期，窗口内新到的事件此前"未归档即被清"（归档与
    // 日志双失，仅剩内存里的残影）。
    let path = temp_sessions_path("archive-memory");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(100_000);
    store.set_persist_path(&path);
    store.append_event(ev_user("local:tui::local_user", "已落盘的消息"));
    store.save_now().await;
    // 保存之后又到一条（窗口期事件，磁盘上没有）。
    store.append_event(ev_user("local:tui::local_user", "未落盘的窗口期消息"));

    let archive = store.archive_history().await.expect("archive");
    let data = std::fs::read_to_string(&archive).unwrap();
    assert!(
        data.contains("未落盘的窗口期消息"),
        "archive must contain the in-memory event disk has not saved yet"
    );
    assert!(data.contains("已落盘的消息"));
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn archive_session_clears_only_the_target_session() {
    // 2026-10 审计回归：会话级归档——A 被归档并清空，B 原样保留；
    // 归档文件是整档快照（恢复时可直接作为会话文件手工换回）。
    let path = temp_sessions_path("archive-scoped");
    let _ = std::fs::remove_file(&path);
    let store = TrunkStore::new(100_000);
    store.set_persist_path(&path);
    let a_id = store.ensure_workspace_channel("proj", "Proj").id.clone();
    let b_id = "local:tui::local_user".to_string();
    for i in 0..3 {
        store.append_event(ev_user(&a_id, &format!("A{i}")));
    }
    for i in 0..3 {
        store.append_event(ev_user(&b_id, &format!("B{i}")));
    }

    let archive = store.archive_session(&a_id).await.expect("archive");
    let data = std::fs::read_to_string(&archive).unwrap();
    assert!(
        data.contains("A0") && data.contains("B0"),
        "snapshot is the whole-file archive"
    );

    // 只剩 B 的 3 条事件；A 被清空。
    let events = store.event_log();
    let a_left = events
        .iter()
        .filter(|e| e.session() == Some(a_id.as_str()))
        .count();
    let b_left = events
        .iter()
        .filter(|e| e.session() == Some(b_id.as_str()))
        .count();
    assert_eq!(a_left, 0, "target session cleared");
    assert_eq!(b_left, 3, "other sessions untouched");
    // timeline 里 A 的条目被清、B 保留；提示行走统一入列路径（带 seq）。
    {
        let timeline = store.timeline.lock();
        assert!(
            timeline.iter().all(|m| m.session_id != a_id),
            "timeline for the archived session is cleared"
        );
        assert!(timeline
            .iter()
            .any(|m| m.content.contains("会话历史已归档")));
        assert!(
            timeline.last().is_some_and(|m| m.seq > 0),
            "archive notice went through the counted push path (seq assigned)"
        );
    }
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn compact_history_is_serialized_by_compact_lock() {
    use std::sync::Arc;
    let store = Arc::new(TrunkStore::new(1000));
    let guard1 = store.compact_lock().await;
    // 第二把锁必须等——try_lock 立即失败证明互斥
    assert!(
        store.compact_lock.try_lock().is_err(),
        "compact_lock must be exclusive while held"
    );
    drop(guard1);
    let _guard2 = store.compact_lock().await;
}

/// 真实会话文件离线体检（人工触发，不进 CI）：
///
/// ```text
/// ECHO_COMPACTION_LIVE_FILE=~/.config/echo-agent-core/echo-sessions-EchoCode.json \
///   cargo test -p echo-agent --lib live_compaction_on_real_session_file -- --ignored
/// ```
///
/// 在**文件副本**上走完整链路（载入 → 预览 → 归档 → 应用），验证真实
/// 数据的分组/统计与归档产物；绝不触碰原文件。
#[tokio::test]
#[ignore = "manual: needs a real session file via ECHO_COMPACTION_LIVE_FILE"]
async fn live_compaction_on_real_session_file() {
    let source = std::env::var("ECHO_COMPACTION_LIVE_FILE")
        .expect("set ECHO_COMPACTION_LIVE_FILE to a sessions JSON file");
    let dir = std::env::temp_dir().join(format!("echo-live-compact-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let copy = dir.join("sessions.json");
    std::fs::copy(&source, &copy).expect("copy session file");

    let store = TrunkStore::new(800_000);
    store.set_persist_path(&copy);
    let restored = store.load_from_file().await;
    assert!(restored > 0, "sessions restored");
    let preview = store.compact_preview(40).expect("preview");
    eprintln!(
        "预览：共 {} 条事件 → 压缩 {} 条，{} 组",
        preview.total_events,
        preview.total_replaced,
        preview.groups.len()
    );
    for group in &preview.groups {
        eprintln!(
            "  - {}: {} 条（{} 用户 / {} 工具调用）",
            group.session.as_deref().unwrap_or("<legacy>"),
            group.replaced_count,
            group.users,
            group.tools
        );
    }
    let archive = store
        .archive_snapshot("precompact-live")
        .await
        .expect("archive");
    eprintln!("归档：{archive}");
    assert!(std::path::Path::new(&archive).exists());

    // 以规则回退文案落地（离线无 LLM；真实链路里这里是模型摘要）。
    let summaries: Vec<CompactionSummary> = preview
        .groups
        .iter()
        .map(|g| CompactionSummary {
            session: g.session.clone(),
            summary: format!("[offline-体检] {} 条事件", g.replaced_count),
            replaced_count: g.replaced_count,
        })
        .collect();
    let applied = store
        .apply_compaction(40, &summaries, Some(&archive))
        .await
        .expect("apply");
    eprintln!(
        "已应用：{} 条 → {} 组（保留 {}）",
        applied.total_replaced, applied.groups, applied.kept
    );
    // 每个受影响会话的投影第一条应是唯一前缀的摘要。
    for group in &preview.groups {
        let Some(sid) = &group.session else { continue };
        let history = store.snapshot_for(sid).await;
        assert!(
            history[0].content.starts_with("[历史摘要] "),
            "session {sid} projection should lead with the summary"
        );
        assert!(!history[0].content.contains("[历史摘要] [历史摘要]"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn archive_snapshot_without_persistence_errors() {
    let store = TrunkStore::new(1000);
    let err = store.archive_snapshot("precompact").await.unwrap_err();
    assert!(err.contains("未启用持久化"), "got: {err}");
}
