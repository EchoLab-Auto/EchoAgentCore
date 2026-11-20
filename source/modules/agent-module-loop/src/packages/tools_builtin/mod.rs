//! 内置工具集：宿主侧只剩**端口接线**（全模块化 S1/S3）。
//!
//! 2026-11 之前，这个文件是工具清单的**所有者**：它知道有哪些工具、
//! 自停护栏的长相、包标签怎么打。现在这些知识全在
//! [`agent_module_tools::module`]（`BuiltinToolProvider` 的实现）里——宿主
//! 只做两件它才该做的事：
//!
//! 1. **提供端口**（实例注册表 / 工作区根 / 联邦出口 / Shell 句柄 / 配置存储）；
//! 2. 把工具箱交给 `Agent`（脱敏器 / 禁用状态 / persona 门控都是宿主策略）。
//!
//! `list_api_profiles` 也在 S3 跟着搬走了（它的数据源 `api_pool` 与
//! `ApiProfile` 都下沉了）——宿主侧**不再认识任何一个内置工具的名字**。

// 工具实现已外迁到 `agent-module-tools`；这里保留原模块路径作为宿主兼容面。
/// Shell 命令域的宿主接缝（组合根实现、模块消费）。
///
/// 2026-11：`shell_*` 工具与 `*Shell*` 命令随 shell 会话一起搬进独立模块
/// `agent-module-shell`（`bash-start` / `bash-exec` / `bash-stop`）。这里保留
/// re-export 是为了宿主侧既有路径不变。
pub use agent_module_shell::commands::{
    ShellCommandHandler, ShellCommandHost as ShellCommandHostAlias,
};
/// Shell 工具集的构造契约（模块提供、宿主调用）。
pub use agent_module_shell::{
    resolve_provider as resolve_shell_provider, standalone_provider as standalone_shell_provider,
    ShellHostCtx, ShellToolProvider,
};
/// 清单工具集的构造契约（2026-11 从内置工具集拆出）。
///
/// 宿主侧只认识这个模块的**接缝**：`resolve_*` 从服务表问"挂上了吗"，
/// `standalone_*` 是模块尚未挂载时的同一实现。
pub use agent_module_checklist::{
    resolve_provider as resolve_checklist_provider,
    standalone_provider as standalone_checklist_provider, ChecklistHostCtx, ChecklistPort,
    ChecklistToolProvider,
};
/// 实例命令域的宿主接缝。
pub use agent_module_tools::instance_commands::{
    InstanceCommandHandler, InstanceCommandHost as InstanceCommandHostAlias,
};
/// 服务表解析（`tools.builtin`）：模块挂上了就有，没挂上返回 `None`。
pub use agent_module_tools::module::{resolve_provider, standalone_provider};
/// 工具管理面命令域的宿主接缝。
pub use agent_module_tools::tool_commands::{
    ToolsCommandHandler, ToolsCommandHost as ToolsCommandHostAlias,
};
pub use agent_module_tools::{
    coding, module as builtin_module, BuiltinToolProvider, BuiltinTools, ListApiProfilesTool,
    ToolsHostCtx,
};

