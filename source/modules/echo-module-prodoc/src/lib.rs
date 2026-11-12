//! # echo-module-prodoc — ProDoc 文档群模块
//!
//! 把一棵 Markdown 目录读成 **ProDoc 文档图**所需的 `files` 映射
//! （`相对路径 → 完整原文`），交给 Panel 的文档画布渲染。
//!
//! ## 为什么是个模块，而不是 Panel 的一部分
//!
//! 渲染在浏览器里，读盘在 Core 里。ProDoc 原本是独立 CLI（`echo-prodoc view`），
//! 自带一个 Vite dev server 负责读盘 + 写盘 + 热更新；要嵌进 Panel，这三件事
//! 必须换成宿主提供的通道。**读盘属于 Core**——Panel 是独立进程、没有文件系统
//! 权限模型，也不该为了看个文档去开文件接口。
//!
//! ## 与 ProDoc CLI 的差异（刻意为之）
//!
//! | 项 | CLI（`server.ts`） | 本模块 |
//! |---|---|---|
//! | 扫描上限 | 无 | 单文件 1 MB / 2000 篇 / 总计 32 MB |
//! | 跳过目录 | 5 个固定名 | 同 5 个 + 跳过隐藏目录 |
//! | 交付 | 内联进虚拟模块源码 | 模块事件（`ModuleEvent`） |
//! | 写盘 | 有（save / delete + diff3 合并） | 有（save / delete；**合并交前端**） |
//!
//! 上限是必须的：CLI 是一次性页面加载，超量顶多慢；本模块走 WS 推送，一个
//! 大目录能瞬间生成几十 MB 的事件消息，把面板的模块事件环形缓冲（200 条）
//! 撑爆。超限**明确报错并列出被跳过的文件**，绝不静默截断——截断的文档图会
//! 缺节点，而用户看不出是"被截了"还是"本来就没写"。
//!
//! ## 事件协议（本模块 → 宿主 → 面板）
//!
//! 模块发 [`events::BUNDLE`]（`prodoc.bundle`），宿主不认识它，于是原样包成
//! `BackendEvent::ModuleEvent` 推给面板；扩展在 `state.moduleEvents` 里按名字取。
//! 这条路是**现成的**——不需要给线协议加变体，也就不需要改 Panel 的类型定义。

pub mod bundle;
pub mod check;
pub mod commands;
pub mod paths;
pub mod write;

use std::sync::{Arc, RwLock};

use echo_module::{BuiltinModule, Module, ModuleManifest};
use serde_json::{json, Value};

/// 模块 id（宿主 `builtin_modules` 目录与门控都用它）。
pub const MODULE_ID: &str = "echo-agent.prodoc";

/// 模块事件名（见模块文档）。
pub mod events {
    /// 文档群快照：载荷 = [`crate::bundle::Bundle`] + `team_id`。
    pub const BUNDLE: &str = "prodoc.bundle";
    /// 失败：载荷 = `{ team_id, root, error }`（面板显示为错误条，不是空白画布）。
    pub const ERROR: &str = "prodoc.error";
    /// 可打开的根列表 + 生效上限（`ProDocRoots` 的**唯一**出口）。
    pub const ROOTS: &str = "prodoc.roots";
    /// 目录选择器的一层（`BrowseProDocDirs` 的**唯一**出口）。
    pub const DIRS: &str = "prodoc.dirs";
    /// 单文件读取回执（`ReadProDocFile`）。
    pub const FILE: &str = "prodoc.file";
    /// 保存回执（`SaveProDocFile`）：成功 / 冲突（附磁盘当前内容）。
    pub const SAVED: &str = "prodoc.saved";
    /// 删除回执（`DeleteProDocFile`）。
    pub const DELETED: &str = "prodoc.deleted";
    /// 检查报告（`ProDocCheck`）。
    pub const REPORT: &str = "prodoc.report";
}

/// 写操作回执（`prodoc.file` / `prodoc.saved` / `prodoc.deleted` 的共同形状）。
///
/// 用同一个结构而不是三套：三种操作的判定分支高度重叠（内容比对 → 冲突时
/// 附磁盘内容），共用一份便于"改造一处、三处同时正确"。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WriteAck {
    /// 面板生成的关联 id（原样带回）。
    pub request_id: Option<String>,
    pub team_id: Option<String>,
    /// 文档根（绝对路径）。
    pub root: String,
    /// 相对路径。
    pub path: String,
    /// 操作是否成功。
    pub ok: bool,
    /// 失败原因（人话，直接显示）。
    pub error: Option<String>,
    /// 是否需要前端合并：磁盘内容既不是 `base` 也不是本次提交内容。
    pub conflict: bool,
    /// 磁盘当前内容（冲突时前端据此跑 diff3；也用于并排对照）。
    ///
    /// `None` = 文件不存在（冲突原因之一是"磁盘上已被删除"）。
    pub disk: Option<String>,
    /// 读取结果（仅 `ReadProDocFile`）。
    pub content: Option<String>,
}

