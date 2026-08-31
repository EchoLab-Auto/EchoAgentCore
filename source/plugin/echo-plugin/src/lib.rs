//! Plugin system for EchoAgentCore — the "everything is a plugin" seam.
//!
//! Design (see document/0013-plugin-architecture.md):
//! - A plugin is a manifest + lifecycle hooks; mounting is a reversible side
//!   effect (returns disposers), matching dsh's "registrations are effects".
//! - Plugins are **source-level** (compiled into the binary). Hot-reload
//!   applies to data plugins (skills/tools/manifests); code plugins reload
//!   via atomic binary replacement + process restart (the self-update path).
//! - Plugins depend only on the definition layer (echo-defs / echo-context /
//!   echo-plugin), never on echo-agent (single-direction dependency).

pub mod manifest;
pub mod plugin;
pub mod registry;

pub use manifest::{PluginError, PluginKind, PluginManifest};
pub use plugin::{
    BuiltinPlugin, MountContext, Plugin, PluginMountError, PluginMountResult, PluginSkillSink,
    PluginToolHandle, PluginToolSink,
};
pub use registry::{PluginDescriptor, PluginRegistry};
