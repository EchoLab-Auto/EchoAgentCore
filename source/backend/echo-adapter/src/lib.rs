//! EchoAgentCore adapter abstraction.
//!
//! This crate defines the [`Adapter`] trait, platform-agnostic message types,
//! a filter pipeline, and an adapter registry — with zero dependency on any
//! specific protocol (OneBot, Telegram, etc.).
//!
//! # Crate layout
//!
//! | module | purpose |
//! |---|---|
//! | [`traits`] | `Adapter` trait, `AdapterError`, `AdapterInfo`（平台中立面） |
//! | [`capabilities`] | 平台能力扩展 trait（`QqManagement` 等） |
//! | [`types`] | `ChannelType`, `IncomingMessage`, `MessageTarget`, `AdapterEvent` |
//! | [`filter`] | `MessageFilter` trait, `FilterPipeline`, built-in filters |
//! | [`registry`] | `AdapterRegistry` |

pub mod capabilities;
pub mod config_store;
pub mod filter;
pub mod registry;
pub mod traits;
pub mod types;

pub use capabilities::{QqInstanceInfo, QqManagement};
pub use config_store::{ensure_table, ConfigStore};

pub use filter::{FilterPipeline, FilterResult, MessageFilter};
pub use registry::AdapterRegistry;
pub use traits::{
    Adapter, AdapterConnectionState, AdapterError, AdapterInfo, GateMode, InboundMessageHook,
};
pub use types::{AdapterEvent, ChannelType, IncomingMessage, MessageTarget, SendResult};
