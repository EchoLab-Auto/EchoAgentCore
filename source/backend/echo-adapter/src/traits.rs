//! Adapter trait and associated types.
//!
//! Definitions live in `echo-defs` (the capability-definition layer); this
//! module re-exports them so `echo_adapter::traits::…` paths keep working.
//! Implementations (filter pipeline, registry, ConfigStore) stay in this
//! crate.

pub use echo_defs::adapter::{
    Adapter, AdapterConnectionState, AdapterError, AdapterInfo, InboundMessageHook,
};
