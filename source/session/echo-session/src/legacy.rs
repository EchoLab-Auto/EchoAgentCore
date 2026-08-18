//! Migration from the pre-event-sourced `echo-sessions.json` v1–v4 formats.
//!
//! The old format stored a single global `trunk_history` (a list of chat
//! messages with `role`/`content`/`reasoning_content`) plus identity labels
//! and a display timeline. The event-sourced store reads it back by turning
//! each message into the equivalent [`SessionEvent`]: user messages become
//! `UserMessage`, assistant messages become `AssistantMessage`, tool messages
//! become `ToolResult` events.
//!
//! Because the old format **dropped** `tool_calls`/`tool_call_id` on the wire
//! (see the Phase 0 baseline test), migrated tool messages degrade to plain
//! text — the same behaviour as the old loader. New logs written by the
//! event-sourced store preserve the structure.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event::{AssistantMessage, SessionEvent, ToolResultEvent, UserMessage};

/// Migration errors.
#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("legacy document is not valid JSON: {0}")]
    InvalidJson(String),
    #[error("legacy document has no recoverable identity/history structure")]
    NoStructure,
    #[error("unsupported legacy version {0}")]
    UnsupportedVersion(u32),
}

/// The legacy document shape (v1–v4), as much as migration needs.
///
/// - v1: a top-level array of `{id, history, ...}` sessions.
/// - v2: `{shared_context, shared_history, sessions}`.
/// - v3: `{trunk_history, identities}`.
/// - v4: `{trunk_history, identities, timeline}`.
#[derive(Debug, Deserialize)]
struct LegacyDocument {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    trunk_history: Vec<V4Message>,
    #[serde(default)]
    shared_history: Vec<V4Message>,
    #[serde(default)]
    identities: Vec<V4Identity>,
    #[serde(default)]
    sessions: Vec<V4Identity>,
    // Parsed for structural compatibility; the event-sourced store does not
    // persist the display timeline (it is projected from the log).
    #[serde(default)]
    #[allow(dead_code)]
    timeline: Vec<serde_json::Value>,
}

impl LegacyDocument {
    fn history(&self) -> &[V4Message] {
        if !self.trunk_history.is_empty() {
            &self.trunk_history
        } else {
            &self.shared_history
        }
    }

    fn identities(&self) -> &[V4Identity] {
        if !self.identities.is_empty() {
            &self.identities
        } else {
            &self.sessions
        }
    }
}

/// One legacy message. `tool_calls`/`tool_call_id` were not persisted.
#[derive(Debug, Deserialize)]
struct V4Message {
    role: String,
    content: String,
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct V4Identity {
    id: String,
}

/// The migrated session: a header plus the reconstructed event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigratedSession {
    pub session_id: String,
    pub events: Vec<SessionEvent>,
}

/// Migrate a legacy `echo-sessions.json` document (v1–v4) into per-session
/// event logs.
///
/// The old format kept **one global trunk** shared by all identities; each
/// identity label is emitted as its own migrated session carrying the same
/// (global) event list, preserving the old sharing semantics. Returns an
/// error only for structurally invalid input; a valid empty document yields
/// an empty migration.
pub fn migrate_v4_document(data: &str) -> Result<Vec<MigratedSession>, MigrationError> {
    // v1 is a top-level array of sessions, not an object; handle it first.
    if data.trim_start().starts_with('[') {
        return migrate_v1_array(data);
    }
    let doc: LegacyDocument =
        serde_json::from_str(data).map_err(|e| MigrationError::InvalidJson(e.to_string()))?;
    if doc.version > 4 {
        return Err(MigrationError::UnsupportedVersion(doc.version));
    }
    if doc.identities().is_empty() && doc.history().is_empty() {
        return Ok(Vec::new());
    }

    let events = migrate_history(doc.history());

    let session_ids: Vec<String> = if doc.identities().is_empty() {
        // A trunk with no identity labels (early v3-style file): keep one
        // synthetic session so history is not lost.
        vec!["trunk".into()]
    } else {
        doc.identities().iter().map(|i| i.id.clone()).collect()
    };

    Ok(session_ids
        .into_iter()
        .map(|session_id| MigratedSession {
            session_id,
            events: events.clone(),
        })
        .collect())
}

