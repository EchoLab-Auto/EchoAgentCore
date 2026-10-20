//! Plugin system for EchoAgentCore — the "everything is a plugin" seam.
//!
//! 本 crate 是**框架层**的一部分：只认识 manifest / 生命周期 / 注册表，
//! 不认识任何应用领域词汇（见 `document/design/plugin-framework-extraction.md`
//! 的边界判据）。领域适配在宿主侧（`echo-plugin-adapter`），因此依赖方向
//! 始终单向：`echo-plugin` 不依赖 `echo-defs`，也不依赖业务内核。
//!
//! Design (see document/plugins/core-plugins.md):
//! - A plugin is a manifest + lifecycle hooks; mounting is a reversible side
//!   effect (returns disposers), matching dsh's "registrations are effects".
//! - Plugins are **source-level** (compiled into the binary). Hot-reload
//!   applies to data plugins (skills/tools/manifests); code plugins reload
//!   via atomic binary replacement + process restart (the self-update path).

pub mod manifest;
pub mod plugin;
pub mod registry;

pub use manifest::{PluginError, PluginManifest, PluginRuntime};
pub use plugin::{BuiltinPlugin, MountContext, Plugin, PluginMountError, PluginMountResult};
pub use registry::{PluginDefinition, PluginDescriptor, PluginRegistry};
