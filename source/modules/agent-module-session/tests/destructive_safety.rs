//! Safety regressions for destructive session operations; synthetic data only.
use agent_module_session::TrunkStore;
use echo_session::{CompactionSummary, SessionEvent, UserMessage};
use std::future::Future;
use std::path::PathBuf;
use std::task::{Context, Poll, Waker};

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "echo-session-safety-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn user(text: &str) -> SessionEvent {
    SessionEvent::UserMessage(UserMessage {
        content: text.into(),
        timestamp: 1,
        message_sequence: None,
        source: None,
        images: vec![],
        files: vec![],
        session: Some("s".into()),
    })
}

#[tokio::test]
async fn archive_snapshot_waits_for_compaction_lock_before_reading_history() {
    let dir = Scratch::new("archive-boundary");
    let store = TrunkStore::new(100_000);
    store.set_persist_path(dir.0.join("sessions.json"));
    store.append_event(user("before"));
    let guard = store.compact_lock().await;
    let mut archive = Box::pin(store.archive_session("s"));
    // Old implementation wrote a snapshot here, then waited to clear: a new
    // event would be cleared without ever entering that snapshot.
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(archive.as_mut().poll(&mut cx), Poll::Pending));
    store.append_event(user("arrived-while-waiting"));
    drop(guard);
    let path = archive.await.unwrap();
    let snapshot = std::fs::read_to_string(path).unwrap();
    assert!(snapshot.contains("arrived-while-waiting"));
    assert!(store.events_for_session("s").is_empty());
}

#[tokio::test]
async fn archive_write_failure_keeps_all_events() {
    let dir = Scratch::new("archive-fail");
    let store = TrunkStore::new(100_000);
    store.set_persist_path(dir.0.join("sessions.json"));
    store.append_event(user("must-survive"));
    std::fs::write(dir.0.join("archives"), "not-a-directory").unwrap();
    let before = store.event_log();
    assert!(store.archive_session("s").await.is_err());
    assert_eq!(store.event_log(), before);
}

#[tokio::test]
async fn compaction_save_failure_returns_error_and_keeps_dirty_state() {
    let dir = Scratch::new("compact-save");
    let store = TrunkStore::new(100_000);
    let path = dir.0.join("sessions.json");
    store.set_persist_path(&path);
    for i in 0..6 {
        store.append_event(user(&format!("m{i}")));
    }
    store.save_now_checked().await.unwrap();
    let original = std::fs::read(&path).unwrap();
    let preview = store.compact_preview(2).unwrap();
    let summaries = vec![CompactionSummary::from_group(&preview.groups[0], "summary")];
    std::fs::create_dir(dir.0.join("sessions.json.tmp")).unwrap();
    let result = store.apply_compaction(2, &summaries, None).await;
    assert!(
        result.is_err(),
        "memory-only commit must not be reported as durable success"
    );
    assert!(store.is_dirty());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(
        matches!(store.event_log().first(), Some(SessionEvent::Compaction(_))),
        "do not roll back memory and overwrite concurrent events"
    );
}

#[tokio::test]
async fn archive_clear_save_failure_reports_backup_and_unsaved_memory() {
    let dir = Scratch::new("archive-save");
    let store = TrunkStore::new(100_000);
    let path = dir.0.join("sessions.json");
    store.set_persist_path(&path);
    store.append_event(user("backed-up"));
    store.save_now_checked().await.unwrap();
    let original = std::fs::read(&path).unwrap();
    std::fs::create_dir(dir.0.join("sessions.json.tmp")).unwrap();
    let error = store
        .archive_session("s")
        .await
        .expect_err("save failure must propagate");
    assert!(
        error.contains("归档"),
        "backup location/status must remain visible: {error}"
    );
    assert!(store.events_for_session("s").is_empty());
    assert!(store.is_dirty());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(
        std::fs::read_dir(dir.0.join("archives")).unwrap().count(),
        1
    );
}
