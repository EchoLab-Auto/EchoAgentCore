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
    ///
    /// **来源**维度（正交于 `runtime`）：门控 manifest 可以编译内置，
    /// 而实现跑在独立进程里。
    pub builtin: bool,
    /// 运行形态（`in_process` / `subprocess`，见 [`PluginRuntime`]）。
    ///
    /// [`PluginRuntime`]: crate::manifest::PluginRuntime
    pub runtime: String,
    /// 所属包 id（横跨 plugin+tool+skill 的标签，见
    /// [`PluginManifest::package_id`]）；未声明 = 插件 id 自身。
    #[serde(default)]
    pub package: String,
    /// Mount error description when `enabled == false && builtin`.
    #[serde(default)]
    pub error: Option<String>,
    /// 该插件在 `plugins.toml` 里由**哪一层**配置（空 = 配置未提及）。
    ///
    /// 由组合根在装载配置时写入（[`PluginRegistry::set_config_origin`]）：
    /// 分层合成（profile → bundles → patches → 行内）之后每个条目都带着
    /// 来源层，面板据此回答「这行为什么长这样」。
    #[serde(default)]
    pub config_origin: String,
    /// **运行态**（`ready` / `stopped` / `failed` / `pending` / `draining`）；
    /// 空 = 非子进程插件（进程内实现没有独立生命周期）。
    ///
    /// 与 `enabled`（挂载/启用开关）正交：`enabled = true` 但
    /// `state = failed` 是「配置开了、进程起不来」——面板要能区分。
    #[serde(default)]
    pub state: String,
    /// 崩溃后已自动重启的次数（仅子进程插件）。
    #[serde(default)]
    pub restarts: u32,
    /// 连接代际（每次就绪自增；仅子进程插件）。
    #[serde(default)]
    pub generation: u64,
    /// 插件**定义从哪读来**：`plugins.toml` 的路径，或数据插件的
    /// `plugin.toml` 路径；空 = 编译内置（无外部定义文件）。
    ///
    /// 排查用：「我改的到底是不是这个文件」——比 `config_origin`（只说哪一层）
    /// 更具体，后者是线格式里的层名，这里是磁盘路径。
    #[serde(default)]
    pub source_path: String,
    /// 子进程插件的启动命令（空 = 进程内实现 / 未声明）。
    ///
    /// 与 `error` 配合回答「为什么起不来」：命令拼错、路径不存在、参数漏了
    /// ——面板不必让人翻 `plugins.toml` 就能看出意图。
    #[serde(default)]
    pub command: String,
    /// 启动参数（`plugins.toml` 的 `args`）。
    #[serde(default)]
    pub args: Vec<String>,
    /// 环境变量的**键名**（值不下发）。
    ///
    /// 只给键：排查「我是不是漏传了某个变量」够用，而值里常有 token /
    /// 密钥，上协议即等于把它们送到前端内存与日志里。
    #[serde(default)]
    pub env_keys: Vec<String>,
}

