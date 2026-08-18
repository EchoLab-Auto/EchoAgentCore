//! Fork/resume metadata for a session log.

use serde::{Deserialize, Serialize};

/// Immutable metadata describing a session and its lineage.
///
/// `parent_session` + `seed_length` let resume and replay distinguish inherited
/// parent history from child work; `origin` classifies subagent children;
/// `delegation_depth` bounds recursion across restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    /// The session's id (canonical string, e.g. `qq:dm::123`).
    pub id: String,
    /// Unix epoch milliseconds when the session was created.
    pub created_at: i64,
    /// The session this one was forked from (seed lineage), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// How many leading events were inherited through a seed. Persisting this
    /// boundary lets resume and replay distinguish parent history from child
    /// work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_length: Option<usize>,
    /// Coarse product classification for a session created as a subagent child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Delegation depth: absent (zero) for a top-level session, parent depth
    /// + 1 for a subagent child.
    #[serde(default)]
    pub delegation_depth: u32,
}

impl SessionHeader {
    /// Build a top-level header (no lineage).
    pub fn top_level(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            created_at: chrono::Utc::now().timestamp_millis(),
            parent_session: None,
            seed_length: None,
            origin: None,
            delegation_depth: 0,
        }
    }

    /// Build a subagent child header, inheriting lineage from `parent`.
    pub fn child_of(id: impl Into<String>, parent: &SessionHeader) -> Self {
        Self {
            id: id.into(),
            created_at: chrono::Utc::now().timestamp_millis(),
            parent_session: Some(parent.id.clone()),
            seed_length: None,
            origin: Some("subagent".into()),
            delegation_depth: parent.delegation_depth + 1,
        }
    }

    /// Whether this header records any lineage (forked/resumed).
    pub fn has_lineage(&self) -> bool {
        self.parent_session.is_some() || self.seed_length.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_has_no_lineage() {
        let header = SessionHeader::top_level("qq:dm::123");
        assert!(!header.has_lineage());
        assert_eq!(header.delegation_depth, 0);
    }

    #[test]
    fn child_inherits_lineage_and_increments_depth() {
        let parent = SessionHeader::top_level("parent");
        let child = SessionHeader::child_of("child", &parent);
        assert_eq!(child.parent_session.as_deref(), Some("parent"));
        assert_eq!(child.origin.as_deref(), Some("subagent"));
        assert_eq!(child.delegation_depth, 1);
        assert!(child.has_lineage());
    }

    #[test]
    fn header_roundtrips_through_json() {
        let header = SessionHeader::child_of("child", &SessionHeader::top_level("parent"));
        let json = serde_json::to_string(&header).unwrap();
        let back: SessionHeader = serde_json::from_str(&json).unwrap();
        assert_eq!(back, header);
    }
}
