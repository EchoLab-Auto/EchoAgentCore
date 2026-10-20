//! # echo-plugin-self-update — 自更新插件
//!
//! 两条命令（`RequestSelfUpdate` / `RequestSelfUpdateStatus`）的归属插件，
//! 连同它们的全部知识：updater 单元名、状态文件布局、重入判定。
//!
//! ## 为什么它不再是"宿主自我管理面"
//!
//! 此前这两条命令在 `HOST_RESERVED_COMMANDS` 里，注释是"插件不得接管的自我
//! 管理面"。结果是**命令泵跳过插件分发**，而实现只在"插件分发 → 联邦插件的
//! 命令处理器"这条路上——两条命令必失败（文案还误报"联邦插件未启用"）。
//!
//! 保留的真正理由是"别让第三方插件劫持自更新"。这个理由用**内置插件**满足
//! 更准确：它是编译进本二进制、启动期即挂载、名字写死在
//! [`PLUGIN_ID`]；而"谁能认领命令"本来就不该由一张 8 条命令的名单守卫
//! （`SendMessage` 也没被守卫）。
//!
//! ## 形态
//!
//! ```text
//!  RequestSelfUpdate        → start_update()  → systemctl --user start <unit>
//!  RequestSelfUpdateStatus  → read_status()   → ~/.local/state/echo-agent-core/update-status
//! ```
//!
//! 两者都只经 [`SelfUpdateHost::emit`] 说话，不碰任何宿主类型。

#![warn(missing_docs)]

use std::sync::Arc;

/// 自更新命令域（`RequestSelfUpdate` / `RequestSelfUpdateStatus`）。
pub mod host_keys;
pub mod commands;

/// 插件 id（`plugins.toml` / Panel 插件清单同一口径）。
pub const PLUGIN_ID: &str = "echo-agent.selfupdate";

/// 构造插件：认领两条自更新命令。
///
/// **零参数**（全插件化 S6）：唯一需要的宿主能力是事件出口，经服务表解析
/// （[`host_keys::EVENTS`]）。updater 单元名与状态文件布局都在本 crate 里
/// ——组合根不认识它们。
pub fn plugin(version: &str) -> Arc<dyn echo_plugin::Plugin> {
    use echo_plugin::{BuiltinPlugin, Inject, PluginManifest, ServiceSlot};

    let events = ServiceSlot::<Arc<dyn Fn(echo_protocol::BackendEvent) + Send + Sync>>::new();
    let bind = events.clone();
    let serve = events.clone();
    let manifest = PluginManifest::builtin(
        PLUGIN_ID,
        "自更新",
        version,
        // kind = `orchestration`：进程级基础设施能力。**刻意不用
        // `management`**——Panel 会把 management 类插件放进**人格级**"启用
        // 插件"勾选区，而自更新按人格启停是个假开关（挂载是进程级的，命令
        // 分发不看人格白名单）。
        "orchestration",
        "selfupdate",
        "自更新：触发 updater 单元（oneshot，脱离本进程构建→替换→重启）+ 读状态文件",
    );
    Arc::new(
        BuiltinPlugin::new(manifest, move |mctx| {
            let keep = bind.bind(mctx, &host_keys::EVENTS)?;
            Ok(vec![keep])
        })
        .with_inject(Inject::new([host_keys::EVENTS.name().to_string()]))
        .with_commands(commands::COMMANDS)
        .with_command_handler_async(move |name, payload| {
            let serve = serve.clone();
            Box::pin(async move {
                let Some(sink) = serve.get() else {
                    return Err(format!(
                        "命令 {name} 到 {PLUGIN_ID} 时插件尚未挂载（事件出口未绑定）"
                    ));
                };
                commands::SelfUpdateHandler::new(Arc::new(commands::SinkHost::new(sink)))
                    .handle(&name, payload)
                    .await
                    .unwrap_or_else(|| {
                        Err(format!("命令 {name} 被 {PLUGIN_ID} 声明，但 self-update 处理器没有认领它"))
                    })
            })
        }),
    )
}
