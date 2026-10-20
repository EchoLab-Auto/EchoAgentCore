//! 内置工具集**插件**：工具清单的所有权在这里，宿主只注入端口。
//!
//! ## 为什么有这个模块（2026-11，全插件化 S1）
//!
//! 此前本 crate 是一堆 `register_*(registry)` 函数，真正的"有哪些工具、
//! 怎么装、包标签是什么、自停护栏长什么样"写在宿主
//! `echo-agent/src/packages/tools_builtin/mod.rs` 里。从**代码位置**看工具
//! 已经外迁，从**所有权**看没有：宿主仍然知道每一个工具的名字与装配细节，
//! 插件只是被调用的一批库函数。
//!
//! 现在反过来：本模块实现 [`BuiltinToolProvider`]——"给一组端口，产出一个
//! 配好的 [`ToolRegistry`]"是插件的职责；宿主
//! （`packages/tools_builtin` → 组合根）只负责**提供端口**（实例注册表 /
//! 工作区根 / 联邦出口 / Shell 句柄）与注入脱敏器。宿主侧不再出现任何
//! 内置工具的名字。
//!
//! ## 服务键
//!
//! `mount` 时把 provider `provide` 到 [`SERVICE_KEY`]，于是"内置工具集"在
//! 服务表里可寻址（可被包装 / 替换 / 观察），而不再只是一个 Cargo 依赖。
//! 依赖声明用 `required_services`：本插件不依赖任何别的服务，因此不声明。
//!
//! ## 边界
//!
//! provider 返回的是**裸工具箱**（无脱敏器、无 persona 门控、无禁用状态）：
//! 那三件事是宿主的策略（`[agent].disabled_tools`、persona 白名单、
//! `redactor` 注入），插件不该知道。装配顺序见组合根。

use std::path::PathBuf;
use std::sync::Arc;

use echo_plugin::{BuiltinPlugin, MountContext, Plugin, PluginManifest};
use echo_tool::ToolRegistry;

/// 插件 id（宿主 `plugins::TOOLS_BUILTIN_PLUGIN_ID` 与本常量同源字面量）。
pub const PLUGIN_ID: &str = "echo-agent.tools.builtin";

/// 服务键：内置工具集的构造器（`Arc<dyn BuiltinToolProvider>`）。
pub const SERVICE_KEY: &str = "tools.builtin";

/// 宿主要塞给工具箱的**端口**（都是"运行时才知道"的东西）。
///
/// 每项都对应工具箱里一个真实接缝（见 lib.rs 的「接缝」章节）——多一项都
/// 意味着插件多知道一件宿主内部结构，所以这里刻意只有四个。
pub struct ToolsHostCtx {
    /// 插件实例注册表（`instance_*` 工具按它枚举/寻址实例）。
    pub instances: Arc<echo_defs::plugin_instance::InstanceRegistry>,
    /// 工作区根：编码类工具的文件沙箱边界。
    pub workspace: PathBuf,
    /// 联邦远程调用出口（`node://` 前缀的分流目标；None = 联邦未接线）。
    pub invoker: Option<crate::RemoteInvoker>,
    /// Shell 会话管理器（`shell_*` 工具）。
    pub shell: echo_shell::ShellHandle,
}

/// 内置工具集的构造契约：端口进，配好的工具箱出。
pub trait BuiltinToolProvider: Send + Sync {
    fn registry(&self, cx: &ToolsHostCtx) -> ToolRegistry;
}

/// 唯一实现（保留成类型而不是闭包：`ctx.provide` 需要 `'static + Any`，
/// 而且将来要按 persona 分支时这是自然的扩展点）。
#[derive(Default)]
pub struct BuiltinTools;

impl BuiltinToolProvider for BuiltinTools {
    fn registry(&self, cx: &ToolsHostCtx) -> ToolRegistry {
        let guard = core_self_stop_guard();
        let mut registry = ToolRegistry::new();
        crate::register_core_tools(&mut registry);
        crate::register_instance_tools(&mut registry, cx.instances.clone());
        crate::register_coding_tools(
            &mut registry,
            cx.workspace.clone(),
            Some(guard.clone()),
            cx.invoker.clone(),
        );
        crate::register_shell_tools(&mut registry, Some(guard), cx.shell.clone());
        // 每个工具打上本包标签：`echo-agent.tools.builtin` 的启停按包批量生效
        // （`ToolRegistry::set_package_enabled`）。打标签的知识归插件——
        // 它知道"这些是我装的"。
        for name in registry.names() {
            registry.set_package(&name, PLUGIN_ID);
        }
        registry
    }
}

