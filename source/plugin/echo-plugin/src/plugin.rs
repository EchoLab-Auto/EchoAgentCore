//! Plugin trait — the mount/unmount lifecycle seam.

use std::sync::Arc;

use echo_context::Disposer;

use crate::manifest::PluginManifest;

/// What a plugin may consume when mounting. Kept deliberately narrow so
/// plugins never depend on echo-agent directly — only the seams they need.
#[derive(Default, Clone)]
pub struct MountContext {
    /// Tool registry hooks (builtin tools register here).
    pub tools: Option<Arc<dyn PluginToolSink>>,
    /// Skill registry hooks (builtin skills register here).
    pub skills: Option<Arc<dyn PluginSkillSink>>,
    /// Global service locator (services resolved by stable key).
    pub ctx: Option<Arc<echo_context::Ctx>>,
}

impl MountContext {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_tools(mut self, tools: Arc<dyn PluginToolSink>) -> Self {
        self.tools = Some(tools);
        self
    }
    pub fn with_skills(mut self, skills: Arc<dyn PluginSkillSink>) -> Self {
        self.skills = Some(skills);
        self
    }
    pub fn with_ctx(mut self, ctx: Arc<echo_context::Ctx>) -> Self {
        self.ctx = Some(ctx);
        self
    }
}

/// Narrow tool sink used by plugins (avoids a dependency on echo-agent's
/// concrete ToolRegistry; echo-defs Tool stays the definition type).
pub trait PluginToolSink: Send + Sync {
    fn register_tool(&self, name: &str, description: &str) -> Arc<dyn PluginToolHandle>;
}

/// Handle to a registered tool that can be removed (reversible registration).
pub trait PluginToolHandle: Send + Sync {
    fn dispose(self: Arc<Self>);
}

/// Narrow skill sink for data plugins.
pub trait PluginSkillSink: Send + Sync {
    /// Register a skill directory (walks for SKILL.md under `dir`) and return
    /// a disposer that removes those skills.
    fn register_skill_dir(&self, dir: &str) -> Result<Disposer, String>;
}

/// Errors produced during mount.
#[derive(Debug, thiserror::Error)]
pub enum PluginMountError {
    #[error("mount failed: {0}")]
    Mount(String),
    #[error("plugin kind {kind} is not supported by this host")]
    UnsupportedKind { kind: &'static str },
}

/// Result of a successful mount: disposers that undo the registration.
pub type PluginMountResult = Result<Vec<Disposer>, PluginMountError>;

/// A mountable plugin.
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;

    /// Mount the plugin: perform all registrations, returning disposers.
    /// The registry holds these and calls them on unmount.
    fn mount(&self, ctx: &MountContext) -> PluginMountResult;

    /// Extra cleanup beyond disposers (default: nothing).
    fn unmount(&self) -> Result<(), PluginMountError> {
        Ok(())
    }
}

/// A ready-made plugin for built-in modules: manifest + a mount closure.
pub struct BuiltinPlugin {
    manifest: PluginManifest,
    mount_fn: Box<dyn Fn(&MountContext) -> PluginMountResult + Send + Sync>,
}

impl BuiltinPlugin {
    pub fn new(
        manifest: PluginManifest,
        mount_fn: impl Fn(&MountContext) -> PluginMountResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            manifest,
            mount_fn: Box::new(mount_fn),
        }
    }
}

impl Plugin for BuiltinPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn mount(&self, ctx: &MountContext) -> PluginMountResult {
        (self.mount_fn)(ctx)
    }
}
