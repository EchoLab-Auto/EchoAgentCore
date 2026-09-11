//! PluginRegistry — discovery, lifecycle and state of all plugins.

use std::collections::BTreeMap;
use std::sync::Arc;

use echo_context::Disposer;

use crate::manifest::PluginManifest;
use crate::plugin::{MountContext, Plugin, PluginMountError, PluginMountResult};

/// Frontend-facing snapshot of a plugin's state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginDescriptor {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    pub description: String,
    pub entry: String,
    pub author: String,
    /// Whether the plugin is currently mounted (active).
    pub enabled: bool,
    /// Whether the plugin ships inside the binary (vs. discovered from disk).
    pub builtin: bool,
    /// 所属包 id（横跨 plugin+tool+skill 的标签，见
    /// [`PluginManifest::package_id`]）；未声明 = 插件 id 自身。
    #[serde(default)]
    pub package: String,
    /// Mount error description when `enabled == false && builtin`.
    #[serde(default)]
    pub error: Option<String>,
}

impl From<&PluginManifest> for PluginDescriptor {
    fn from(m: &PluginManifest) -> Self {
        Self {
            id: m.id.clone(),
            name: m.name.clone(),
            version: m.version.clone(),
            kind: m.kind.as_str().into(),
            description: m.description.clone(),
            entry: m.entry.clone(),
            author: m.author.clone(),
            enabled: true,
            builtin: true,
            package: m.package_id().to_string(),
            error: None,
        }
    }
}

/// Registry of plugins with mount/unmount lifecycle.
///
/// Thread-safe: `register` is for composition-root-time registration of
/// builtins; `mount_all`/`unmount_all` manage the active set. `set_enabled`
/// toggles a plugin at runtime (used by the Panel TogglePlugin command).
#[derive(Default)]
pub struct PluginRegistry {
    plugins: std::sync::Mutex<BTreeMap<String, Arc<dyn Plugin>>>,
    active: std::sync::Mutex<BTreeMap<String, Vec<Disposer>>>,
    enabled_state: std::sync::Mutex<BTreeMap<String, bool>>,
}