impl WriteAck {
    fn to_json(&self) -> Value {
        json!({
            "request_id": self.request_id,
            "team_id": self.team_id,
            "root": self.root,
            "path": self.path,
            "ok": self.ok,
            "error": self.error,
            "conflict": self.conflict,
            "disk": self.disk,
            "content": self.content,
        })
    }
}

/// 一个可打开的文档根（面板下拉框的一项）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RootEntry {
    /// 显示名（路径最后一段）。
    pub label: String,
    /// 绝对路径。
    pub path: String,
}

/// 目录选择器的一层条目（只列子目录——选的是"文档根"，不是文件）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// 生效的扫描上限（面板显示「最多 2000 篇 / 单文件 1 MB」这类说明）。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Caps {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Caps {
    pub fn of(config: &Config) -> Self {
        Self {
            max_files: config.max_files,
            max_file_bytes: config.max_file_bytes,
            max_total_bytes: config.max_total_bytes,
        }
    }
}

/// 模块配置（`modules.toml` 的 `[module.config]`）。
///
/// ```toml
/// [module.config]
/// # 允许打开的文档根（绝对路径）。空 = 只允许各人格的工作区目录与 HOME。
/// roots = ["/home/me/docs", "/srv/wiki"]
/// # 上限可按需放宽（缺省即下方 `Default`）。
/// max_file_bytes = 1048576
/// max_files = 2000
/// max_total_bytes = 33554432
/// ```
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// 允许打开的根目录（面板的目录选择器只能在这些树下走）。
    ///
    /// 刻意**不**默认放开整个文件系统：文档根一旦可任意指定，模块就成了一个
    /// "读任意 `.md`"的接口，而 Core 的工作区目录集合本来就有明确边界。
    pub roots: Vec<String>,
    /// 单文件字节上限（UTF-8 原文长度）。
    pub max_file_bytes: u64,
    /// 文件篇数上限。
    pub max_files: usize,
    /// 全部文件的总字节上限。
    pub max_total_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            max_file_bytes: 1024 * 1024,
            max_files: 2000,
            max_total_bytes: 32 * 1024 * 1024,
        }
    }
}

/// 模块运行期状态。
///
/// 文档群是**无状态**的：每次请求都从磁盘重新读（文档目录可能被 git 切分支、
/// 被别的编辑器改）。这里只存配置与事件出口，不缓存文件内容——缓存会让
/// "磁盘变了但面板没刷新"变成常态。
pub struct ModuleState {
    config: RwLock<Config>,
    emit: RwLock<Option<echo_module::EventSink>>,
}

impl Default for ModuleState {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl ModuleState {
    pub fn new(config: Config) -> Self {
        Self {
            config: RwLock::new(config),
            emit: RwLock::new(None),
        }
    }

