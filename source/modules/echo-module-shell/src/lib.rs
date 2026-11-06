//! Shell 会话**模块**：工具清单与命令域的所有权在这里，宿主只注入端口。
//!
//! ## 为什么它是独立模块（2026-11）
//!
//! 此前 `bash-start` / `bash-exec` / `bash-stop` 与四条 `*Shell*` 命令住在
//! `echo-module-tools`（6,000 行的"内置工具集"）里。它们与那套工具的**唯一**
//! 共同点是"都是模型能调用的工具"，而这一点几乎不构成理由：
//!
//! | 维度 | 内置工具集 | Shell 会话 |
//! |---|---|---|
//! | 机制依赖 | 纯函数 + 可选 `echo-module-shell` | **必须** `echo-module-shell`（进程组生命周期） |
//! | 状态 | 无（除 checklist 的会话内状态） | 长驻子进程 + 哨兵分帧 |
//! | 面板 | 工具列表里几行 | 独立的 Shell 页面（会话卡 / 实时流） |
//! | 关闭需求 | "我不想要 shell" → 不想连计算器一起关 | 独立启停 |
//!
//! 于是它成为自己的模块：`echo-agent.shell`。想关 shell 只关它；想换实现
//! （例如换成容器内执行）只换它。
//!
//! ## 服务键与端口
//!
//! `mount` 时把 [`ShellToolProvider`] provide 到 [`SERVICE_KEY`]，组合根在
//! 建每个 agent 的工具表时解析它（与 `tools.builtin` 同款：**模块的工具由
//! 模块自己造**，宿主只给端口——这里是 [`ShellHostCtx`] 的两个字段）。
//!
//! 命令域经 [`host_keys::SHELL_HOST`] 解析宿主适配器（会话管理器 / 事件出口 /
//! 归属人格 / 进度回调）。
//!
//! ## persona 门控
//!
//! **不在这里**：挂载/卸载是一件事，"这个使用者看不看得到它"是另一件事。
//! 宿主监听 `module.mounted` / `module.unmounted` 统一拨（见组合根）。

use std::sync::Arc;

pub use crate::session::{CommandGuard, ShellHandle};

use echo_module::{BuiltinModule, Inject, Module, ModuleManifest, MountContext, ServiceSlot};
use echo_module_tools::ToolRegistry;
#[allow(ambiguous_glob_reexports)]
pub use session::*;

pub mod commands;
pub mod host_keys;
pub mod session;
pub mod tools;

/// 模块 id（`modules.toml` / Panel 模块清单同一口径）。
pub const MODULE_ID: &str = "echo-agent.shell";

/// 服务键：Shell 工具集的构造器（`Arc<dyn ShellToolProvider>`）。
pub const SERVICE_KEY: &str = "shell";

/// 宿主要塞给工具箱的**端口**（都是"运行时才知道"的东西）。
pub struct ShellHostCtx {
    /// Shell 会话管理器句柄（`echo-module-shell`）。
    pub shell: ShellHandle,
    /// 命令护栏（`None` = 不做该检查）。
    pub guard: Option<CommandGuard>,
}

/// Shell 工具集的构造契约：端口进，配好的工具箱出。
///
/// 与 `echo-module-tools::BuiltinToolProvider` 同款：**工具由模块造**，
/// 宿主只传端口。返回的注册表已按本项目打上包标签（`echo-agent.shell`），
/// 于是面板按包启停与 persona 门控都作用于正确的模块。
pub trait ShellToolProvider: Send + Sync {
    fn registry(&self, cx: &ShellHostCtx) -> ToolRegistry;
}

/// 唯一实现（保留成类型而不是闭包：`ctx.provide` 需要 `'static + Any`）。
#[derive(Default)]
pub struct ShellTools;

impl ShellToolProvider for ShellTools {
    fn registry(&self, cx: &ShellHostCtx) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        tools::register_shell_tools(&mut registry, cx.guard.clone(), cx.shell.clone());
        // 每个工具打上本包标签：面板与 persona 门控都按包批量生效。
        // 打标签的知识归模块——它知道"这些是我装的"。
        for name in registry.names() {
            registry.set_package(&name, MODULE_ID);
        }
        registry
    }
}

/// 模块清单（id / 名称 / 类别 / 路由键与宿主既有字面量一致）。
pub fn manifest(version: &str) -> ModuleManifest {
    ModuleManifest::builtin(
        MODULE_ID,
        "Shell 会话",
        version,
        "tool",
        "shell",
        "后台 bash 会话（bash-start / bash-exec / bash-stop）+ Shell 面板命令域",
    )
}

