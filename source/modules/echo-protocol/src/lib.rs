//! Compatibility facade: the management module owns the protocol.
pub use agent_module_management::protocol::*;
pub use agent_module_management::protocol::{command, event};

/// Compatibility module identity.
pub mod module {
    pub const MODULE_ID: &str = agent_module_management::PROTOCOL_MODULE_ID;
}
