//! Shared enums referenced by both sides of the frontend protocol.
//!
//! Definitions live in `echo-defs` (the strategy-definition layer); this
//! module re-exports them so `echo_protocol::ThinkingMode` etc. keep working —
//! the wire contract must not own policy types.

pub use echo_defs::mode::{LoopMode, ReasoningEffort, ThinkingMode};