/// 拒绝会从 Core 自己的进程树里停掉 / 杀掉 Core 服务的命令。
///
/// `systemctl stop` 之后没有 `start`：systemd 会终止整个服务 cgroup，包含
/// 正在跑这条命令的 shell，于是 `stop && cp ... && start` 的后半段永远不执行。
/// `systemctl restart` 是安全的——重启作业由 systemd 持有，即使调用方被终止。
///
/// 宽泛的 `pkill -f` / `killall` 同样拒绝提到 `core.toml` 或
/// `echo-agent-core` 的模式：即使本意是打某个冒烟测试进程，`pkill -f core.toml`
/// 也会匹配到生产 Core 的命令行（`--config .../core.toml`）。
///
/// 2026-10 技术债清理：此前用 `echo_bootstrap::get_or_init` 存那份编译好的
/// `Regex`——但正则编译产物是**纯计算常量**，不是宿主状态。改为「构造护栏时
/// 编译一次、move 进闭包」：零全局态，护栏的唯一实例由调用方持有。
pub fn core_self_stop_guard() -> crate::CommandGuard {
    const PATTERN: &str = r#"(?im)(?:^|[;&|]\s*|(?:ba)?sh\s+-[A-Za-z]*c\b\s*['\"]?\s*)\s*(?:sudo\s+)?(?:command\s+)?(?:(?:/usr/bin/)?systemctl\b[^\n;&|]*\b(?:stop|kill)\b[^\n;&|]*\becho-agent-core(?:\.service)?\b|(?:pkill|killall)\b[^\n;&|]*(?:(?:\[e\]|e)cho-agent-core|(?:\[c\]|c)ore\.toml))"#;
    let regex = regex::Regex::new(PATTERN).expect("self-stop regex is valid");
    std::sync::Arc::new(move |command: &str| {
        if regex.is_match(command) {
            return Err(echo_tool::ToolError::Execution(
                "blocked command: stopping or killing echo-agent-core from inside its own process would terminate this tool before later cleanup/start steps run. Use `systemctl --user restart echo-agent-core.service` or the managed updater `scripts/update.sh` instead.".into(),
            ));
        }
        Ok(())
    })
}

/// 插件清单（id / 名称 / 类别 / 路由键与宿主既有字面量一致）。
pub fn manifest(version: &str) -> PluginManifest {
    PluginManifest::builtin(
        PLUGIN_ID,
        "内置工具集",
        version,
        "tool",
        "builtin_tools",
        "平台无关的内置工具（计算/搜索/清单/编码/插件实例管理）",
    )
}

/// 构造插件：`mount` 把 provider 挂到 [`SERVICE_KEY`]，并把门控开关拨到位。
///
/// `gate` 是**宿主的配置概念**（persona 白名单 / 全局启停），插件只表达
/// "我挂上了 / 我卸下了"——这也是别的内置插件（workspace / skills）同一套
/// 约定，宿主因此只有一处接线。
pub fn plugin(
    version: &str,
    shell_host: Arc<dyn crate::shell_commands::ShellCommandHost>,
    gate: impl Fn(bool) + Send + Sync + 'static,
) -> Arc<dyn Plugin> {
    let gate = Arc::new(gate);
    let gate_mount = gate.clone();
    let handler = Arc::new(crate::shell_commands::ShellCommandHandler::new(shell_host));
    Arc::new(BuiltinPlugin::new(manifest(version), move |mctx: &MountContext| {
        gate_mount(true);
        let gate_unmount = gate_mount.clone();
        let mut disposers = vec![echo_context::Disposer::from_fn(move || gate_unmount(false))];
        // 服务表可能不存在（裸注册表 / 单测）：此时只做门控，不提供键——
        // 与 loop 插件同样的降级口径（缺服务表不是错误）。
        if let Some(ctx) = &mctx.ctx {
            let provider: Arc<dyn BuiltinToolProvider> = Arc::new(BuiltinTools);
            let keep = ctx
                .provide(&service_key(), provider)
                .map_err(|e| echo_plugin::PluginMountError::Mount(e.to_string()))?;
            disposers.push(keep);
        }
        Ok(disposers)
    })
    // 命令域：4 条 `*Shell*` 命令由本插件处理（宿主 `apply_command` 在落
    // 内置 match 之前问一次插件）。`shell_*` 工具也在这里——会话的模型侧与
    // 面板侧从此同属一个插件。
    .with_commands(crate::shell_commands::COMMANDS)
    .with_command_handler_async(move |name, payload| {
        let handler = handler.clone();
        Box::pin(async move {
            handler
                .handle(&name, payload)
                .await
                .unwrap_or(Ok(serde_json::Value::Null))
        })
    }))
}

