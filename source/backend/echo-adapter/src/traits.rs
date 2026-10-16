//! Adapter trait and associated types.
//!
//! Definitions live in `echo-defs` (the capability-definition layer); this
//! module re-exports them so `echo_adapter::traits::…` paths keep working.
//! Implementations (filter pipeline, registry, ConfigStore) stay in this
//! crate.

pub use echo_defs::adapter::{
    Adapter, AdapterConnectionState, AdapterError, AdapterInfo, InboundMessageHook,
};

/// Gating mode: which users/groups may interact with the bot.
///
/// `GateMode` is a platform-neutral policy type defined in `echo-defs` and
/// re-exported through `echo-protocol`; re-exported here so
/// `echo_adapter::GateMode` keeps working.
pub use echo_defs::GateMode;
