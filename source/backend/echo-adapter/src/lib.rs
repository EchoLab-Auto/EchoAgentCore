//! EchoAgentCore adapter abstraction.
//!
//! This crate holds what is genuinely **platform-neutral and host-side**:
//! an [`AdapterRegistry`] (name → handle lookup). The capability definitions
//! themselves ([`Adapter`] trait, status/error types, platform-agnostic message
//! types, [`AdapterManagement`]) live in `echo-defs` (the definition layer) and
//! are re-exported here so `echo_adapter::…` paths keep working.
//!
//! Config persistence used to live here too; it was never adapter-specific and
//! now lives in its own `echo-config` crate.
//!
//! Filtering is **not** here: after adapter pluginization, inbound filtering
//! runs inside the adapter plugin process (before `deliver_message` reaches
//! the host). A host-side pipeline had no callers and was removed.
//!
//! # Crate layout
//!
//! | module | purpose |
//! |---|---|
//! | [`traits`] | re-export of `echo-defs`: `Adapter` trait, `AdapterError`, `AdapterInfo`（平台中立面） |
//! | [`capabilities`] | re-export of `echo-defs`：适配器管理能力扩展（[`AdapterManagement`]） |
//! | [`types`] | re-export of `echo-defs::chat`: `ChannelType`, `IncomingMessage`, `MessageTarget` |
//! | [`registry`] | `AdapterRegistry`（name → `Arc<dyn Adapter>`） |

pub mod capabilities;
pub mod registry;
pub mod traits;
pub mod types;

pub use capabilities::AdapterManagement;
pub use registry::AdapterRegistry;
pub use traits::{Adapter, AdapterConnectionState, AdapterError, AdapterInfo, AdapterToolSpec};
pub use types::{ChannelType, IncomingMessage};
