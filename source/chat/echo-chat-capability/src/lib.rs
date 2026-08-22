//! EchoAgentCore chat-platform capability seam — Service Definition role.
//!
//! A chat platform (QQ, Telegram, ...) is a swappable capability. The agent
//! loop must not import platform tool names or parse platform payloads; it
//! drives deliveries through the [`DeliveryPolicy`] seam instead. A platform
//! provider (e.g. `echo-qq`) implements this policy plus the
//! [`ChatAdapter`](echo_defs::chat::ChatAdapter) lifecycle; the loop depends
//! only on this crate.

use std::collections::HashSet;

use echo_defs::message::ToolCall;

/// A delivery target the current input requires the agent to reach.
///
/// The variant set is platform-independent: a direct user, a group, or the
/// backend/TUI. Platform specifics (which tool name carries the delivery,
/// which id field identifies the target) live in the policy implementation,
/// never here — so the loop can build reminders and validate calls without
/// knowing the platform.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DeliveryTarget {
    Direct { user_id: String },
    Group { group_id: String },
    Backend { session_id: String },
}

/// The delivery policy of a chat platform: how to read required deliveries
/// out of an input, validate that a tool call satisfies one, and describe the
/// outstanding work to the model.
///
/// This is the **Service Definition** of the delivery seam: a platform
/// provider implements it; the agent loop (and its reminders/validation)
/// depends only on this trait.
pub trait DeliveryPolicy: Send + Sync {
    /// Parse the required deliveries out of an input. Returns `None` when the
    /// input carries no delivery requirements.
    fn plan_from_input(&self, content: &str) -> Option<Vec<DeliveryTarget>>;

    /// Validate a tool call against the plan and record it as delivered.
    ///
    /// Returns `Ok(None)` when the call is not a delivery call; `Ok(Some(key))`
    /// when it satisfies a declared target (the key is recorded into
    /// `delivered`, which drives the "at least one delivery" reminder);
    /// `Err` when it targets an undeclared destination. Repeat deliveries to
    /// the same target are allowed — how many replies to send is the agent's
    /// decision.
    fn validate_delivery_call(
        &self,
        plan: &[DeliveryTarget],
        delivered: &mut HashSet<String>,
        call: &ToolCall,
    ) -> Result<Option<String>, String>;

    /// The model-facing correction text listing undelivered targets.
    fn delivery_reminder(&self, pending: &[&DeliveryTarget]) -> String;
}

/// Convenience: the dedup key of a target (used by implementations).
pub fn target_key(target: &DeliveryTarget) -> String {
    match target {
        DeliveryTarget::Direct { user_id } => format!("direct:{user_id}"),
        DeliveryTarget::Group { group_id } => format!("group:{group_id}"),
        DeliveryTarget::Backend { session_id } => format!("backend:{session_id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_keys_are_distinct_per_kind() {
        assert_ne!(
            target_key(&DeliveryTarget::Direct {
                user_id: "1".into()
            }),
            target_key(&DeliveryTarget::Group {
                group_id: "1".into()
            }),
        );
        assert_eq!(
            target_key(&DeliveryTarget::Direct {
                user_id: "1".into()
            }),
            target_key(&DeliveryTarget::Direct {
                user_id: "1".into()
            }),
        );
    }
}
