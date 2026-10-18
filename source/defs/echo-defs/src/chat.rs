//! Platform-agnostic **chat vocabulary** — the message/target/channel types
//! that cross the adapter boundary.
//!
//! An adapter translates platform events into [`IncomingMessage`] and accepts
//! [`MessageTarget`] to send replies. Consumers (agent loop, adapter tools)
//! depend only on these types — never on a concrete platform crate.
//!
//! The seam **trait** for adapters is [`crate::adapter::Adapter`] (not here):
//! an earlier `ChatAdapter` trait in this module never got an implementation
//! and was removed rather than left as a decoy.

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

/// An inbound file attachment, downloaded to the local machine.
///
/// Adapters that can receive files (e.g. QQ group uploads / private files)
/// download the bytes next to the incoming message and surface them here;
/// the agent reads `path` with its file/bash tools.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncomingFile {
    /// File name as shown on the platform (e.g. `report.pdf`).
    pub name: String,
    /// Absolute path of the downloaded file; `None` when the download
    /// failed (see `error`) or was skipped (e.g. over the size limit).
    pub path: Option<String>,
    /// File size in bytes as reported by the platform (0 = unknown).
    pub size: u64,
    /// Download failure/skip reason when `path` is `None`.
    pub error: Option<String>,
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
    /// Multimodal media attached to the message: image URLs or `data:` URIs.
    ///
    /// Populated by adapters that carry media (e.g. QQ images); the agent
    /// forwards them to vision-capable models. Empty for text-only messages.
    pub images: Vec<String>,
    /// File attachments already downloaded to this machine.
    ///
    /// Empty for messages without files. Populated by adapters that support
    /// file transfer (e.g. QQ group uploads); see [`IncomingFile`].
    pub files: Vec<IncomingFile>,
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
