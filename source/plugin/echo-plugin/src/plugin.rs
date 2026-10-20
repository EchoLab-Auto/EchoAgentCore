//! Plugin trait — the mount/unmount lifecycle seam.

use std::sync::Arc;

use echo_context::Disposer;

use crate::manifest::PluginManifest;

/// What a plugin may consume when mounting.
///
/// **只携带框架自己能解释的东西**：一个服务定位器 `Ctx`。插件要注册能力，
/// 就 [`Ctx::provide`] 一个服务（按服务键 + 类型）；框架不认识"工具""技能"
/// 这类词汇——它们是**宿主的领域概念**，由宿主侧适配层在注册表里解释。
///
/// 历史（2026-10 M2）：这里曾有 `tools` / `skills` 两个字段，各配一个
/// `PluginToolSink` / `PluginSkillSink` trait。审计发现**零使用**——全仓
/// `mount` 闭包都是 `|_ctx| Ok(vec![])`，`with_skills` 从未被调用，
/// `register_tool` 零调用点。它们是"给未来外部工具插件预留的接缝"，
/// 而预留方式恰好把应用领域词汇写进了框架。删除后框架层零领域依赖。
///
/// [`Ctx::provide`]: echo_context::Ctx::provide
#[derive(Default, Clone)]
pub struct MountContext {
    /// Global service locator (services resolved by stable key).
    pub ctx: Option<Arc<echo_context::Ctx>>,
}

impl MountContext {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_ctx(mut self, ctx: Arc<echo_context::Ctx>) -> Self {
        self.ctx = Some(ctx);
        self
    }
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
