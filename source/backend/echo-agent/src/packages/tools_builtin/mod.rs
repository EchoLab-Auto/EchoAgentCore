//! 内置工具集：宿主侧只剩**端口接线**（全插件化 S1）。
//!
//! 2026-11 之前，这个文件是工具清单的**所有者**：它知道有哪些工具、
//! 自停护栏的长相、包标签怎么打。现在这些知识全在
//! [`echo_plugin_tools::plugin`]（`BuiltinToolProvider` 的实现）里——宿主
//! 只做三件它才该做的事：
//!
//! 1. **提供端口**（实例注册表 / 工作区根 / 联邦出口 / Shell 句柄）；
//! 2. **注册插件自己不做的那几个工具**（`list_api_profiles` 依赖宿主的
//!    供应商池读取助手，见 [`api_profiles`]——那是 S1b 的债，见下）；
//! 3. 把工具箱交给 `Agent`（脱敏器 / 禁用状态 / persona 门控都是宿主策略）。
//!
//! ## 为什么还留着一个 `build_registry`
//!
//! 组合根里 `core_agent` 的创建早于插件挂载（插件清单在它之后装配），
//! 所以那一个 agent 拿不到服务表里的 provider。两条路径落到**同一份实现**
//! （`echo_plugin_tools::plugin::BuiltinTools`），不是两套代码：
//! 一个是"从服务表解析"，一个是"直接构造"。等插件挂载顺序前移
//! （S1b：把插件清单块提到 agent factory 之前），后者即可删除。
//!
//! `list_api_profiles` 暂留宿主的原因：它要把 `core.toml` 的
//! `[agent].api_profiles` 解析成 `AgentConfig` 的 `ApiProfile`，而这两个类型
//! 还在 `echo-agent::config` / `echo-agent::api_pool`。等它们下沉到
//! `echo-config`（S1b），这个工具就跟着进插件，本文件只剩端口接线。

pub mod api_profiles;

// 工具实现已外迁到 `echo-plugin-tools`；这里保留原模块路径作为宿主兼容面。
pub use echo_plugin_tools::{coding, plugin as builtin_plugin, BuiltinToolProvider, BuiltinTools, ToolsHostCtx};
/// 服务表解析（`tools.builtin`）：插件挂上了就有，没挂上返回 `None`。
pub use echo_plugin_tools::plugin::{resolve_provider, standalone_provider};
/// Shell 命令域的宿主接缝（组合根实现、插件消费）。
pub use echo_plugin_tools::shell_commands::{
    ShellCommandHost as ShellCommandHostAlias, ShellCommandHandler,
};
/// 实例命令域的宿主接缝。
pub use echo_plugin_tools::instance_commands::{
    InstanceCommandHost as InstanceCommandHostAlias, InstanceCommandHandler,
};

/// 组装一个 persona 的内置工具集：**实现全在插件里**，这里只传端口。
///
/// 调用方（组合根）拿到之后照旧注入脱敏器、应用禁用状态与门控。
pub fn build_registry(
    instances: std::sync::Arc<echo_defs::plugin_instance::InstanceRegistry>,
    workspace: std::path::PathBuf,
    config_store: echo_config::ConfigStore,
    invoker: Option<echo_plugin_tools::RemoteInvoker>,
    shell: echo_shell::ShellHandle,
    provider: Option<std::sync::Arc<dyn BuiltinToolProvider>>,
) -> crate::tool::ToolRegistry {
    let provider = provider.unwrap_or_else(echo_plugin_tools::plugin::standalone_provider);
    let cx = ToolsHostCtx {
        instances,
        workspace,
        invoker,
        shell,
    };
    let mut registry = provider.registry(&cx);
    // 唯一留在宿主的工具：数据源是宿主的供应商池读取助手（S1b 随类型下沉）。
    registry.register(std::sync::Arc::new(api_profiles::ListApiProfilesTool::new(
        config_store,
    )));
    registry.set_package(
        "list_api_profiles",
        crate::plugins::TOOLS_BUILTIN_PLUGIN_ID,
    );
    registry
}

#[cfg(test)]
mod tests {
    use super::build_registry;

    /// 内置工具集内部不能有重名：`ToolRegistry::register` 是覆盖语义，
    /// 撞名会让一个工具静默消失（工具定义还在，执行却落到另一个实现上）。
    /// 这条不变量此前靠人肉检查——踩过一次（`calculator` 与插件示例同名）。
    #[test]
    fn builtin_tools_have_unique_names() {
        let dir = std::env::temp_dir().join(format!("echo-builtin-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = echo_config::ConfigStore::new(dir.join("core.toml"));
        let registry = build_registry(
            std::sync::Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
            dir.clone(),
            store,
            None,
            echo_shell::ShellHandle::new(),
            None,
        );
        let conflicts = registry.name_conflicts();
        assert!(
            conflicts.is_empty(),
            "内置工具重名: {conflicts:?}"
        );
    }
}