/// 构造模块：`mount` 把 provider 挂到 [`SERVICE_KEY`]，并绑定命令域接缝。
///
/// **零参数**（全模块化 S6 的纪律）：需要什么由 `Inject` 声明、`mount` 里
/// 按同一批键解析。宿主因此只需注册能力，不必知道"这个模块要什么"。
pub fn module(version: &str) -> Arc<dyn Module> {
    let host = ServiceSlot::<Arc<dyn commands::ShellCommandHost>>::new();
    let bind = host.clone();
    let serve = host.clone();

    Arc::new(
        BuiltinModule::new(manifest(version), move |mctx: &MountContext| {
            let mut disposers = Vec::new();
            // 服务表可能不存在（裸注册表 / 单测）：此时只做门控式挂载，
            // 不 provide 键——与 loop / tools 模块同样的降级口径。
            if let Some(ctx) = mctx.context() {
                let provider: Arc<dyn ShellToolProvider> = Arc::new(ShellTools);
                disposers.push(
                    ctx.provide(&service_key(), provider)
                        .map_err(|e| echo_module::ModuleMountError::Mount(e.to_string()))?,
                );
            }
            disposers.push(bind.bind(mctx, &host_keys::SHELL_HOST)?);
            Ok(disposers)
        })
        .with_inject(Inject::new([
            echo_module_tools::SERVICE_KEY.to_string(),
            host_keys::SHELL_HOST.name().to_string(),
        ]))
        .with_commands(commands::COMMANDS)
        .with_command_handler_async(move |name, payload| {
            let serve = serve.clone();
            Box::pin(async move {
                let Some(host) = serve.get() else {
                    return Err(format!(
                        "命令 {name} 到 {MODULE_ID} 时模块尚未挂载（宿主接缝未绑定）"
                    ));
                };
                commands::ShellCommandHandler::new(host)
                    .handle(&name, payload)
                    .await
                    .unwrap_or_else(|| {
                        Err(format!(
                            "命令 {name} 被 {MODULE_ID} 声明，但 shell 处理器没有认领它"
                        ))
                    })
            })
        }),
    )
}

/// `SERVICE_KEY` 的强类型键（与 [`SERVICE_KEY`] 同源）。
pub fn service_key() -> echo_context::ServiceKey<Arc<dyn ShellToolProvider>> {
    echo_context::ServiceKey::new(SERVICE_KEY)
}

/// 从服务表解析 provider；模块未挂载 / 未接线时 `None`。
///
/// 组合根用它构造每个 agent 的 shell 工具——未挂载 = 没有 `bash-*` 工具
/// （而不是"有工具但执行时报错"）。
pub fn resolve_provider(ctx: &Arc<echo_context::Ctx>) -> Option<Arc<dyn ShellToolProvider>> {
    ctx.service(&service_key())
}

/// 不依赖服务表的实现（单测 / 嵌入式装配 / 文档生成用）。
pub fn standalone_provider() -> Arc<dyn ShellToolProvider> {
    Arc::new(ShellTools)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模块声明要处理的命令与处理器认领的**必须是同一批**。
    ///
    /// 不守这条的后果很具体：声明了但处理器不认领 → 面板点了没反应，
    /// 且宿主不会回落到内置分支（它以为模块接住了）。
    #[test]
    fn declared_commands_are_all_handled() {
        let declared: Vec<String> = commands::COMMANDS.iter().map(|c| c.to_string()).collect();
        assert_eq!(declared.len(), 4, "shell 命令域是 4 条：{declared:?}");
        for name in &declared {
            assert!(
                matches!(
                    name.as_str(),
                    "RequestShellSessions" | "ShellStart" | "ShellExec" | "ShellStop"
                ),
                "意外的命令名 {name}"
            );
        }
    }

    /// 工具集：三个工具都在，且**都打上了本模块的包标签**。
    ///
    /// 包标签决定 persona 门控与面板按包启停——标错会让"禁用 shell 模块"
    /// 之后工具仍然可见可调（2026-10 那类静默失效的形状）。
    #[tokio::test]
    async fn tools_are_registered_and_tagged_with_this_module() {
        let provider = standalone_provider();
        let registry = provider.registry(&ShellHostCtx {
            shell: ShellHandle::new(),
            guard: None,
        });
        let names = registry.names();
        for expected in ["bash-start", "bash-exec", "bash-stop"] {
            assert!(
                names.iter().any(|n| n == expected),
                "缺少工具 {expected}（实际: {names:?}）"
            );
        }
        for name in &names {
            assert_eq!(
                registry.package_of(name).as_deref(),
                Some(MODULE_ID),
                "{name} 的包标签必须是 {MODULE_ID}（门控与面板按包生效）"
            );
        }
    }

    /// 旧名 `shell_exec` 不得再出现——它是被用户点名改掉的。
    ///
    /// 这条守的是"改名真的生效"，而不是"加了个别名"：留着旧名会让模型
    /// 同时看到两个等价工具，选择退化成掷骰子。
    #[tokio::test]
    async fn legacy_shell_exec_name_is_gone() {
        let registry = standalone_provider().registry(&ShellHostCtx {
            shell: ShellHandle::new(),
            guard: None,
        });
        assert!(
            !registry.names().iter().any(|n| n == "shell_exec"),
            "旧名 shell_exec 仍然存在：{:?}",
            registry.names()
        );
    }
}