    /// 当前配置快照。
    pub fn config(&self) -> Config {
        self.config
            .read()
            .map(|c| c.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// 覆盖配置（`mount` 时由 `modules.toml` 注入）。
    pub fn set_config(&self, config: Config) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config;
    }

    /// 注入事件出口（`mount` 时一次）。
    pub fn set_event_sink(&self, sink: echo_module::EventSink) {
        *self.emit.write().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    /// 发一条模块事件（无出口时静默丢弃——单测 / 裸装配场景）。
    pub fn emit_event(&self, name: &str, payload: Value) {
        let sink = self.emit.read().ok().and_then(|slot| slot.as_ref().cloned());
        if let Some(sink) = sink {
            sink(echo_module::ModuleEvent::new(MODULE_ID, name, payload));
        }
    }

    /// 推一份文档群快照。
    pub fn emit_bundle(&self, bundle: &bundle::Bundle) {
        self.emit_event(
            events::BUNDLE,
            json!({
                "team_id": bundle.team_id,
                "root": bundle.root,
                "files": bundle.files,
                "skipped": bundle.skipped,
                "stats": bundle.stats,
            }),
        );
    }

    /// 推可打开的根列表。
    pub fn emit_roots(&self, roots: &[RootEntry], caps: &Caps) {
        self.emit_event(
            events::ROOTS,
            json!({
                "roots": roots,
                "caps": {
                    "max_files": caps.max_files,
                    "max_file_bytes": caps.max_file_bytes,
                    "max_total_bytes": caps.max_total_bytes,
                },
            }),
        );
    }

    /// 推目录选择器的一层。
    pub fn emit_dirs(
        &self,
        path: Option<&str>,
        roots: &[RootEntry],
        entries: &[DirEntry],
    ) {
        self.emit_event(
            events::DIRS,
            json!({
                "path": path,
                "roots": roots,
                "entries": entries,
            }),
        );
    }

    /// 推一条写操作回执（读取 / 保存 / 删除共用同一条管道）。
    ///
    /// `request_id` 由面板生成并原样带回：三条写命令的载荷形状几乎相同，而
    /// 事件**没有**天然的关联键（一个文件可以连续存两次）。靠 `(root, path)`
    /// 匹配会在快速连点时错配——面板补丁数据要落到刚提交的那一份，而不是
    /// 上一次的。
    pub fn emit_write(&self, event: &str, payload: WriteAck) {
        self.emit_event(event, payload.to_json());
    }

    /// 推一份检查报告。
    ///
    /// 报告里既有**错误也有警告**，所以不复用 `emit_error`：错误事件在面板上
    /// 是"这次操作失败了"的红条，而检查有问题不代表命令失败——文档本来就允许
    /// 有未完成的草稿。
    pub fn emit_report(&self, team_id: Option<&str>, root: &str, report: &check::Report) {
        self.emit_event(
            events::REPORT,
            json!({
                "team_id": team_id,
                "root": root,
                "stats": report.stats,
                "issues": report.issues,
                "ok": !report.has_errors(),
            }),
        );
    }

    /// 推一条失败（面板据此显示错误条）。
    pub fn emit_error(&self, team_id: Option<&str>, root: &str, message: impl Into<String>) {
        self.emit_event(
            events::ERROR,
            json!({
                "team_id": team_id,
                "root": root,
                "error": message.into(),
            }),
        );
    }
}

/// 模块清单。
pub fn manifest(version: &str) -> ModuleManifest {
    ModuleManifest::builtin(
        MODULE_ID,
        "ProDoc 文档群",
        version,
        "tool",
        "prodoc",
        "把 Markdown 目录渲染成可交互的文档图画布（ProDoc）",
    )
}

/// 构造模块（宿主装配期调用一次）。
pub fn module(version: &str) -> Arc<dyn Module> {
    module_with_state(version).0
}

/// 构造模块并交出内部状态句柄（宿主测试 / 嵌入式集成用）。
pub fn module_with_state(version: &str) -> (Arc<dyn Module>, Arc<ModuleState>) {
    let state = Arc::new(ModuleState::default());
    let mount_state = state.clone();
    let command_state = state.clone();

    let built = Arc::new(
        BuiltinModule::new(manifest(version), move |ctx| {
            // 配置解析失败不挡启动（与宿主「配置缺失不阻断开机」同款取舍）。
            let config: Config = if ctx.config().is_null() {
                Config::default()
            } else {
                match serde_json::from_value(ctx.config().clone()) {
                    Ok(config) => config,
                    Err(error) => {
                        tracing::warn!(
                            module = MODULE_ID,
                            %error,
                            "prodoc 模块配置解析失败，按缺省上限继续"
                        );
                        Config::default()
                    }
                }
            };
            tracing::info!(
                module = MODULE_ID,
                roots = config.roots.len(),
                max_files = config.max_files,
                "prodoc 模块已挂载"
            );
            mount_state.set_config(config);
            if let Some(sink) = &ctx.events {
                mount_state.set_event_sink(sink.clone());
            }
            Ok(Vec::new())
        })
        .with_commands(commands::COMMANDS.iter().copied())
        .with_command_handler_async(move |name, payload| {
            let state = command_state.clone();
            Box::pin(async move {
                match commands::handle(&state, &name, payload).await {
                    Some(outcome) => outcome,
                    None => Err(format!("命令 {name} 未被 prodoc 模块认领")),
                }
            })
        }),
    );
    (built, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 缺省上限必须存在且彼此自洽：单文件上限不能大于总量上限，
    /// 否则"允许单文件 100MB、总量 32MB"这种配置会让报错文案自相矛盾。
    #[test]
    fn default_caps_are_coherent() {
        let config = Config::default();
        assert!(config.max_file_bytes > 0);
        assert!(config.max_file_bytes <= config.max_total_bytes);
        assert!(config.max_files > 0);
        assert!(config.roots.is_empty(), "缺省不放开任何显式根");
    }

    /// 配置里写错字段名要报错而不是静默忽略——静默忽略会让人以为
    /// `max_file` 生效了。
    #[test]
    fn unknown_config_field_is_rejected() {
        let error = serde_json::from_value::<Config>(json!({"max_file": 10}));
        assert!(error.is_err());
    }

    /// 没有事件出口时 emit 不 panic（裸装配 / 单测场景）。
    #[test]
    fn emit_without_sink_is_silent() {
        let state = ModuleState::default();
        state.emit_error(Some("t"), "/docs", "boom");
        state.emit_bundle(&bundle::Bundle::default());
    }
}
