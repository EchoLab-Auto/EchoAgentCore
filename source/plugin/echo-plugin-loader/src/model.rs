//! Public data model: entry kinds, composed rows, and the loaded tree.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How a plugin instance is launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Inline builtin: resolved from the composition root's builtin registry.
    Builtin,
    /// Standalone process speaking the plugin protocol over stdio.
    Stdio,
    /// Dynamically loaded library (experimental track).
    Dylib,
    /// WebAssembly module (experimental track).
    Wasm,
}

impl EntryKind {
    /// Lowercase config spelling (`builtin` / `stdio` / `dylib` / `wasm`).
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Builtin => "builtin",
            EntryKind::Stdio => "stdio",
            EntryKind::Dylib => "dylib",
            EntryKind::Wasm => "wasm",
        }
    }
}

/// One composed row: how to launch and configure a single plugin instance.
///
/// Distinct from `echo_plugin::PluginManifest` (what a plugin *is*): a row
/// describes how the host starts it (inline builtin / standalone stdio
/// process / dylib / wasm) and carries its config.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginEntry {
    /// Stable lookup key used by the patch semantics (`按 id 定位`).
    pub id: String,
    /// Launch mechanism.
    pub kind: EntryKind,
    /// For `builtin`: the plugin id to mount; for `stdio`: the executable name.
    pub name: String,
    /// Executable path; required (non-empty) when `kind == Stdio`.
    pub command: Option<String>,
    /// Extra command-line arguments.
    pub args: Vec<String>,
    /// Extra environment variables for the launched instance.
    pub env: BTreeMap<String, String>,
    /// Whether the entry participates in the default runtime set.
    pub enabled: bool,
    /// Ids of other rows that must be available first (drives the ordering).
    pub requires: Vec<String>,
    /// Free-form plugin config (TOML table turned into a JSON object; empty
    /// object when absent).
    pub config: serde_json::Value,
    /// 0-based index of the layer (applied file) that last wrote this row.
    pub layer: usize,
    /// File name of that layer.
    pub layer_source: String,
}

/// A composed tree plus non-fatal diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedTree {
    /// Rows in dependency order (stable topological sort by `requires`).
    pub entries: Vec<PluginEntry>,
    /// Non-fatal diagnostics, e.g. a duplicate id inside one layer.
    pub warnings: Vec<String>,
}
