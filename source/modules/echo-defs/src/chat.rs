//! Platform-agnostic **chat vocabulary** — the message/target/channel types
//! that cross the instance boundary.
//!
//! An instance translates platform events into [`IncomingMessage`] and accepts
//! [`MessageTarget`] to send replies. Consumers (agent loop, instance tools)
//! depend only on these types — never on a concrete platform crate.
//!
//! The seam **trait** for instances is [`crate::instance::ModuleInstance`] (not here):
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
}

impl std::fmt::Display for ChannelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelType::Direct => write!(f, "direct"),
            ChannelType::Group { group_id } => write!(f, "group:{}", group_id),
        }
    }
}

/// An inbound file attachment, saved on the local machine.
///
/// Instances that can receive files (e.g. QQ group uploads / private files)
/// save the bytes next to the incoming message and surface them here;
/// the agent reads `path` with its file/bash tools.
///
/// # 为什么它同时是**线格式**类型
///
/// 同一个文件从三条入口进来（平台上传、面板拖入、远端节点转发），要先后
/// 经过四层（`IncomingMessage` → ws 协议 → 会话事件日志 → 显示时间线）。
/// 每层各定义一个同形结构会有四份真值，而它们的字段名是**线格式**——
/// 改名不会有编译错误，只会让某一层的 `path` 变成 `undefined`。
/// 所以这里带 serde（各层直接复用它），面板的 TS 类型由 ts-rs 从它生成。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
pub struct IncomingFile {
    /// File name as shown to the user (e.g. `report.pdf`).
    pub name: String,
    /// Absolute path of the saved file; `None` when saving failed (see
    /// `error`) or was skipped (e.g. over the size limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-bindings", ts(optional))]
    pub path: Option<String>,
    /// File size in bytes (0 = unknown).
    #[serde(default)]
    pub size: u64,
    /// Failure/skip reason when `path` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-bindings", ts(optional))]
    pub error: Option<String>,
}

/// An inbound message from a platform instance.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    /// The instance that received this message (e.g., "qq").
    pub instance_name: String,
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
    /// Populated by instances that carry media (e.g. QQ images); the agent
    /// forwards them to vision-capable models. Empty for text-only messages.
    pub images: Vec<String>,
    /// File attachments already downloaded to this machine.
    ///
    /// Empty for messages without files. Populated by instances that support
    /// file transfer (e.g. QQ group uploads); see [`IncomingFile`].
    pub files: Vec<IncomingFile>,
}