/// `tools.builtin` 的强类型键（与 [`SERVICE_KEY`] 同源）。
pub fn service_key() -> echo_context::ServiceKey<Arc<dyn BuiltinToolProvider>> {
    echo_context::ServiceKey::new(SERVICE_KEY)
}

/// 便捷解析：宿主在工具装配点问一句"内置工具集在不在"。
///
/// 返回 `None` 表示插件未挂载（禁用了 / 还没挂上）——调用方据此降级
/// （宿主当前的口径是**直接构造**同一实现，见组合根注释）。
pub fn resolve_provider(
    ctx: &echo_context::Ctx,
) -> Option<Arc<dyn BuiltinToolProvider>> {
    ctx.service(&service_key())
}

/// 不依赖服务表直接构造（组合根的"插件尚未挂载"路径与单测用）。
pub fn standalone_provider() -> Arc<dyn BuiltinToolProvider> {
    Arc::new(BuiltinTools)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 命令域测试不关心 shell：最小宿主（未接线）。
    struct NoShellHost;

    impl crate::shell_commands::ShellCommandHost for NoShellHost {
        fn manager(&self) -> Option<Arc<echo_shell::ShellManager>> {
            None
        }
        fn emit(&self, _event: echo_protocol::BackendEvent) {}
        fn team_id(&self) -> Option<String> {
            None
        }
        fn progress(&self) -> echo_shell::ShellEmit {
            Arc::new(|_event| {})
        }
    }

    fn ports() -> ToolsHostCtx {
        ToolsHostCtx {
            instances: Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
            workspace: std::env::temp_dir(),
            invoker: None,
            shell: echo_shell::ShellHandle::new(),
        }
    }

    /// provider 产出的工具箱必须带上本包标签：包级启停靠它。
    #[test]
    fn provider_tags_every_tool_with_the_package_id() {
        let registry = BuiltinTools.registry(&ports());
        assert!(!registry.is_empty(), "内置工具集不该是空的");
        for name in registry.names() {
            assert_eq!(
                registry.package_of(&name).as_deref(),
                Some(PLUGIN_ID),
                "工具 {name} 缺包标签"
            );
        }
    }

    /// 自停护栏仍是工具箱的一部分（搬模块时最容易漏的一条）：
    /// `systemctl stop <core>` 被拒，`restart` 放行。
    #[test]
    fn self_stop_guard_rejects_stop_and_allows_restart() {
        let guard = core_self_stop_guard();
        assert!(
            guard("systemctl stop echo-agent-core").is_err(),
            "stop 必须被拒（会杀死调用方自己）"
        );
        assert!(
            guard("pkill -f core.toml").is_err(),
            "宽泛 pkill 必须被拒（会误伤生产 Core）"
        );
        assert!(
            guard("systemctl --user restart echo-agent-core.service").is_ok(),
            "restart 必须放行（systemd 持有重启作业）"
        );
        // 工具箱里仍有编码类工具（护栏的挂载点）。
        let registry = BuiltinTools.registry(&ports());
        assert!(
            registry.contains_tool("bash"),
            "编码类工具未装：{}",
            registry.names().join(", ")
        );
    }

    /// 挂到服务表之后可被解析；卸载后键消失（可逆）。
    #[test]
    fn mount_provides_and_unmount_revokes_the_service() {
        let ctx = Arc::new(echo_context::Ctx::default());
        let host: Arc<dyn crate::shell_commands::ShellCommandHost> =
            Arc::new(NoShellHost);
        let plugin = plugin("9.9.9", host, |_| {});
        let mounted = plugin
            .mount(&MountContext::new().with_ctx(ctx.clone()))
            .expect("mount ok");
        assert!(
            resolve_provider(&ctx).is_some(),
            "挂载后应能解析到 provider"
        );
        for d in mounted {
            d.dispose();
        }
        assert!(
            resolve_provider(&ctx).is_none(),
            "卸载后服务键必须消失"
        );
    }
}
