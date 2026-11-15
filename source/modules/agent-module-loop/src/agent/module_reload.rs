//! 数据模块热重载（module.toml 目录监视 → 挂载/卸载 skills/tools 类模块）。
//!
//! 2026-10 架构评审：自 `agent/mod.rs` 拆出（纯搬移，行为不变）。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;

use super::*;

impl Agent {
    /// Watch the configured module directory for `module.toml` manifests and
    /// hot-(re)mount discovered data modules (skills/tools). Code modules
    /// (provider/loop/instance) still require the binary-reload path: this
    /// watcher only handles data modules.
    /// 模块热重载任务的**任务体**（由 [`Agent::start_background_tasks`] 拉起）。
    ///
    /// 这里不再自己 `tokio::spawn` + 登记句柄（2026-11 C 复审）：
    /// - 自己登记的话，"读配置 / 判退场"的那一小段不在册——退场 `take()`
    ///   漏 join 它，之后它还可能把模块挂上一个已下线的实例；
    /// - 句柄统一由启动入口在**拉起处**登记，退场 join 的名单 = 真正跑着的任务。
    ///
    /// 退场 / 已取消时立刻返回（停止状态不可重启：这条任务属于那一代，不能
    /// 被迟到路径复活）。
    pub(crate) async fn run_module_reload_task(self: &Arc<Self>) {
        let cancel = self.background.cancel.clone();
        if self.is_retired() || cancel.is_cancelled() {
            tracing::debug!("module hot reload not started: persona retired");
            return;
        }
        // 本 watcher 的读侧临界区共享这把锁：每轮扫描 / mount 与初次读配置
        // 都必须拿读锁；升级 pause 拿写锁时会等本轮真正结束。
        let writers = Arc::clone(&self.turns.upgrade_writers);
        // 初次读配置也是读侧操作。**只罩启动判定，不把 guard 留到 loop**：
        // 留到 loop 就等于整生命周期持锁，pause 永远拿不到写锁。
        let modules_dir = {
            let _startup_writers = tokio::select! {
                guard = writers.read() => guard,
                _ = cancel.cancelled() => return,
            };
            let modules_dir = self.model.config.read().await.modules_dir.clone();
            if modules_dir.trim().is_empty()
                || !std::path::Path::new(&modules_dir).exists()
                || self
                    .modules
                    .reload_started
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            {
                return;
            }
            modules_dir
        };
        // 读配置是 await 点：退场可能刚好发生在上面那条检查之后。
        if self.is_retired() || cancel.is_cancelled() {
            tracing::debug!("module hot reload not started: persona retired");
            return;
        }
        // 本代后台任务的令牌（退场时统一取消；`turns.cancel` 只负责在飞 turn）。
        tracing::info!(path = %modules_dir, interval_seconds = 5, "module hot reload started");
        let start = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut interval = tokio::time::interval_at(start, std::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // 等读锁可能被 pause 挡住：select 取消令牌，退场时可退出。
                    // guard 持有到本轮 discover + mutate/mount（含 emit）全部结束，
                    // 然后随分支作用域释放——下一轮 tick 重新拿，绝不复用整段空闲期。
                    let _writers = tokio::select! {
                        guard = writers.read() => guard,
                        _ = cancel.cancelled() => break,
                    };
                    let _ = self.reload_data_modules(&modules_dir).await;
                }
                _ = cancel.cancelled() => break,
            }
        }
    }

    /// Scan `modules_dir` for `module.toml` files and hot-mount/unmount data
    /// modules based on manifest changes (simple: unmount all discovered,
    /// remount from current disk state).
    pub(super) async fn reload_data_modules(&self, modules_dir: &str) -> Result<(), String> {
        let dir = std::path::Path::new(modules_dir);
        if !dir.exists() {
            return Ok(());
        }
        let mut manifests = Vec::new();
        for entry in walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if path.is_file() && path.file_name().and_then(|n| n.to_str()) == Some("module.toml") {
                if let Ok(manifest) = echo_module::manifest::load_manifest(path) {
                    // 记下定义文件路径：面板技术信息报「这个模块从哪个
                    // module.toml 读来的」（热重载场景下尤其重要——同一个 id
                    // 可能出现在多个目录）。
                    manifests.push((manifest, path.display().to_string()));
                }
            }
        }
        let id_set: std::collections::HashSet<String> =
            manifests.iter().map(|(m, _)| m.id.clone()).collect();
        let host = &self.module_host();
        // Unmount discovered modules that no longer exist.
        for desc in host.descriptors() {
            if !desc.builtin && desc.enabled && !id_set.contains(&desc.id) {
                let _ = host.registry.unmount(&desc.id);
            }
        }
        // Mount newly seen data modules (skill/tool kinds); manifests whose
        // content changed since the last scan are remounted (diff reload).
        for (manifest, source_path) in manifests {
            // 只有数据类模块（skill/tool）能 diff 重挂；代码模块需二进制重载。
            // `kind` 是宿主自定义标签（框架不再有枚举），所以这里直接比字面量。
            if !matches!(manifest.kind.as_str(), "skill" | "tool") {
                continue;
            }
            if desc_exists(host, &manifest.id) {
                // 已挂载：manifest 内容 diff → 重挂载（unmount + 重注册 + mount）。
                // 此前改 module.toml 不会有任何效果（除非删掉再放回），面板
                // 上显示的版本/描述/包归属也一直停留在首次挂载的快照。
                let current = host.descriptors().into_iter().find(|d| d.id == manifest.id);
                let stale = current.is_some_and(|d| data_module_stale(&d, &manifest));
                if !stale {
                    continue;
                }
                tracing::info!(id = %manifest.id, "data module manifest changed, remounting");
                let _ = host.registry.unmount(&manifest.id);
                let _ = host.registry.unregister(&manifest.id);
            }
            let manifest2 = manifest.clone();
            let module =
                std::sync::Arc::new(echo_module::BuiltinModule::new(manifest, move |_ctx| {
                    Ok(vec![])
                }));
            let _ = &manifest2;
            let definition = echo_module::ModuleDefinition {
                source_path,
                command: String::new(),
                args: Vec::new(),
                env_keys: Vec::new(),
            };
            match host.register_and_mount(module) {
                Ok(()) => host
                    .registry
                    .set_definition(manifest2.id.clone(), definition),
                Err(e) => tracing::warn!(%e, "discovered module mount failed"),
            }
        }
        Ok(())
    }
}

pub(super) fn desc_exists(host: &crate::modules::ModuleHost, id: &str) -> bool {
    host.descriptors().iter().any(|d| d.id == id)
}

/// 已挂载的描述符与磁盘 manifest 是否有内容 diff（name/version/description/
/// author/package 任一变化都算 stale）——stale 的数据模块需要重挂载。
/// 挂在函数级便于单测（`reload_data_modules` 本体依赖完整 Agent）。
pub(super) fn data_module_stale(
    current: &echo_module::ModuleDescriptor,
    manifest: &echo_module::ModuleManifest,
) -> bool {
    current.name != manifest.name
        || current.version != manifest.version
        || current.description != manifest.description
        || current.author != manifest.author
        || current.package != manifest.package_id()
}
