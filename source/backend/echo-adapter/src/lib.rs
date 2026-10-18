//! EchoAgentCore adapter abstraction.
//!
//! EchoAgentCore adapter abstraction.
//!
//! This crate holds what is genuinely **platform-neutral and host-side**:
//! an [`AdapterRegistry`] (name → handle lookup) and a [`ConfigStore`]
//! (atomic TOML read-modify-write). The capability definitions themselves
//! ([`Adapter`] trait, status/error types, platform-agnostic message types,
//! [`AdapterManagement`]) live in `echo-defs` (the definition layer) and are
//! re-exported here so `echo_adapter::…` paths keep working.
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
//! | [`config_store`] | `ConfigStore`（TOML 原子读改写，全宿主共用） |

pub mod capabilities;
pub mod config_store;
pub mod registry;
pub mod traits;
pub mod types;

pub use capabilities::AdapterManagement;
pub use config_store::{ensure_table, ConfigStore};

pub use registry::AdapterRegistry;
pub use traits::{Adapter, AdapterConnectionState, AdapterError, AdapterInfo};
pub use types::{ChannelType, IncomingMessage, MessageTarget, SendResult};
