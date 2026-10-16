//! EchoAgentCore adapter abstraction.
//!
//! This crate implements a filter pipeline and an adapter registry — with
//! zero dependency on any specific protocol (OneBot, Telegram, etc.).
//! The platform-neutral definitions ([`Adapter`] trait, status/error types,
//! platform-agnostic message types, [`AdapterManagement`]) live in
//! `echo-defs` (the capability-definition layer) and are re-exported here so
//! `echo_adapter::…` paths keep working.
//!
//! # Crate layout
//!
//! | module | purpose |
//! |---|---|
//! | [`traits`] | re-export of `echo-defs`: `Adapter` trait, `AdapterError`, `AdapterInfo`（平台中立面） |
//! | [`capabilities`] | re-export of `echo-defs`：适配器管理能力扩展（[`AdapterManagement`]） |
//! | [`types`] | re-export of `echo-defs::chat`: `ChannelType`, `IncomingMessage`, `MessageTarget`, `AdapterEvent` |
//! | [`filter`] | `MessageFilter` trait, `FilterPipeline`, built-in filters |
//! | [`registry`] | `AdapterRegistry` |

pub mod capabilities;
pub mod config_store;
pub mod filter;
pub mod registry;
pub mod traits;
pub mod types;

pub use capabilities::AdapterManagement;
pub use config_store::{ensure_table, ConfigStore};

pub use filter::{FilterPipeline, FilterResult, MessageFilter};
pub use registry::AdapterRegistry;
pub use traits::{
    Adapter, AdapterConnectionState, AdapterError, AdapterInfo, InboundMessageHook,
};
pub use types::{AdapterEvent, ChannelType, IncomingMessage, MessageTarget, SendResult};