impl std::fmt::Debug for PluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("ids", &self.ids())
            .finish()
    }
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a plugin definition (composition root / discovery). Does not
    /// mount it yet.
    pub fn register(&self, plugin: Arc<dyn Plugin>) -> Result<(), String> {
        let id = plugin.manifest().id.clone();
        let mut plugins = self.plugins.lock().unwrap();
        if plugins.contains_key(&id) {
            return Err(format!("plugin id already registered: {id}"));
        }
        plugins.insert(id, Arc::clone(&plugin));
        Ok(())
    }

    /// Remove a registered (and unmounted) plugin by id.
    pub fn unregister(&self, id: &str) -> bool {
        let active = self.active.lock().unwrap();
        if active.contains_key(id) {
            return false; // must be unmounted first
        }
        drop(active);
        self.plugins.lock().unwrap().remove(id).is_some()
    }

    /// Activate a plugin: run its mount hooks and retain disposers.
    pub fn mount(&self, id: &str, ctx: &MountContext) -> PluginMountResult {
        let plugin = {
            let plugins = self.plugins.lock().unwrap();
            match plugins.get(id) {
                Some(p) => Arc::clone(p),
                None => {
                    return Err(PluginMountError::Mount(format!(
                        "plugin not registered: {id}"
                    )))
                }
            }
        };
        let disposers = plugin.mount(ctx)?;
        let mut active = self.active.lock().unwrap();
        active.insert(id.to_string(), disposers);
        Ok(Vec::new())
    }

    /// Deactivate a plugin: dispose all registrations (reversible effects).
    pub fn unmount(&self, id: &str) -> Result<(), PluginMountError> {
        let mut active = self.active.lock().unwrap();
        if let Some(mut disposers) = active.remove(id) {
            for d in disposers.drain(..) {
                d.dispose();
            }
        }
        Ok(())
    }

    /// Mount every registered plugin (starting-up). Plugins whose mount fails
    /// are skipped and reported via `descriptors()`.
    pub fn mount_all(&self, ctx: &MountContext) -> Vec<(String, String)> {
        let mut errors = Vec::new();
        let ids: Vec<String> = {
            let plugins = self.plugins.lock().unwrap();
            plugins.keys().cloned().collect()
        };
        for id in ids {
            if !self.is_enabled(&id) {
                continue;
            }
            if let Err(e) = self.mount(&id, ctx) {
                errors.push((id, e.to_string()));
            }
        }
        errors
    }

    /// Unmount everything (shutdown).
    pub fn unmount_all(&self) {
        let ids: Vec<String> = {
            let active = self.active.lock().unwrap();
            active.keys().cloned().collect()
        };
        for id in ids {
            let _ = self.unmount(&id);
        }
    }

    /// Runtime enable/disable. Disabling unmounts; enabling remounts.
    /// `persisted` is the caller's disk-state (e.g. `[agent].disabled_plugins`).
    pub fn set_enabled(&self, id: &str, enabled: bool, ctx: &MountContext) -> Result<(), String> {
        let exists = self.plugins.lock().unwrap().contains_key(id);
        if !exists {
            return Err(format!("plugin not found: {id}"));
        }
        let mut states = self.enabled_state.lock().unwrap();
        if enabled {
            if states.get(id).copied().unwrap_or(true) {
                return Ok(()); // already enabled
            }
            self.mount(id, ctx).map_err(|e| e.to_string())?;
            states.insert(id.to_string(), true);
        } else {
            if !states.get(id).copied().unwrap_or(true) {
                return Ok(()); // already disabled
            }
            self.unmount(id).map_err(|e| e.to_string())?;
            states.insert(id.to_string(), false);
        }
        Ok(())
    }

    /// Whether a plugin is currently enabled (default true before any
    /// `apply_disabled` / `set_enabled`).
    pub fn is_enabled(&self, id: &str) -> bool {
        self.enabled_state
            .lock()
            .unwrap()
            .get(id)
            .copied()
            .unwrap_or(true)
    }

    /// Apply persisted disable state at boot (before mount_all).
    pub fn apply_disabled(&self, disabled: &[String]) {
        let mut states = self.enabled_state.lock().unwrap();
        for id in disabled {
            states.insert(id.clone(), false);
        }
    }

    pub fn ids(&self) -> Vec<String> {
        self.plugins.lock().unwrap().keys().cloned().collect()
    }

    pub fn plugin(&self, id: &str) -> Option<Arc<dyn Plugin>> {
        self.plugins.lock().unwrap().get(id).cloned()
    }

    /// Full descriptor list for the Panel (plugins + active state).
    pub fn descriptors(&self) -> Vec<PluginDescriptor> {
        let plugins = self.plugins.lock().unwrap();
        let states = self.enabled_state.lock().unwrap();
        let ids: Vec<String> = plugins.keys().cloned().collect();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(p) = plugins.get(&id) {
                let mut d = PluginDescriptor::from(p.manifest());
                d.enabled = states.get(&id).copied().unwrap_or(true);
                out.push(d);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{PluginKind, PluginManifest};
    use crate::plugin::{BuiltinPlugin, PluginMountError};

    #[test]
    fn register_and_descriptors() {
        let reg = PluginRegistry::new();
        reg.register(Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("t.t.t", "test", "1.0", PluginKind::Tool, "entry", "d"),
            |_| Ok(vec![]),
        )))
        .unwrap();
        let ds = reg.descriptors();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].id, "t.t.t");
        assert!(ds[0].enabled);
        assert!(ds[0].builtin);
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let reg = PluginRegistry::new();
        let p = || {
            Arc::new(BuiltinPlugin::new(
                PluginManifest::builtin("a.a.a", "x", "1", PluginKind::Skill, "", ""),
                |_| Ok(vec![]),
            ))
        };
        reg.register(p()).unwrap();
        assert!(reg.register(p()).is_err());
    }

    #[test]
    fn mount_unmount_roundtrip() {
        let reg = PluginRegistry::new();
        let ctx = MountContext::new();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = counter.clone();
        reg.register(Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("m.m.m", "m", "1", PluginKind::Tool, "", ""),
            move |_| {
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![])
            },
        )))
        .unwrap();
        reg.mount_all(&ctx);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        reg.unmount_all();
        reg.mount_all(&ctx);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn disabled_plugins_skip_mount() {
        let reg = PluginRegistry::new();
        reg.register(Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("d.d.d", "d", "1", PluginKind::Tool, "", ""),
            |_| Ok(vec![]),
        )))
        .unwrap();
        reg.apply_disabled(&["d.d.d".into()]);
        reg.mount_all(&MountContext::new());
        let ds = reg.descriptors();
        assert!(!ds[0].enabled);
    }

    #[test]
    fn mount_failure_is_reported() {
        let reg = PluginRegistry::new();
        reg.register(Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("f.f.f", "f", "1", PluginKind::Tool, "", ""),
            |_| Err(PluginMountError::Mount("boom".into())),
        )))
        .unwrap();
        let errors = reg.mount_all(&MountContext::new());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "f.f.f");
    }
}
