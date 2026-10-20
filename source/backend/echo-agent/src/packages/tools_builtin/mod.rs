//! 内置工具集：宿主侧只剩**端口接线**（全插件化 S1/S3）。
//!
//! 2026-11 之前，这个文件是工具清单的**所有者**：它知道有哪些工具、
//! 自停护栏的长相、包标签怎么打。现在这些知识全在
//! [`echo_plugin_tools::plugin`]（`BuiltinToolProvider` 的实现）里——宿主
//! 只做两件它才该做的事：
//!
//! 1. **提供端口**（实例注册表 / 工作区根 / 联邦出口 / Shell 句柄 / 配置存储）；
//! 2. 把工具箱交给 `Agent`（脱敏器 / 禁用状态 / persona 门控都是宿主策略）。
//!
//! `list_api_profiles` 也在 S3 跟着搬走了（它的数据源 `api_pool` 与
//! `ApiProfile` 都下沉了）——宿主侧**不再认识任何一个内置工具的名字**。

// 工具实现已外迁到 `echo-plugin-tools`；这里保留原模块路径作为宿主兼容面。
pub use echo_plugin_tools::{
    coding, plugin as builtin_plugin, BuiltinToolProvider, BuiltinTools, ListApiProfilesTool,
    ToolsHostCtx,
};
/// 服务表解析（`tools.builtin`）：插件挂上了就有，没挂上返回 `None`。
pub use echo_plugin_tools::plugin::{resolve_provider, standalone_provider};
/// 工具管理面命令域的宿主接缝。
pub use echo_plugin_tools::tool_commands::{
    ToolsCommandHost as ToolsCommandHostAlias, ToolsCommandHandler,
};
/// 实例命令域的宿主接缝。
pub use echo_plugin_tools::instance_commands::{
    InstanceCommandHost as InstanceCommandHostAlias, InstanceCommandHandler,
};
/// Shell 命令域的宿主接缝（组合根实现、插件消费）。
pub use echo_plugin_tools::shell_commands::{
    ShellCommandHost as ShellCommandHostAlias, ShellCommandHandler,
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
        // 共享配置存储：`list_api_profiles` 读供应商池的唯一入口。
        config_store: Some(config_store),
    };
    provider.registry(&cx)
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
            std::sync::Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
            dir.clone(),
            store,
            None,
            echo_shell::ShellHandle::new(),
            None,
        );
        assert!(
            registry.contains_tool("list_api_profiles"),
            "缺 list_api_profiles：{:?}",
            registry.names()
        );
    }
}