/// 组装一个 persona 的内置工具集：**实现全在模块里**，这里只传端口。
///
/// 调用方（组合根）拿到之后照旧注入脱敏器、应用禁用状态与门控。
pub fn build_registry(
    instances: std::sync::Arc<echo_defs::module_instance::InstanceRegistry>,
    workspace: std::path::PathBuf,
    config_store: echo_config::ConfigStore,
    invoker: Option<agent_module_tools::RemoteInvoker>,
    shell: agent_module_shell::ShellHandle,
    session_query: Option<std::sync::Arc<dyn echo_defs::session_query::SessionQuery>>,
    // 本 persona 的清单端口（持久 store / 冻结失败 / None=内存 store）。
    // 由组合根按 `ChecklistRuntime` 的 per-persona store 构造。
    checklist_port: Option<agent_module_checklist::ChecklistPort>,
    // 清单工具的构造契约（2026-11 拆出 agent-module-checklist）。
    //
    // `Some(None)` = **这个部署把清单交给了独立产物**；此时不再回退
    // standalone 实现，否则内置工具会先占住 `checklist` 这个名字，外部模块
    // 的工具注册时被当成重名跳过（实测：
    // "外部模块工具与已注册工具重名，已跳过该模块工具 tool=checklist"）。
    //
    // `Some(Some(p))` = 模块已挂载，用它的 provider。
    // `None` = 装配期（模块尚未挂载），回退 standalone——这是既有行为。
    checklist_provider: Option<Option<std::sync::Arc<dyn agent_module_checklist::ChecklistToolProvider>>>,
    provider: Option<std::sync::Arc<dyn BuiltinToolProvider>>,
    shell_provider: Option<std::sync::Arc<dyn ShellToolProvider>>,
) -> crate::tool::ToolRegistry {
    let provider = provider.unwrap_or_else(agent_module_tools::module::standalone_provider);
    let cx = ToolsHostCtx {
        instances,
        workspace,
        invoker,
        // 共享配置存储：`list_api_profiles` 读供应商池的唯一入口。
        config_store: Some(config_store),
        session_query,
    };
    let mut registry = provider.registry(&cx);

    // 任务清单工具（`checklist`）**由清单模块造**（2026-11 从内置工具集拆出）：
    // 它有自己的持久化真值（per-persona store）与包标签（`echo-agent.checklist`），
    // 宿主只把端口递过去。降级口径与 shell 一致：装配期模块尚未挂载时回退到
    // standalone 实现，persona 门控与包级启停仍在运行期生效。
    //
    // `Some(None)`（部署方声明清单由独立产物提供）时**不注册任何内置清单
    // 工具**：那份能力归外部模块，宿主再造一个只会撞名。
    match checklist_provider {
        Some(None) => {}
        other => {
            let provider = match other {
                Some(Some(provider)) => provider,
                _ => agent_module_checklist::standalone_provider(),
            };
            let checklist_registry =
                provider.registry(&agent_module_checklist::ChecklistHostCtx {
                    port: checklist_port,
                });
            merge_registry(&mut registry, checklist_registry);
        }
    }

    // Shell 会话工具（`bash-start` / `bash-exec` / `bash-stop`）**由 shell 模块
    // 造**（2026-11）：它知道这些工具该打什么包标签、需要什么端口。宿主只把
    // 端口递过去并把产物并进同一个注册表——**不复制工具清单**。
    //
    // 与 `tools.builtin` 同一个降级口径：**装配期**模块尚未挂载（`make_agent`
    // 早于 `mount_plan`），此时回退到模块的 standalone 实现；"这个使用者看不
    // 看得到"由 persona 门控（`kind` 定作用域：`tool` 属 Persona 级）与
    // 包级启停负责——那是运行期的事，不该在构造工具表时表达。
    let shell_provider = shell_provider.unwrap_or_else(agent_module_shell::standalone_provider);
    let shell_registry = shell_provider.registry(&ShellHostCtx {
        shell: shell.clone(),
        guard: Some(agent_module_shell::self_stop_guard()),
    });
    merge_registry(&mut registry, shell_registry);
    registry
}

/// 把一个模块产出的工具并进宿主注册表（**连包标签一起**）。
///
/// 合并而不复制清单：宿主不认识任何一个工具名，"模块装了哪些工具、它们属于
/// 哪个包"是模块自己的知识。包标签必须跟着走——否则面板按包启停与 persona
/// 门控会作用在错误的包上（或干脆找不到它们）。
fn merge_registry(target: &mut crate::tool::ToolRegistry, source: crate::tool::ToolRegistry) {
    for name in source.names() {
        let Some(tool) = source.get(&name) else {
            continue;
        };
        target.register(tool);
        if let Some(package) = source.package_of(&name) {
            target.set_package(&name, package);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::build_registry;

    /// 内置工具集内部不能有重名：`ToolRegistry::register` 是覆盖语义，
    /// 撞名会让一个工具静默消失（工具定义还在，执行却落到另一个实现上）。
    /// 这条不变量此前靠人肉检查——踩过一次（`calculator` 与模块示例同名）。
    #[test]
    fn builtin_tools_have_unique_names() {
        let dir = std::env::temp_dir().join(format!("echo-builtin-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = echo_config::ConfigStore::new(dir.join("core.toml"));
        let registry = build_registry(
            std::sync::Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
            dir.clone(),
            store,
            None,
            agent_module_shell::ShellHandle::new(),
            None,
            None,
            None,
            None,
            // 带上 shell 模块造的工具一起查重名：两个模块产出的名字同处一张
            // 注册表，撞名会被静默覆盖（这正是本测试要抓的）。
            Some(crate::tool::builtin::standalone_shell_provider()),
        );
        let conflicts = registry.name_conflicts();
        assert!(conflicts.is_empty(), "内置工具重名: {conflicts:?}");
    }

    /// 供应商池工具随配置存储一起接线：给了 `ConfigStore` 就必须有
    /// `list_api_profiles`（它是模型挑选子代理模型时唯一的查询入口）。
    #[test]
    fn api_profile_tool_is_registered_when_the_store_is_wired() {
        let dir = std::env::temp_dir().join(format!("echo-builtin-pool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = echo_config::ConfigStore::new(dir.join("core.toml"));
        let registry = build_registry(
            std::sync::Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
            dir.clone(),
            store,
            None,
            agent_module_shell::ShellHandle::new(),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(
            registry.contains_tool("list_api_profiles"),
            "缺 list_api_profiles：{:?}",
            registry.names()
        );
    }
}
