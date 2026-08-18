//! The append-only event log and its durable persistence.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event::SessionEvent;
use crate::LOG_FORMAT_VERSION;

/// Persistence errors.
#[derive(Debug, Error)]
pub enum EventLogError {
    #[error("log read failed: {0}")]
    Read(String),
    #[error("log parse failed: {0}")]
    Parse(String),
    #[error("log serialize failed: {0}")]
    Serialize(String),
    #[error("log write failed: {0}")]
    Write(String),
    #[error("unsupported log version {0}")]
    UnsupportedVersion(u32),
}

/// An append-only, per-session event log.
///
/// Events are only appended; compaction is expressed as a
/// [`SessionEvent::Compaction`] event (a summary replacing a prefix), never
/// by mutating or deleting earlier events. The log is the single source of
/// truth — the model context is derived from it, never stored alongside it.
///
/// Persistence is a whole-log JSON document (atomic tmp+rename), versioned by
/// [`LOG_FORMAT_VERSION`].
#[derive(Debug, Clone, Default)]
pub struct EventLog {
    events: Arc<Mutex<Vec<SessionEvent>>>,
}

impl EventLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one event (durable on the next save).
    pub fn append(&self, event: SessionEvent) {
        self.events.lock().expect("event log poisoned").push(event);
    }

    /// Append many events in order.
    pub fn extend(&self, events: impl IntoIterator<Item = SessionEvent>) {
        self.events
            .lock()
            .expect("event log poisoned")
            .extend(events);
    }

    /// Read the full log, oldest first.
    pub fn log(&self) -> Vec<SessionEvent> {
        self.events.lock().expect("event log poisoned").clone()
    }

    /// Insert an event after the last `UserMessage` carrying `sequence`.
    ///
    /// Concurrent branches merge their replies back into the log in request
    /// order: a reply for request `N` is inserted directly after that
    /// request's user event, so the projected order matches the requests'
    /// arrival order even when replies finish out of order. Falls back to
    /// appending when no matching user event exists.
    ///
    /// A tool turn appends `ToolCall`/`ToolResult` events after its user
    /// event; the reply is inserted past them so the projected order never
    /// shows the final reply before the tool round it answers.
    pub fn insert_after_sequence(&self, sequence: u64, event: SessionEvent) {
        let mut events = self.events.lock().expect("event log poisoned");
        let insert_at = events.iter().rposition(|existing| match existing {
            SessionEvent::UserMessage(message) => message.message_sequence == Some(sequence),
            _ => false,
        });
        match insert_at {
            Some(index) => {
                let mut insert_at = index + 1;
                while insert_at < events.len()
                    && matches!(
                        events[insert_at],
                        SessionEvent::ToolCall(_) | SessionEvent::ToolResult(_)
                    )
                {
                    insert_at += 1;
                }
                events.insert(insert_at, event);
            }
            None => events.push(event),
        }
    }

    pub fn len(&self) -> usize {
        self.events.lock().expect("event log poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.lock().expect("event log poisoned").is_empty()
    }

    /// Serialize the whole log (the durable representation).
    pub fn serialize(&self) -> Result<String, EventLogError> {
        let events = self.log();
        serde_json::to_string_pretty(&LogDocument {
            version: LOG_FORMAT_VERSION,
            events,
        })
        .map_err(|e| EventLogError::Serialize(e.to_string()))
    }

    /// Parse a serialized log document.
    pub fn deserialize(data: &str) -> Result<Vec<SessionEvent>, EventLogError> {
        let doc: LogDocument =
            serde_json::from_str(data).map_err(|e| EventLogError::Parse(e.to_string()))?;
        if doc.version != LOG_FORMAT_VERSION {
            return Err(EventLogError::UnsupportedVersion(doc.version));
        }
        Ok(doc.events)
    }

    /// Atomically persist the log to `path` (tmp + rename).
    pub fn save_to(&self, path: &Path) -> Result<(), EventLogError> {
        let data = self.serialize()?;
        let tmp = format!("{}.tmp", path.display());
        std::fs::write(&tmp, &data).map_err(|e| EventLogError::Write(e.to_string()))?;
        std::fs::rename(&tmp, path).map_err(|e| EventLogError::Write(e.to_string()))?;
        Ok(())
    }

    /// Load a log from `path`; a missing file yields an empty log.
    pub fn load_from(&self, path: &Path) -> Result<Vec<SessionEvent>, EventLogError> {
        let data = std::fs::read_to_string(path).map_err(|e| EventLogError::Read(e.to_string()))?;
        Self::deserialize(&data)
    }

    /// The persistence path convention for a session id.
    pub fn path_for(dir: &Path, session_id: &str) -> PathBuf {
        // Session ids contain ':' which is invalid on Windows; sanitize.
        let safe = session_id.replace(':', "_");
        dir.join(format!("{safe}.json"))
    }
}

