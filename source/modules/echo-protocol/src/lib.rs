//! Compatibility facade: the management module owns the protocol.
pub use echo_module_management::protocol::*;
pub use echo_module_management::protocol::{event, command};

/// Compatibility module identity.
pub mod module {
    pub const MODULE_ID: &str = echo_module_management::PROTOCOL_MODULE_ID;
}