/// 插件的**定义来源**（面板技术信息展示用）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginDefinition {
    /// 定义文件路径（`plugins.toml` / 数据插件的 `plugin.toml`）。
    pub source_path: String,
    /// 子进程启动命令（进程内实现为空）。
    pub command: String,
    /// 启动参数。
    pub args: Vec<String>,
    /// 环境变量**键名**（值不下发：常含 token / 密钥）。
    pub env_keys: Vec<String>,
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
            builtin: m.builtin,
            runtime: m.runtime.as_str().to_string(),
            package: m.package_id().to_string(),
            error: None,
            config_origin: String::new(),
            state: String::new(),
            restarts: 0,
            generation: 0,
            source_path: String::new(),
            command: String::new(),
            args: Vec::new(),
            env_keys: Vec::new(),
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
    /// 插件 id → 声明它的配置层（展示用；见 [`PluginRegistry::set_config_origin`]）。
    config_origins: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// 插件 id → 装配/启动失败原因（展示用；`None` 值表示"曾失败后已恢复"）。
    errors: std::sync::Mutex<BTreeMap<String, String>>,
    /// 插件 id → 「定义从哪来 / 怎么启动」（展示用；见
    /// [`PluginRegistry::set_definition`]）。
    definitions: std::sync::Mutex<BTreeMap<String, PluginDefinition>>,
    /// 注册顺序（`descriptors()` 按它输出）。
    ///
    /// 为什么单独存：底层 `plugins` 是 `BTreeMap`（按 id 排序），而
    /// `plugins.toml` 的**行序 = 挂载顺序**是有意义的配置表达（bundle 里
    /// 靠前的先挂）。按 id 排序会把这条信息丢掉。
    order: std::sync::Mutex<Vec<String>>,
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
        let mut plugins = self
            .plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if plugins.contains_key(&id) {
            return Err(format!("plugin id already registered: {id}"));
        }
        plugins.insert(id.clone(), Arc::clone(&plugin));
        drop(plugins);
        self.order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id);
        Ok(())
    }

    /// Remove a registered (and unmounted) plugin by id.
    pub fn unregister(&self, id: &str) -> bool {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.contains_key(id) {
            return false; // must be unmounted first
        }
        drop(active);
        self.order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|existing| existing != id);
        self.plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
            .is_some()
    }

    /// Activate a plugin: run its mount hooks and retain disposers.
    pub fn mount(&self, id: &str, ctx: &MountContext) -> PluginMountResult {
        let plugin = {
            let plugins = self
                .plugins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.insert(id.to_string(), disposers);
        Ok(Vec::new())
    }

    /// Deactivate a plugin: dispose all registrations (reversible effects).
    pub fn unmount(&self, id: &str) -> Result<(), PluginMountError> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let plugins = self
                .plugins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let active = self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active.keys().cloned().collect()
        };
        for id in ids {
            let _ = self.unmount(&id);
        }
    }

    /// Runtime enable/disable. Disabling unmounts; enabling remounts.
    /// `persisted` is the caller's disk-state (e.g. `[agent].disabled_plugins`).
    pub fn set_enabled(&self, id: &str, enabled: bool, ctx: &MountContext) -> Result<(), String> {
        let exists = self
            .plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(id);
        if !exists {
            return Err(format!("plugin not found: {id}"));
        }
        let mut states = self
            .enabled_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .copied()
            .unwrap_or(true)
    }

    /// Apply persisted disable state at boot (before mount_all).
    pub fn apply_disabled(&self, disabled: &[String]) {
        let mut states = self
            .enabled_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in disabled {
            states.insert(id.clone(), false);
        }
    }

    /// 全部已注册插件 id（**注册序**，与 [`PluginRegistry::descriptors`] 一致）。
    pub fn ids(&self) -> Vec<String> {
        self.descriptors().into_iter().map(|d| d.id).collect()
    }

    pub fn plugin(&self, id: &str) -> Option<Arc<dyn Plugin>> {
        self.plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    /// Full descriptor list for the Panel (plugins + active state).
    pub fn descriptors(&self) -> Vec<PluginDescriptor> {
        let plugins = self
            .plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let states = self
            .enabled_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 注册序（= 组合顺序 / config 行序），不是 id 字典序。
        let ids: Vec<String> = {
            let order = self
                .order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut ids: Vec<String> = order
                .iter()
                .filter(|id| plugins.contains_key(*id))
                .cloned()
                .collect();
            // 防御：绕过 register 塞进 map 的（不该存在）补在末尾，不丢条目。
            let mut seen: std::collections::HashSet<String> = ids.iter().cloned().collect();
            for id in plugins.keys() {
                if seen.insert(id.clone()) {
                    ids.push(id.clone());
                }
            }
            ids
        };
        let mut out = Vec::with_capacity(ids.len());
        let origins = self.config_origins();
        let definitions = self
            .definitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let errors = self
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for id in ids {
            if let Some(p) = plugins.get(&id) {
                let mut d = PluginDescriptor::from(p.manifest());
                d.enabled = states.get(&id).copied().unwrap_or(true);
                if let Some(origin) = origins.get(&id) {
                    d.config_origin = origin.clone();
                }
                if let Some(error) = errors.get(&id) {
                    d.error = Some(error.clone());
                }
                if let Some(definition) = definitions.get(&id) {
                    d.source_path = definition.source_path.clone();
                    d.command = definition.command.clone();
                    d.args = definition.args.clone();
                    d.env_keys = definition.env_keys.clone();
                }
                out.push(d);
            }
        }
        out
    }

    /// 记录「该插件由配置的哪一层声明」（组合根装载 `plugins.toml` 后写入）。
    ///
    /// 只影响展示（`PluginDescriptor::config_origin`）——运行期语义仍由
    /// 启停状态与 mount 计划决定。
    pub fn set_config_origin(&self, id: impl Into<String>, origin: impl Into<String>) {
        self.config_origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.into(), origin.into());
    }

    /// 批量写入来源层（组合根一次装载多处声明时用）。
    pub fn set_config_origins<I, K, V>(&self, origins: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut map = self
            .config_origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (id, origin) in origins {
            map.insert(id.into(), origin.into());
        }
    }

    /// 记录「定义从哪来 / 怎么启动」（面板技术信息用）。
    ///
    /// 只影响展示：运行期行为仍由 manifest 与配置决定。
    pub fn set_definition(&self, id: impl Into<String>, definition: PluginDefinition) {
        self.definitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.into(), definition);
    }

    /// 记录插件装配/启动失败原因（组合根在启动子进程 / 装载数据插件后写入）。
    ///
    /// 面板据此区分「用户主动禁用」与「装配失败被跳过」——两者
    /// `enabled` 都是 false，但后者要人去修配置。
    pub fn set_plugin_error(&self, id: impl Into<String>, error: impl Into<String>) {
        self.errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.into(), error.into());
    }

    fn config_origins(&self) -> std::collections::HashMap<String, String> {
        self.config_origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 重载一个已启用的插件：卸下（执行全部 disposer）再重新挂载。
    ///
    /// 与 `set_enabled(false) → set_enabled(true)` 的差别：**不动启用状态**
    /// （不写 `disabled_plugins`、不触发持久化），因此可以安全地用于
    /// "让某个插件按新配置/新外部状态重新初始化"。已禁用的插件直接报错
    /// ——重启的前提是先启用。
    pub fn restart(&self, id: &str, ctx: &MountContext) -> Result<(), String> {
        if !self.is_enabled(id) {
            return Err(format!(
                "plugin {id} is disabled; enable it before restarting"
            ));
        }
        self.unmount(id).map_err(|e| e.to_string())?;
        self.mount(id, ctx).map(|_| ()).map_err(|e| e.to_string())
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

    /// `restart` = 卸下 + 重新挂载，**不动**启用状态；
    /// 禁用的插件不允许重启（前提是先启用）。
    #[test]
    fn restart_remounts_without_touching_enabled_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reg = PluginRegistry::new();
        let ctx = MountContext::new();
        let mounts = Arc::new(AtomicUsize::new(0));
        let disposes = Arc::new(AtomicUsize::new(0));
        let (m, d) = (mounts.clone(), disposes.clone());
        reg.register(Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("r.r.r", "r", "1", PluginKind::Tool, "", ""),
            move |_| {
                m.fetch_add(1, Ordering::SeqCst);
                let d = d.clone();
                Ok(vec![Disposer::from_fn(move || {
                    d.fetch_add(1, Ordering::SeqCst);
                })])
            },
        )))
        .unwrap();
        reg.mount_all(&ctx);
        assert_eq!(mounts.load(Ordering::SeqCst), 1);

        reg.restart("r.r.r", &ctx).expect("restart enabled plugin");
        assert_eq!(mounts.load(Ordering::SeqCst), 2, "应重新挂载一次");
        assert_eq!(
            disposes.load(Ordering::SeqCst),
            1,
            "旧挂载的 disposer 应执行"
        );
        // 启用状态未变（restart 不是 set_enabled 的别名）。
        assert!(reg.is_enabled("r.r.r"));
        assert!(reg.descriptors()[0].enabled);

        // 禁用后拒绝重启。
        reg.set_enabled("r.r.r", false, &ctx).unwrap();
        let err = reg.restart("r.r.r", &ctx).expect_err("disabled plugin");
        assert!(err.contains("disabled"), "{err}");
    }

    /// 注册序 = 输出序：`plugins.toml` 的行序（= 挂载顺序）是有意义的配置
    /// 表达，`descriptors()` 不能按 id 字典序重排它。
    #[test]
    fn descriptors_follow_registration_order() {
        let reg = PluginRegistry::new();
        for id in ["zzz.last", "aaa.first", "mmm.middle"] {
            reg.register(Arc::new(BuiltinPlugin::new(
                PluginManifest::builtin(id, id, "1", PluginKind::Tool, "", ""),
                |_| Ok(vec![]),
            )))
            .unwrap();
        }
        let ids: Vec<String> = reg.descriptors().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["zzz.last", "aaa.first", "mmm.middle"]);
        // ids() 与 descriptors() 同序（同一份真值）。
        assert_eq!(reg.ids(), ids);
        // 注销后顺序里也不留空洞。
        reg.unregister("aaa.first");
        assert_eq!(reg.ids(), vec!["zzz.last", "mmm.middle"]);
    }

    /// 装配失败原因进 descriptor：面板据此区分「用户主动禁用」（error 空）
    /// 与「启动失败被跳过」（error 有值）。
    #[test]
    fn plugin_errors_reach_descriptors() {
        let reg = PluginRegistry::new();
        reg.register(Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("e.e.e", "e", "1", PluginKind::Tool, "", ""),
            |_| Ok(vec![]),
        )))
        .unwrap();
        assert_eq!(reg.descriptors()[0].error, None);
        reg.set_plugin_error("e.e.e", "spawn failed: No such file or directory");
        assert_eq!(
            reg.descriptors()[0].error.as_deref(),
            Some("spawn failed: No such file or directory")
        );
    }

    /// 定义来源进 descriptor：来源文件 / 启动命令 / 参数 / 环境变量键名。
    ///
    /// 只下发 env 的**键**：值里常带 token，上协议等于送进前端内存与日志。
    #[test]
    fn definitions_reach_descriptors_without_env_values() {
        let reg = PluginRegistry::new();
        reg.register(Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("x.ext", "x", "1", PluginKind::Tool, "", "")
                .with_runtime(crate::PluginRuntime::Subprocess)
                .with_builtin_flag(false),
            |_| Ok(vec![]),
        )))
        .unwrap();
        // 缺省：无定义来源（编译内置的形态）。
        assert_eq!(reg.descriptors()[0].source_path, "");
        assert_eq!(reg.descriptors()[0].command, "");

        reg.set_definition(
            "x.ext",
            PluginDefinition {
                source_path: "/etc/echo/plugins.toml".into(),
                command: "/opt/x/plugin".into(),
                args: vec!["--mode".into(), "fast".into()],
                env_keys: vec!["X_TOKEN".into(), "X_HOME".into()],
            },
        );
        let d = &reg.descriptors()[0];
        assert_eq!(d.source_path, "/etc/echo/plugins.toml");
        assert_eq!(d.command, "/opt/x/plugin");
        assert_eq!(d.args, vec!["--mode", "fast"]);
        assert_eq!(d.env_keys, vec!["X_TOKEN", "X_HOME"]);
    }

    /// 配置来源层进 descriptor（面板展示「来自哪一层」）。
    #[test]
    fn config_origin_flags_reach_descriptors() {
        let reg = PluginRegistry::new();
        reg.register(Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("o.o.o", "o", "1", PluginKind::Tool, "", ""),
            |_| Ok(vec![]),
        )))
        .unwrap();
        // 缺省：配置未提及。
        assert_eq!(reg.descriptors()[0].config_origin, "");
        reg.set_config_origins([("o.o.o", "base.toml · layer 0")]);
        assert_eq!(reg.descriptors()[0].config_origin, "base.toml · layer 0");
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