/// On-disk envelope for the event log.
#[derive(Debug, Serialize, Deserialize)]
struct LogDocument {
    version: u32,
    events: Vec<SessionEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AssistantMessage, UserMessage};

    fn sample_log() -> EventLog {
        let log = EventLog::new();
        log.append(SessionEvent::UserMessage(UserMessage {
            content: "你好".into(),
            timestamp: 1700000000,
            message_sequence: None,
            source: None,
        }));
        log.append(SessionEvent::AssistantMessage(AssistantMessage {
            content: "回复".into(),
            reasoning_content: None,
            tool_calls: vec![],
        }));
        log
    }

    #[test]
    fn append_and_read_in_order() {
        let log = sample_log();
        assert_eq!(log.len(), 2);
        let events = log.log();
        assert_eq!(events[0].kind(), "user/message");
        assert_eq!(events[1].kind(), "assistant/message");
    }

    #[test]
    fn insert_after_sequence_skips_tool_events() {
        use crate::event::{ToolCallEvent, ToolResultEvent};
        let log = EventLog::new();
        log.append(SessionEvent::UserMessage(UserMessage {
            content: "跑一下".into(),
            timestamp: 1700000000,
            message_sequence: Some(1),
            source: None,
        }));
        log.append(SessionEvent::ToolCall(ToolCallEvent {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: "{}".into(),
        }));
        log.append(SessionEvent::ToolResult(ToolResultEvent {
            tool_call_id: "call_1".into(),
            result: "ok".into(),
        }));
        // The final reply must land after the turn's own tool events, not
        // between the user event and them.
        log.insert_after_sequence(
            1,
            SessionEvent::AssistantMessage(AssistantMessage {
                content: "完成".into(),
                reasoning_content: None,
                tool_calls: vec![],
            }),
        );
        let events = log.log();
        assert_eq!(events.len(), 4);
        assert!(matches!(events[1], SessionEvent::ToolCall(_)));
        assert!(matches!(events[2], SessionEvent::ToolResult(_)));
        assert!(matches!(events[3], SessionEvent::AssistantMessage(_)));
    }

    #[test]
    fn serialize_deserialize_roundtrip() {
        let log = sample_log();
        let data = log.serialize().unwrap();
        let restored = EventLog::deserialize(&data).unwrap();
        assert_eq!(restored, log.log());
    }

    #[test]
    fn save_and_load_via_file() {
        let log = sample_log();
        let path =
            std::env::temp_dir().join(format!("echo-session-log-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        log.save_to(&path).unwrap();
        let loaded = EventLog::new();
        let restored = loaded.load_from(&path).unwrap();
        assert_eq!(restored.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let err = EventLog::deserialize(r#"{"version": 99, "events": []}"#).unwrap_err();
        assert!(matches!(err, EventLogError::UnsupportedVersion(99)));
    }

    #[test]
    fn malformed_document_is_parse_error() {
        assert!(matches!(
            EventLog::deserialize("not json"),
            Err(EventLogError::Parse(_))
        ));
    }

    #[test]
    fn path_for_sanitizes_colons() {
        let dir = Path::new("/tmp");
        let path = EventLog::path_for(dir, "qq:dm::123");
        assert_eq!(path, PathBuf::from("/tmp/qq_dm__123.json"));
    }
}