/// Migrate a v1 top-level array of per-session objects.
fn migrate_v1_array(data: &str) -> Result<Vec<MigratedSession>, MigrationError> {
    #[derive(Debug, Deserialize)]
    struct V1Session {
        id: String,
        #[serde(default)]
        history: Vec<V4Message>,
    }
    let sessions: Vec<V1Session> =
        serde_json::from_str(data).map_err(|e| MigrationError::InvalidJson(e.to_string()))?;
    Ok(sessions
        .into_iter()
        .map(|session| MigratedSession {
            session_id: session.id,
            events: migrate_history(&session.history),
        })
        .collect())
}

/// Reconstruct events from a legacy history (tool structure was not
/// persisted).
fn migrate_history(history: &[V4Message]) -> Vec<SessionEvent> {
    history
        .iter()
        .filter_map(|message| match message.role.as_str() {
            "user" => Some(SessionEvent::UserMessage(UserMessage {
                content: message.content.clone(),
                timestamp: 0,
                message_sequence: None,
                source: None,
            })),
            "assistant" => Some(SessionEvent::AssistantMessage(AssistantMessage {
                content: message.content.clone(),
                reasoning_content: message.reasoning_content.clone(),
                // The old format dropped tool_calls; nothing to restore.
                tool_calls: vec![],
            })),
            "tool" => Some(SessionEvent::ToolResult(ToolResultEvent {
                // The old format dropped tool_call_id; the content is kept.
                tool_call_id: String::new(),
                result: message.content.clone(),
            })),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_global_trunk_into_per_identity_logs() {
        let data = r#"{
            "version": 4,
            "trunk_history": [
                {"role": "user", "content": "你好"},
                {"role": "assistant", "content": "回复", "reasoning_content": "想"},
                {"role": "tool", "content": "2"}
            ],
            "identities": [{"id": "qq:dm::1"}, {"id": "qq:dm::2"}],
            "timeline": []
        }"#;
        let sessions = migrate_v4_document(data).unwrap();
        assert_eq!(sessions.len(), 2, "one log per identity");
        assert_eq!(sessions[0].session_id, "qq:dm::1");
        assert_eq!(sessions[0].events.len(), 3);
        assert_eq!(sessions[0].events[0].kind(), "user/message");
        assert_eq!(sessions[0].events[1].kind(), "assistant/message");
        assert_eq!(sessions[0].events[2].kind(), "tool/result");
    }

    #[test]
    fn migrates_v2_shared_context_document() {
        let data = r#"{
            "version": 2,
            "shared_context": true,
            "shared_history": [{"role": "user", "content": "shared"}],
            "sessions": [
                {"id": "local:tui::local_user", "platform": "local"},
                {"id": "qq:dm::123", "platform": "qq"}
            ]
        }"#;
        let sessions = migrate_v4_document(data).unwrap();
        assert_eq!(sessions.len(), 2, "one log per v2 session");
        assert_eq!(sessions[0].events.len(), 1);
        assert_eq!(sessions[0].events[0].kind(), "user/message");
    }

    #[test]
    fn migrates_v1_array_of_sessions() {
        let data = r#"[
            {"id": "local:tui::local_user", "nickname": "local", "history": [{"role": "user", "content": "local"}]},
            {"id": "qq:dm::123", "nickname": "alice", "history": [{"role": "user", "content": "qq"}]}
        ]"#;
        let sessions = migrate_v4_document(data).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "local:tui::local_user");
        assert_eq!(sessions[1].session_id, "qq:dm::123");
    }

    #[test]
    fn empty_document_migrates_empty() {
        assert!(
            migrate_v4_document(r#"{"version": 4, "trunk_history": [], "identities": []}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn invalid_json_is_error() {
        assert!(matches!(
            migrate_v4_document("not json"),
            Err(MigrationError::InvalidJson(_))
        ));
    }

    #[test]
    fn newer_version_is_rejected() {
        assert!(matches!(
            migrate_v4_document(r#"{"version": 6, "trunk_history": [], "identities": []}"#),
            Err(MigrationError::UnsupportedVersion(6))
        ));
    }
}
