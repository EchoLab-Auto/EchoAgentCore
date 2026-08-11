//! Platform-agnostic message types for adapters.

use serde::{Deserialize, Serialize};

/// Distinguishes between direct messages and group messages across platforms.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChannelType {
    /// Private / direct message.
    Direct,
    /// Group / guild message.
    Group {
        /// Platform-specific group/channel id.
        group_id: String,
    },
}

impl ChannelType {
    pub fn is_group(&self) -> bool {
        matches!(self, ChannelType::Group { .. })
    }

    pub fn group_id(&self) -> Option<&str> {
        match self {
            ChannelType::Direct => None,
            ChannelType::Group { group_id } => Some(group_id.as_str()),
        }
    }

    /// String representation for use in session IDs / display.
    pub fn as_scope(&self) -> &str {
        match self {
            ChannelType::Direct => "dm",
            ChannelType::Group { .. } => "group",
        }
    }
}

impl std::fmt::Display for ChannelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelType::Direct => write!(f, "direct"),
            ChannelType::Group { group_id } => write!(f, "group:{}", group_id),
        }
    }
}

/// An inbound message from a platform adapter.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    /// The adapter that received this message (e.g., "qq").
    pub adapter_name: String,
    /// Platform identifier string (e.g., "qq").
    pub platform: String,
    /// Platform user ID as a string (e.g., "123456789").
    pub user_id: String,
    /// Display name of the sender.
    pub user_name: String,
    /// The channel the message was sent in.
    pub channel: ChannelType,
    /// Group/channel display name, if applicable.
    pub group_name: Option<String>,
    /// Plain text content.
    pub content: String,
    /// Unix timestamp from the platform.
    pub timestamp: i64,
    /// Whether the bot was @-mentioned.
    pub at_me: bool,
    /// Opaque metadata for platform-specific use.
    pub metadata: serde_json::Value,
}

/// Describes where to send a reply message.
#[derive(Debug, Clone)]
pub struct MessageTarget {
    /// Adapter to use (e.g., "qq").
    pub adapter_name: String,
    /// Channel type.
    pub channel: ChannelType,
    /// The target user id (for DM replies or @-mentions).
    pub user_id: String,
}

/// Result of sending a message.
#[derive(Debug, Clone)]
pub struct SendResult {
    /// Platform message ID, if available.
    pub message_id: Option<String>,
    /// Whether the send succeeded.
    pub success: bool,
    /// Error details if failed.
    pub error: Option<String>,
}

/// Events emitted by adapters for the agent to consume.
#[derive(Debug, Clone)]
pub enum AdapterEvent {
    /// A new message was received from the platform.
    MessageReceived(IncomingMessage),
    /// Connection state changed.
    ConnectionState {
        adapter_name: String,
        connected: bool,
        self_id: Option<String>,
    },
    /// The adapter was stopped.
    Stopped { adapter_name: String },
    /// An error occurred in the adapter.
    Error {
        adapter_name: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_type_is_group() {
        assert!(!ChannelType::Direct.is_group());
        assert!(ChannelType::Group {
            group_id: "123".into()
        }
        .is_group());
    }

    #[test]
    fn channel_type_group_id() {
        assert_eq!(ChannelType::Direct.group_id(), None);
        assert_eq!(
            ChannelType::Group {
                group_id: "456".into()
            }
            .group_id(),
            Some("456")
        );
    }

    #[test]
    fn channel_type_display() {
        assert_eq!(format!("{}", ChannelType::Direct), "direct");
        assert_eq!(
            format!(
                "{}",
                ChannelType::Group {
                    group_id: "789".into()
                }
            ),
            "group:789"
        );
    }

    #[test]
    fn channel_type_scope() {
        assert_eq!(ChannelType::Direct.as_scope(), "dm");
        assert_eq!(
            ChannelType::Group {
                group_id: "x".into()
            }
            .as_scope(),
            "group"
        );
    }
}
