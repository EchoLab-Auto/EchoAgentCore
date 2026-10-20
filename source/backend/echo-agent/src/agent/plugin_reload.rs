//! 数据插件热重载（plugin.toml 目录监视 → 挂载/卸载 skills/tools 类插件）。
//!
//! 2026-10 架构评审：自 `agent/mod.rs` 拆出（纯搬移，行为不变）。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;

use super::*;

impl Agent {
    /// Watch the configured plugin directory for `plugin.toml` manifests and
    /// hot-(re)mount discovered data plugins (skills/tools). Code plugins
    /// (provider/loop/instance) still require the binary-reload path: this
    /// watcher only handles data plugins.
    pub async fn start_plugin_reload_task(self: &Arc<Self>) {
        let plugins_dir = self.model.config.read().await.plugins_dir.clone();
        if plugins_dir.trim().is_empty()
            || !std::path::Path::new(&plugins_dir).exists()
            || self
                .plugin_reload_started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let agent = Arc::clone(self);
        let cancel = self.cancel.clone();
        tracing::info!(path = %plugins_dir, interval_seconds = 5, "plugin hot reload started");
        tokio::spawn(async move {
            let start = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut interval = tokio::time::interval_at(start, std::time::Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let _ = agent.reload_data_plugins(&plugins_dir).await;
                    }
                    _ = cancel.cancelled() => break,
                }
            }
        });
    }

    /// Scan `plugins_dir` for `plugin.toml` files and hot-mount/unmount data
    /// plugins based on manifest changes (simple: unmount all discovered,
    /// remount from current disk state).
    pub(super) async fn reload_data_plugins(&self, plugins_dir: &str) -> Result<(), String> {
        let dir = std::path::Path::new(plugins_dir);
        if !dir.exists() {
            return Ok(());
        }
        let mut manifests = Vec::new();
        for entry in walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if path.is_file() && path.file_name().and_then(|n| n.to_str()) == Some("plugin.toml") {
                if let Ok(manifest) = echo_plugin::manifest::load_manifest(path) {
                    // 记下定义文件路径：面板技术信息报「这个插件从哪个
                    // plugin.toml 读来的」（热重载场景下尤其重要——同一个 id
                    // 可能出现在多个目录）。
                    manifests.push((manifest, path.display().to_string()));
                }
            }
        }
        let id_set: std::collections::HashSet<String> =
            manifests.iter().map(|(m, _)| m.id.clone()).collect();
        let host = &self.plugin_host();
        // Unmount discovered plugins that no longer exist.
        for desc in host.descriptors() {
            if !desc.builtin && desc.enabled && !id_set.contains(&desc.id) {
                let _ = host.registry.unmount(&desc.id);
            }
        }
        // Mount newly seen data plugins (skill/tool kinds); manifests whose
        // content changed since the last scan are remounted (diff reload).
        for (manifest, source_path) in manifests {
            // 只有数据类插件（skill/tool）能 diff 重挂；代码插件需二进制重载。
            // `kind` 是宿主自定义标签（框架不再有枚举），所以这里直接比字面量。
            if !matches!(manifest.kind.as_str(), "skill" | "tool") {
                continue;
            }
            if desc_exists(host, &manifest.id) {
                // 已挂载：manifest 内容 diff → 重挂载（unmount + 重注册 + mount）。
                // 此前改 plugin.toml 不会有任何效果（除非删掉再放回），面板
                // 上显示的版本/描述/包归属也一直停留在首次挂载的快照。
                let current = host.descriptors().into_iter().find(|d| d.id == manifest.id);
                let stale = current.is_some_and(|d| data_plugin_stale(&d, &manifest));
                if !stale {
                    continue;
                }
                tracing::info!(id = %manifest.id, "data plugin manifest changed, remounting");
                let _ = host.registry.unmount(&manifest.id);
                let _ = host.registry.unregister(&manifest.id);
            }
            let manifest2 = manifest.clone();
            let plugin =
                std::sync::Arc::new(echo_plugin::BuiltinPlugin::new(manifest, move |_ctx| {
                    Ok(vec![])
                }));
            let _ = &manifest2;
            let definition = echo_plugin::PluginDefinition {
                source_path,
                command: String::new(),
                args: Vec::new(),
                env_keys: Vec::new(),
            };
            match host.register_and_mount(plugin) {
                Ok(()) => host
                    .registry
                    .set_definition(manifest2.id.clone(), definition),
                Err(e) => tracing::warn!(%e, "discovered plugin mount failed"),
            }
        }
        Ok(())
    }
}

pub(super) fn desc_exists(host: &crate::plugins::PluginHost, id: &str) -> bool {
    host.descriptors().iter().any(|d| d.id == id)
}

/// 已挂载的描述符与磁盘 manifest 是否有内容 diff（name/version/description/
/// author/package 任一变化都算 stale）——stale 的数据插件需要重挂载。
/// 挂在函数级便于单测（`reload_data_plugins` 本体依赖完整 Agent）。
pub(super) fn data_plugin_stale(
    current: &echo_plugin::PluginDescriptor,
    manifest: &echo_plugin::PluginManifest,
) -> bool {
    current.name != manifest.name
        || current.version != manifest.version
        || current.description != manifest.description
        || current.author != manifest.author
        || current.package != manifest.package_id()
}
