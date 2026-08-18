//! Shared policy enums used across capabilities and the frontend protocol.

use serde::{Deserialize, Serialize};

/// Gating mode: which users/groups may interact with the bot.
///
/// Serialised as `"none" | "allowlist" | "denylist"` (snake_case), matching
/// the legacy string-based protocol so WS payloads stay wire-compatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateMode {
    /// Everyone may interact.
    #[default]
    None,
    /// Only allowlisted users/groups may interact.
    Allowlist,
    /// Allowlisted-excluded: only denylisted users/groups are blocked.
    Denylist,
}

impl GateMode {
    pub fn as_str(self) -> &'static str {
        match self {
            GateMode::None => "none",
            GateMode::Allowlist => "allowlist",
            GateMode::Denylist => "denylist",
        }
    }
}

/// Whether model thinking/reasoning is enabled for DeepSeek-compatible APIs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    #[default]
    Enabled,
    Disabled,
}

impl ThinkingMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }
}

/// DeepSeek reasoning effort level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    High,
    #[default]
    Max,
}

impl ReasoningEffort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}
